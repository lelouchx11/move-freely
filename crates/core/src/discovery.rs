//! Local network peer discovery over mDNS/DNS-SD (`_move-freely._tcp.local.`).

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use mf_protocol::{ArchKind, DeviceId, OsKind, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

pub const MDNS_SERVICE_TYPE: &str = "_move-freely._tcp.local.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredPeer {
    pub device_id: DeviceId,
    pub name: String,
    pub os: OsKind,
    pub arch: ArchKind,
    pub port: u16,
    pub addrs: Vec<SocketAddr>,
    pub pairing_open: bool,
    pub protocol_version: u32,
}

#[derive(Clone)]
pub struct DiscoveryService {
    daemon: Option<ServiceDaemon>,
    local_id: DeviceId,
    instance_fullname: Arc<Mutex<Option<String>>>,
    peers: Arc<Mutex<HashMap<DeviceId, DiscoveredPeer>>>,
    fullname_to_id: Arc<Mutex<HashMap<String, DeviceId>>>,
    updates_tx: broadcast::Sender<Vec<DiscoveredPeer>>,
}

impl DiscoveryService {
    /// Start mDNS registration and browsing. If mDNS cannot bind on this host
    /// (for instance in a restricted sandbox), falls back cleanly with an empty
    /// discovery list so manual IP connections continue working.
    pub fn start(
        local_id: DeviceId,
        device_name: &str,
        port: u16,
        pairing_open: bool,
    ) -> Result<Self> {
        let (updates_tx, _) = broadcast::channel(32);
        let peers = Arc::new(Mutex::new(HashMap::new()));
        let fullname_to_id = Arc::new(Mutex::new(HashMap::new()));
        let instance_fullname = Arc::new(Mutex::new(None));

        let daemon = match ServiceDaemon::new() {
            Ok(d) => Some(d),
            Err(e) => {
                tracing::warn!(error = %e, "mDNS daemon unavailable; falling back to manual IP");
                None
            }
        };

        let svc = Self {
            daemon,
            local_id,
            instance_fullname,
            peers,
            fullname_to_id,
            updates_tx,
        };

        if svc.daemon.is_some() {
            svc.advertise(device_name, port, pairing_open)?;
            svc.spawn_browser()?;
        }

        Ok(svc)
    }

    pub fn advertise(&self, device_name: &str, port: u16, pairing_open: bool) -> Result<()> {
        let Some(daemon) = &self.daemon else {
            return Ok(());
        };
        let instance = format!("mf-{}", self.local_id.short());
        let host_label = sanitize_mdns_host(device_name);
        let host = format!("{host_label}-{}.local.", self.local_id.short());
        let os = format!("{:?}", OsKind::current()).to_lowercase();
        let arch = format!("{:?}", ArchKind::current()).to_lowercase();
        let ver = PROTOCOL_VERSION.to_string();
        let pairing = if pairing_open { "1" } else { "0" };

        let props = [
            ("id", self.local_id.as_str()),
            ("name", device_name),
            ("os", os.as_str()),
            ("arch", arch.as_str()),
            ("ver", ver.as_str()),
            ("pairing", pairing),
        ];

        let info = ServiceInfo::new(
            MDNS_SERVICE_TYPE,
            &instance,
            &host,
            "",
            port,
            &props[..],
        )?
        .enable_addr_auto();

        let fullname = info.get_fullname().to_string();
        daemon.register(info)?;
        *self.instance_fullname.lock().unwrap() = Some(fullname);
        Ok(())
    }

    pub fn list_peers(&self) -> Vec<DiscoveredPeer> {
        let guard = self.peers.lock().unwrap();
        let mut out: Vec<_> = guard.values().cloned().collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Vec<DiscoveredPeer>> {
        self.updates_tx.subscribe()
    }

    pub fn shutdown(&self) {
        if let Some(daemon) = &self.daemon {
            if let Some(fullname) = self.instance_fullname.lock().unwrap().take() {
                let _ = daemon.unregister(&fullname);
            }
            let _ = daemon.shutdown();
        }
    }

    fn spawn_browser(&self) -> Result<()> {
        let Some(daemon) = &self.daemon else {
            return Ok(());
        };
        let rx = daemon.browse(MDNS_SERVICE_TYPE)?;
        let local_id = self.local_id.clone();
        let peers = self.peers.clone();
        let fullname_to_id = self.fullname_to_id.clone();
        let updates_tx = self.updates_tx.clone();

        tokio::spawn(async move {
            while let Ok(event) = rx.recv_async().await {
                match event {
                    ServiceEvent::ServiceResolved(resolved) => {
                        let props = resolved.get_properties();
                        let Some(id_val) = props.get_property_val_str("id") else {
                            continue;
                        };
                        let dev_id = DeviceId::from(id_val);
                        if dev_id == local_id {
                            continue;
                        }
                        let name = props
                            .get_property_val_str("name")
                            .unwrap_or("unknown")
                            .to_string();
                        let os = parse_os(props.get_property_val_str("os").unwrap_or("other"));
                        let arch =
                            parse_arch(props.get_property_val_str("arch").unwrap_or("other"));
                        let pairing_open =
                            props.get_property_val_str("pairing") == Some("1");
                        let protocol_version = props
                            .get_property_val_str("ver")
                            .and_then(|v| v.parse().ok())
                            .unwrap_or(PROTOCOL_VERSION);

                        let port = resolved.get_port();
                        let mut addrs: Vec<SocketAddr> = resolved
                            .get_addresses()
                            .iter()
                            .map(|ip| SocketAddr::new(ip.to_ip_addr(), port))
                            .collect();
                        // Prefer IPv4 addresses first for predictable local LAN dialing.
                        addrs.sort_by_key(|a| match a.ip() {
                            IpAddr::V4(_) => 0,
                            IpAddr::V6(_) => 1,
                        });

                        let fullname = resolved.get_fullname().to_string();
                        fullname_to_id
                            .lock()
                            .unwrap()
                            .insert(fullname, dev_id.clone());
                        peers.lock().unwrap().insert(
                            dev_id.clone(),
                            DiscoveredPeer {
                                device_id: dev_id,
                                name,
                                os,
                                arch,
                                port,
                                addrs,
                                pairing_open,
                                protocol_version,
                            },
                        );
                        let snapshot: Vec<_> = peers.lock().unwrap().values().cloned().collect();
                        let _ = updates_tx.send(snapshot);
                    }
                    ServiceEvent::ServiceRemoved(_ty, fullname) => {
                        if let Some(id) = fullname_to_id.lock().unwrap().remove(&fullname) {
                            peers.lock().unwrap().remove(&id);
                            let snapshot: Vec<_> =
                                peers.lock().unwrap().values().cloned().collect();
                            let _ = updates_tx.send(snapshot);
                        }
                    }
                    _ => {}
                }
            }
        });
        Ok(())
    }
}

fn sanitize_mdns_host(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() {
        "move-freely".into()
    } else {
        trimmed.chars().take(32).collect()
    }
}

fn parse_os(s: &str) -> OsKind {
    match s {
        "macos" => OsKind::MacOs,
        "windows" => OsKind::Windows,
        "linux" => OsKind::Linux,
        _ => OsKind::Other,
    }
}

fn parse_arch(s: &str) -> ArchKind {
    match s {
        "aarch64" => ArchKind::Aarch64,
        "x86_64" => ArchKind::X86_64,
        _ => ArchKind::Other,
    }
}

/// Enumerate non-loopback local interface IP addresses for UI diagnostics.
pub fn local_ip_addresses() -> Vec<IpAddr> {
    let mut ips = Vec::new();
    // Binding a UDP socket to a multicast/RFC1918 address reveals the primary outbound NIC IP
    // without sending any packets over the network.
    for probe in ["192.168.0.1:80", "10.0.0.1:80", "172.16.0.1:80"] {
        if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
            if sock.connect(probe).is_ok() {
                if let Ok(local) = sock.local_addr() {
                    let ip = local.ip();
                    if !ip.is_unspecified() && !ip.is_loopback() && !ips.contains(&ip) {
                        ips.push(ip);
                    }
                }
            }
        }
    }
    ips
}
