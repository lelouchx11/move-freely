//! Encrypted peer-to-peer TLS transport, pairing handshake, dual Control/Data
//! channels, priority queuing, RTT heartbeat, and automatic reconnection.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use mf_protocol::{
    read_wire_frame, write_frame, write_wire_frame, ArchKind, BinaryChunk, ChannelRole, DeviceId,
    Message, Monitor, OsKind, PeerCapabilities, WireFrame, PROTOCOL_VERSION,
};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio_rustls::{TlsAcceptor, TlsConnector};

use crate::config::AppConfig;
use crate::security::{
    cert_fingerprint, compute_pair_proof, is_allowed_peer_ip, now_unix_secs, random_bytes,
    random_u64, verify_pair_proof, DeviceIdentity, PairingManager, TrustedPeer, TrustedPeerStore,
    TLS_EKM_LABEL,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);
const PAIR_CONSENT_TIMEOUT: Duration = Duration::from_secs(45);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(7);

/// Inbound event delivered from network peers up to the engine.
#[derive(Debug)]
pub enum NetworkEvent {
    PeerConnected(PeerInfo),
    PeerDisconnected {
        device_id: DeviceId,
        reason: String,
    },
    ControlMessage {
        from: DeviceId,
        msg: Message,
    },
    DataChunk {
        from: DeviceId,
        chunk: BinaryChunk,
    },
    IncomingPairRequest(IncomingPairPrompt),
    LatencyUpdated {
        device_id: DeviceId,
        latency_ms: u64,
    },
}

#[derive(Debug)]
pub struct IncomingPairPrompt {
    pub device_id: DeviceId,
    pub name: String,
    pub os: OsKind,
    pub addr: SocketAddr,
    pub pin: String,
    pub responder: oneshot::Sender<bool>,
}

#[derive(Debug, Clone)]
pub struct PeerInfo {
    pub device_id: DeviceId,
    pub name: String,
    pub os: OsKind,
    pub arch: ArchKind,
    pub addr: SocketAddr,
    pub monitors: Vec<Monitor>,
    pub capabilities: PeerCapabilities,
    pub latency_ms: u64,
    pub active_controller: Option<(u64, DeviceId)>,
}

struct PeerSession {
    info: PeerInfo,
    session_nonce: u64,
    high_tx: mpsc::UnboundedSender<Message>,
    normal_tx: mpsc::UnboundedSender<Message>,
    data_tx: Mutex<Option<mpsc::Sender<WireFrame>>>,
    latency_ms: Arc<AtomicU64>,
    cancel_tx: oneshot::Sender<()>,
}

#[derive(Clone)]
pub struct NetworkManager {
    pub identity: DeviceIdentity,
    pub trust_store: TrustedPeerStore,
    pub pairing: Arc<PairingManager>,
    local_state: Arc<Mutex<LocalAdvertiseState>>,
    sessions: Arc<Mutex<HashMap<DeviceId, PeerSession>>>,
    dialing: Arc<Mutex<HashMap<DeviceId, Instant>>>,
    event_tx: mpsc::UnboundedSender<NetworkEvent>,
    config: Arc<Mutex<AppConfig>>,
}

#[derive(Clone)]
pub struct LocalAdvertiseState {
    pub name: String,
    pub monitors: Vec<Monitor>,
    pub capabilities: PeerCapabilities,
    pub active_controller: Option<(u64, DeviceId)>,
}

impl NetworkManager {
    pub fn new(
        identity: DeviceIdentity,
        trust_store: TrustedPeerStore,
        pairing: Arc<PairingManager>,
        config: Arc<Mutex<AppConfig>>,
        initial_state: LocalAdvertiseState,
        event_tx: mpsc::UnboundedSender<NetworkEvent>,
    ) -> Self {
        Self {
            identity,
            trust_store,
            pairing,
            local_state: Arc::new(Mutex::new(initial_state)),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            dialing: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
            config,
        }
    }

    pub fn update_local_state(&self, f: impl FnOnce(&mut LocalAdvertiseState)) {
        let mut guard = self.local_state.lock().unwrap();
        f(&mut guard);
    }

    pub fn connected_peers(&self) -> Vec<PeerInfo> {
        let guard = self.sessions.lock().unwrap();
        let mut out: Vec<_> = guard
            .values()
            .map(|s| {
                let mut info = s.info.clone();
                info.latency_ms = s.latency_ms.load(Ordering::Relaxed);
                info
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn is_connected(&self, id: &DeviceId) -> bool {
        self.sessions.lock().unwrap().contains_key(id)
    }

    /// Send a high-priority input/ownership frame to a specific peer.
    pub fn send_high(&self, id: &DeviceId, msg: Message) -> bool {
        let guard = self.sessions.lock().unwrap();
        if let Some(s) = guard.get(id) {
            s.high_tx.send(msg).is_ok()
        } else {
            false
        }
    }

    /// Send a normal-priority control/clipboard-text frame to a specific peer.
    pub fn send_normal(&self, id: &DeviceId, msg: Message) -> bool {
        let guard = self.sessions.lock().unwrap();
        if let Some(s) = guard.get(id) {
            s.normal_tx.send(msg).is_ok()
        } else {
            false
        }
    }

    pub fn broadcast_high(&self, msg: &Message) {
        let guard = self.sessions.lock().unwrap();
        for s in guard.values() {
            let _ = s.high_tx.send(msg.clone());
        }
    }

    pub fn broadcast_normal(&self, msg: &Message) {
        let guard = self.sessions.lock().unwrap();
        for s in guard.values() {
            let _ = s.normal_tx.send(msg.clone());
        }
    }

    /// Stream a bulk binary chunk over the dedicated Data channel if connected,
    /// falling back to waiting briefly for the Data channel.
    pub async fn send_data_chunk(&self, id: &DeviceId, chunk: BinaryChunk) -> Result<()> {
        let tx = {
            let guard = self.sessions.lock().unwrap();
            guard
                .get(id)
                .and_then(|s| s.data_tx.lock().unwrap().clone())
        };
        if let Some(tx) = tx {
            tx.send(WireFrame::Chunk(chunk))
                .await
                .map_err(|_| anyhow!("data channel closed"))?;
            return Ok(());
        }
        // Fallback: if data channel isn't established yet, fail cleanly so caller can retry or skip.
        bail!("data channel not connected for peer {id}")
    }

    pub fn disconnect_peer(&self, id: &DeviceId, reason: &str) {
        if let Some(session) = self.sessions.lock().unwrap().remove(id) {
            let _ = session.high_tx.send(Message::Goodbye {
                reason: reason.to_string(),
            });
            let _ = session.cancel_tx.send(());
            let _ = self.event_tx.send(NetworkEvent::PeerDisconnected {
                device_id: id.clone(),
                reason: reason.to_string(),
            });
        }
    }

    pub fn disconnect_all(&self, reason: &str) {
        let ids: Vec<DeviceId> = self.sessions.lock().unwrap().keys().cloned().collect();
        for id in ids {
            self.disconnect_peer(&id, reason);
        }
    }

    /// Start the TLS listener loop on the configured `bind_ip:listen_port`.
    pub async fn start_listener(&self, mut shutdown_rx: tokio::sync::watch::Receiver<bool>) -> Result<SocketAddr> {
        let (bind_ip, port) = {
            let cfg = self.config.lock().unwrap();
            (cfg.bind_ip.clone(), cfg.listen_port)
        };
        let bind_addr = format!("{bind_ip}:{port}");
        let listener = TcpListener::bind(&bind_addr)
            .await
            .with_context(|| format!("binding TLS listener at {bind_addr}"))?;
        let local_addr = listener.local_addr()?;
        tracing::info!(%local_addr, device_id = %self.identity.device_id.short(), "TLS peer listener active");

        let server_cfg = self.identity.tls_server_config()?;
        let acceptor = TlsAcceptor::from(server_cfg);
        let mgr = self.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_rx.changed() => {
                        if *shutdown_rx.borrow() {
                            break;
                        }
                    }
                    accepted = listener.accept() => {
                        match accepted {
                            Ok((tcp, peer_addr)) => {
                                let mgr = mgr.clone();
                                let acceptor = acceptor.clone();
                                tokio::spawn(async move {
                                    if let Err(e) = mgr.handle_inbound(tcp, peer_addr, acceptor).await {
                                        tracing::debug!(%peer_addr, error = %e, "inbound peer session ended");
                                    }
                                });
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "TCP accept error");
                                tokio::time::sleep(Duration::from_millis(100)).await;
                            }
                        }
                    }
                }
            }
        });

        Ok(local_addr)
    }

    async fn handle_inbound(
        &self,
        tcp: TcpStream,
        peer_addr: SocketAddr,
        acceptor: TlsAcceptor,
    ) -> Result<()> {
        let (lan_only, allow_routable) = {
            let cfg = self.config.lock().unwrap();
            (cfg.lan_only_ips, cfg.allow_routable_ips)
        };
        if !is_allowed_peer_ip(peer_addr.ip(), lan_only, allow_routable) {
            tracing::warn!(%peer_addr, "rejected non-LAN connection attempt");
            return Ok(());
        }
        tcp.set_nodelay(true).ok();

        let mut tls = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(tcp))
            .await
            .context("TLS accept timeout")??;

        let (peer_fp, ekm) = {
            let (_, conn) = tls.get_ref();
            let certs = conn
                .peer_certificates()
                .ok_or_else(|| anyhow!("peer did not present TLS client certificate"))?;
            let fp = cert_fingerprint(&certs[0]);
            let mut ekm_buf = [0u8; 32];
            let _ = conn.export_keying_material(&mut ekm_buf, TLS_EKM_LABEL, None);
            (fp, ekm_buf)
        };

        let first = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_wire_frame(&mut tls))
            .await
            .context("handshake frame timeout")??;

        match first {
            Some(WireFrame::Message(Message::PairInit {
                device_id,
                name,
                os,
                arch,
                version,
                client_nonce,
            })) => {
                self.handle_inbound_pair(
                    tls,
                    peer_addr,
                    peer_fp,
                    ekm,
                    device_id,
                    name,
                    os,
                    arch,
                    version,
                    client_nonce,
                )
                .await
            }
            Some(WireFrame::Message(Message::Hello {
                device_id,
                name,
                os,
                arch,
                version,
                channel,
                session_nonce,
                monitors,
                capabilities,
            })) => {
                if device_id != peer_fp {
                    let _ = write_frame(
                        &mut tls,
                        &Message::Reject {
                            reason: "device_id does not match TLS certificate".into(),
                        },
                    )
                    .await;
                    bail!("spoofed device_id from {peer_addr}");
                }
                if version != PROTOCOL_VERSION {
                    let _ = write_frame(
                        &mut tls,
                        &Message::Reject {
                            reason: format!(
                                "protocol version mismatch: local {PROTOCOL_VERSION}, peer {version}"
                            ),
                        },
                    )
                    .await;
                    return Ok(());
                }
                if !self.trust_store.is_trusted_and_enabled(&peer_fp) {
                    tracing::warn!(%peer_addr, peer = %peer_fp.short(), "rejected untrusted or disabled peer");
                    let _ = write_frame(
                        &mut tls,
                        &Message::Reject {
                            reason: "device is not paired or is disabled".into(),
                        },
                    )
                    .await;
                    return Ok(());
                }

                let local = self.local_state.lock().unwrap().clone();
                write_frame(
                    &mut tls,
                    &Message::Welcome {
                        device_id: self.identity.device_id.clone(),
                        name: local.name,
                        os: OsKind::current(),
                        arch: ArchKind::current(),
                        session_nonce,
                        monitors: local.monitors,
                        capabilities: local.capabilities,
                        active_controller: local.active_controller,
                    },
                )
                .await?;

                let _ = self
                    .trust_store
                    .update_seen(&peer_fp, &name, Some(peer_addr));

                match channel {
                    ChannelRole::Control => {
                        // Simultaneous-dial tie-breaker: if we already have an active session
                        // and our DeviceId is smaller, keep our canonical outbound session.
                        if !self.should_accept_inbound_control(&peer_fp) {
                            tracing::debug!(peer = %peer_fp.short(), "closing duplicate inbound control in favor of canonical outbound");
                            let _ = write_frame(
                                &mut tls,
                                &Message::Goodbye {
                                    reason: "duplicate connection tie-break".into(),
                                },
                            )
                            .await;
                            return Ok(());
                        }
                        let info = PeerInfo {
                            device_id: peer_fp.clone(),
                            name,
                            os,
                            arch,
                            addr: peer_addr,
                            monitors,
                            capabilities,
                            latency_ms: 0,
                            active_controller: None,
                        };
                        self.spawn_control_loop(tls, info, session_nonce);
                        Ok(())
                    }
                    ChannelRole::Data => {
                        self.run_data_loop(tls, peer_fp, session_nonce).await
                    }
                }
            }
            Some(other) => {
                tracing::warn!(%peer_addr, ?other, "unexpected initial frame");
                Ok(())
            }
            None => Ok(()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_inbound_pair<S>(
        &self,
        mut tls: S,
        peer_addr: SocketAddr,
        peer_fp: DeviceId,
        ekm: [u8; 32],
        claimed_id: DeviceId,
        name: String,
        os: OsKind,
        arch: ArchKind,
        version: u32,
        client_nonce: [u8; 16],
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        if claimed_id != peer_fp {
            let _ = write_frame(
                &mut tls,
                &Message::PairReject {
                    reason: "certificate fingerprint mismatch".into(),
                },
            )
            .await;
            return Ok(());
        }
        if version != PROTOCOL_VERSION {
            let _ = write_frame(
                &mut tls,
                &Message::PairReject {
                    reason: format!("protocol version mismatch ({version} != {PROTOCOL_VERSION})"),
                },
            )
            .await;
            return Ok(());
        }
        if let Err(e) = self.pairing.check_rate_limit() {
            let _ = write_frame(
                &mut tls,
                &Message::PairReject {
                    reason: e.to_string(),
                },
            )
            .await;
            return Ok(());
        }

        // If user already opened a PIN window, use it; otherwise generate an on-demand PIN
        // and ask the UI/CLI for explicit consent.
        let mut needs_prompt = false;
        let pin = match self.pairing.current_pin() {
            Some(p) => p,
            None => {
                needs_prompt = true;
                self.pairing
                    .open_window(None, Duration::from_secs(120))?
            }
        };

        let server_nonce = random_bytes::<16>()?;
        let local = self.local_state.lock().unwrap().clone();
        write_frame(
            &mut tls,
            &Message::PairChallenge {
                device_id: self.identity.device_id.clone(),
                name: local.name,
                os: OsKind::current(),
                arch: ArchKind::current(),
                server_nonce,
                requires_pin: true,
            },
        )
        .await?;

        if needs_prompt {
            let (resp_tx, resp_rx) = oneshot::channel();
            let _ = self
                .event_tx
                .send(NetworkEvent::IncomingPairRequest(IncomingPairPrompt {
                    device_id: peer_fp.clone(),
                    name: name.clone(),
                    os,
                    addr: peer_addr,
                    pin: pin.clone(),
                    responder: resp_tx,
                }));
            let allowed = tokio::time::timeout(PAIR_CONSENT_TIMEOUT, resp_rx)
                .await
                .ok()
                .and_then(|r| r.ok())
                .unwrap_or(false);
            if !allowed {
                self.pairing.close_window();
                let _ = write_frame(
                    &mut tls,
                    &Message::PairReject {
                        reason: "pairing declined by user".into(),
                    },
                )
                .await;
                return Ok(());
            }
        }

        let proof = match tokio::time::timeout(PAIR_CONSENT_TIMEOUT, read_wire_frame(&mut tls)).await
        {
            Ok(Ok(Some(WireFrame::Message(Message::PairProof { proof_hmac })))) => proof_hmac,
            _ => {
                let _ = write_frame(
                    &mut tls,
                    &Message::PairReject {
                        reason: "pairing proof timed out".into(),
                    },
                )
                .await;
                return Ok(());
            }
        };

        let expected = compute_pair_proof(
            &pin,
            b"client",
            &ekm,
            &client_nonce,
            &server_nonce,
            &peer_fp,
            &self.identity.device_id,
        );
        if !verify_pair_proof(&expected, &proof) {
            self.pairing.record_failure();
            let _ = write_frame(
                &mut tls,
                &Message::PairReject {
                    reason: "invalid pairing PIN".into(),
                },
            )
            .await;
            return Ok(());
        }

        self.pairing.record_success();
        let server_proof = compute_pair_proof(
            &pin,
            b"server",
            &ekm,
            &client_nonce,
            &server_nonce,
            &peer_fp,
            &self.identity.device_id,
        );
        write_frame(
            &mut tls,
            &Message::PairAccept {
                proof_hmac: server_proof,
            },
        )
        .await?;

        let now = now_unix_secs();
        self.trust_store.upsert(TrustedPeer {
            device_id: peer_fp.clone(),
            name: name.clone(),
            alias: None,
            cert_sha256: peer_fp.0.clone(),
            os,
            arch,
            enabled: true,
            last_addrs: vec![peer_addr],
            paired_at_unix: now,
            last_seen_unix: now,
        })?;
        tracing::info!(%name, peer = %peer_fp.short(), "paired new trusted peer");
        Ok(())
    }

    /// Initiate interactive PIN pairing with a remote peer at `addr`.
    pub async fn pair_with_addr(
        &self,
        addr: SocketAddr,
        expected_id: Option<DeviceId>,
        pin: &str,
    ) -> Result<TrustedPeer> {
        let (lan_only, allow_routable) = {
            let cfg = self.config.lock().unwrap();
            (cfg.lan_only_ips, cfg.allow_routable_ips)
        };
        if !is_allowed_peer_ip(addr.ip(), lan_only, allow_routable) {
            bail!("refusing to pair with non-LAN IP address {addr}");
        }

        let client_cfg = self.identity.tls_client_config(expected_id)?;
        let connector = TlsConnector::from(client_cfg);
        let tcp = tokio::time::timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(addr))
            .await
            .context("TCP connect timeout")??;
        tcp.set_nodelay(true).ok();

        let server_name = ServerName::try_from("move-freely.local")?;
        let mut tls = tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp))
            .await
            .context("TLS handshake timeout")??;

        let (peer_fp, ekm) = {
            let (_, conn) = tls.get_ref();
            let certs = conn
                .peer_certificates()
                .ok_or_else(|| anyhow!("peer presented no TLS certificate"))?;
            let fp = cert_fingerprint(&certs[0]);
            let mut ekm_buf = [0u8; 32];
            let _ = conn.export_keying_material(&mut ekm_buf, TLS_EKM_LABEL, None);
            (fp, ekm_buf)
        };

        let client_nonce = random_bytes::<16>()?;
        let local = self.local_state.lock().unwrap().clone();
        write_frame(
            &mut tls,
            &Message::PairInit {
                device_id: self.identity.device_id.clone(),
                name: local.name,
                os: OsKind::current(),
                arch: ArchKind::current(),
                version: PROTOCOL_VERSION,
                client_nonce,
            },
        )
        .await?;

        let (srv_id, srv_name, srv_os, srv_arch, server_nonce) =
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_wire_frame(&mut tls)).await?? {
                Some(WireFrame::Message(Message::PairChallenge {
                    device_id,
                    name,
                    os,
                    arch,
                    server_nonce,
                    ..
                })) => (device_id, name, os, arch, server_nonce),
                Some(WireFrame::Message(Message::PairReject { reason })) => {
                    bail!("peer rejected pairing: {reason}");
                }
                other => bail!("unexpected pairing response: {other:?}"),
            };

        if srv_id != peer_fp {
            bail!("peer certificate identity mismatch");
        }

        let client_proof = compute_pair_proof(
            pin.trim(),
            b"client",
            &ekm,
            &client_nonce,
            &server_nonce,
            &self.identity.device_id,
            &peer_fp,
        );
        write_frame(
            &mut tls,
            &Message::PairProof {
                proof_hmac: client_proof,
            },
        )
        .await?;

        match tokio::time::timeout(PAIR_CONSENT_TIMEOUT, read_wire_frame(&mut tls)).await?? {
            Some(WireFrame::Message(Message::PairAccept { proof_hmac })) => {
                let expected_srv = compute_pair_proof(
                    pin.trim(),
                    b"server",
                    &ekm,
                    &client_nonce,
                    &server_nonce,
                    &self.identity.device_id,
                    &peer_fp,
                );
                if !verify_pair_proof(&expected_srv, &proof_hmac) {
                    bail!("server pairing proof verification failed");
                }
            }
            Some(WireFrame::Message(Message::PairReject { reason })) => {
                bail!("pairing rejected: {reason}");
            }
            other => bail!("unexpected pairing completion: {other:?}"),
        }

        let now = now_unix_secs();
        let trusted = TrustedPeer {
            device_id: peer_fp.clone(),
            name: srv_name,
            alias: None,
            cert_sha256: peer_fp.0,
            os: srv_os,
            arch: srv_arch,
            enabled: true,
            last_addrs: vec![addr],
            paired_at_unix: now,
            last_seen_unix: now,
        };
        self.trust_store.upsert(trusted.clone())?;
        Ok(trusted)
    }

    /// Dial an already-trusted peer at `addr` (Control + companion Data channel).
    pub async fn connect_trusted_peer(&self, peer_id: DeviceId, addr: SocketAddr) -> Result<()> {
        if !self.trust_store.is_trusted_and_enabled(&peer_id) {
            bail!("peer {peer_id} is not trusted or is disabled");
        }
        if self.is_connected(&peer_id) {
            return Ok(());
        }
        {
            let mut dialing = self.dialing.lock().unwrap();
            if let Some(since) = dialing.get(&peer_id) {
                if since.elapsed() < Duration::from_secs(10) {
                    return Ok(());
                }
            }
            dialing.insert(peer_id.clone(), Instant::now());
        }

        let res = self.connect_trusted_inner(peer_id.clone(), addr).await;
        self.dialing.lock().unwrap().remove(&peer_id);
        res
    }

    async fn connect_trusted_inner(&self, peer_id: DeviceId, addr: SocketAddr) -> Result<()> {
        let (lan_only, allow_routable) = {
            let cfg = self.config.lock().unwrap();
            (cfg.lan_only_ips, cfg.allow_routable_ips)
        };
        if !is_allowed_peer_ip(addr.ip(), lan_only, allow_routable) {
            bail!("blocked non-LAN peer address {addr}");
        }

        let client_cfg = self.identity.tls_client_config(Some(peer_id.clone()))?;
        let connector = TlsConnector::from(client_cfg);
        let tcp = tokio::time::timeout(Duration::from_secs(4), TcpStream::connect(addr)).await??;
        tcp.set_nodelay(true).ok();

        let server_name = ServerName::try_from("move-freely.local")?;
        let mut tls = tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp)).await??;

        let session_nonce = random_u64()?;
        let local = self.local_state.lock().unwrap().clone();
        write_frame(
            &mut tls,
            &Message::Hello {
                device_id: self.identity.device_id.clone(),
                name: local.name,
                os: OsKind::current(),
                arch: ArchKind::current(),
                version: PROTOCOL_VERSION,
                channel: ChannelRole::Control,
                session_nonce,
                monitors: local.monitors,
                capabilities: local.capabilities,
            },
        )
        .await?;

        let welcome = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_wire_frame(&mut tls)).await??;
        let info = match welcome {
            Some(WireFrame::Message(Message::Welcome {
                device_id,
                name,
                os,
                arch,
                monitors,
                capabilities,
                active_controller,
                ..
            })) => {
                if device_id != peer_id {
                    bail!("peer identity mismatch on welcome");
                }
                let _ = self.trust_store.update_seen(&peer_id, &name, Some(addr));
                PeerInfo {
                    device_id,
                    name,
                    os,
                    arch,
                    addr,
                    monitors,
                    capabilities,
                    latency_ms: 0,
                    active_controller,
                }
            }
            Some(WireFrame::Message(Message::Reject { reason })) => {
                bail!("peer rejected connection: {reason}");
            }
            other => bail!("unexpected welcome frame: {other:?}"),
        };

        let mgr_ctrl = self.clone();
        mgr_ctrl.spawn_control_loop(tls, info, session_nonce);

        // Spawn companion Data TLS channel after Control session is registered.
        let mgr_data = self.clone();
        let pid_data = peer_id.clone();
        tokio::spawn(async move {
            if let Err(e) = mgr_data.connect_data_channel(pid_data, addr, session_nonce).await {
                tracing::debug!(error = %e, "companion data channel ended");
            }
        });

        Ok(())
    }

    async fn connect_data_channel(
        &self,
        peer_id: DeviceId,
        addr: SocketAddr,
        session_nonce: u64,
    ) -> Result<()> {
        let client_cfg = self.identity.tls_client_config(Some(peer_id.clone()))?;
        let connector = TlsConnector::from(client_cfg);
        let tcp = tokio::time::timeout(Duration::from_secs(4), TcpStream::connect(addr)).await??;
        let server_name = ServerName::try_from("move-freely.local")?;
        let mut tls = tokio::time::timeout(HANDSHAKE_TIMEOUT, connector.connect(server_name, tcp)).await??;
        let local = self.local_state.lock().unwrap().clone();
        write_frame(
            &mut tls,
            &Message::Hello {
                device_id: self.identity.device_id.clone(),
                name: local.name,
                os: OsKind::current(),
                arch: ArchKind::current(),
                version: PROTOCOL_VERSION,
                channel: ChannelRole::Data,
                session_nonce,
                monitors: Vec::new(),
                capabilities: local.capabilities,
            },
        )
        .await?;
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_wire_frame(&mut tls)).await?? {
            Some(WireFrame::Message(Message::Welcome { .. })) => {}
            _ => bail!("data channel handshake rejected"),
        }
        self.run_data_loop(tls, peer_id, session_nonce).await
    }

    fn should_accept_inbound_control(&self, peer_id: &DeviceId) -> bool {
        let guard = self.sessions.lock().unwrap();
        if !guard.contains_key(peer_id) {
            return true;
        }
        // Deterministic tie-breaker: the smaller DeviceId owns the outbound dial.
        // If the remote peer has a smaller ID than ours, its inbound dial wins.
        peer_id < &self.identity.device_id
    }

    fn spawn_control_loop<S>(
        &self,
        tls: S,
        info: PeerInfo,
        session_nonce: u64,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let peer_id = info.device_id.clone();
        let (high_tx, mut high_rx) = mpsc::unbounded_channel::<Message>();
        let (normal_tx, mut normal_rx) = mpsc::unbounded_channel::<Message>();
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
        let latency_ms = Arc::new(AtomicU64::new(0));

        // Replace any older session cleanly.
        if let Some(old) = self.sessions.lock().unwrap().insert(
            peer_id.clone(),
            PeerSession {
                info: info.clone(),
                session_nonce,
                high_tx: high_tx.clone(),
                normal_tx,
                data_tx: Mutex::new(None),
                latency_ms: latency_ms.clone(),
                cancel_tx,
            },
        ) {
            let _ = old.cancel_tx.send(());
        }

        let _ = self.event_tx.send(NetworkEvent::PeerConnected(info.clone()));
        let mgr = self.clone();

        tokio::spawn(async move {
            let (mut reader, mut writer) = tokio::io::split(tls);
            let writer_task = tokio::spawn(async move {
                loop {
                    let msg = tokio::select! {
                        biased;
                        m = high_rx.recv() => match m {
                            Some(m) => m,
                            None => break,
                        },
                        m = normal_rx.recv() => match m {
                            Some(m) => m,
                            None => break,
                        }
                    };
                    if write_frame(&mut writer, &msg).await.is_err() {
                        break;
                    }
                }
            });

            let start = Instant::now();
            let mut last_rx = Instant::now();
            let mut hb_seq: u64 = 0;
            let mut hb_tick = tokio::time::interval(HEARTBEAT_INTERVAL);
            hb_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

            let mut disconnect_reason = "connection closed".to_string();

            loop {
                tokio::select! {
                    _ = &mut cancel_rx => {
                        disconnect_reason = "superseded or disconnected".into();
                        break;
                    }
                    _ = hb_tick.tick() => {
                        if last_rx.elapsed() > HEARTBEAT_TIMEOUT {
                            disconnect_reason = "heartbeat timeout".into();
                            break;
                        }
                        hb_seq = hb_seq.wrapping_add(1);
                        let sent_ms = start.elapsed().as_millis() as u64;
                        if high_tx.send(Message::Heartbeat { seq: hb_seq, sent_ms }).is_err() {
                            break;
                        }
                    }
                    frame = read_wire_frame(&mut reader) => {
                        match frame {
                            Ok(Some(WireFrame::Message(msg))) => {
                                last_rx = Instant::now();
                                match msg {
                                    Message::Heartbeat { seq, sent_ms } => {
                                        let _ = high_tx.send(Message::HeartbeatAck { seq, echoed_ms: sent_ms });
                                    }
                                    Message::HeartbeatAck { echoed_ms, .. } => {
                                        let now_ms = start.elapsed().as_millis() as u64;
                                        let rtt = now_ms.saturating_sub(echoed_ms);
                                        let prev = latency_ms.load(Ordering::Relaxed);
                                        let smoothed = if prev == 0 { rtt } else { (prev * 3 + rtt) / 4 };
                                        latency_ms.store(smoothed, Ordering::Relaxed);
                                        let _ = mgr.event_tx.send(NetworkEvent::LatencyUpdated {
                                            device_id: peer_id.clone(),
                                            latency_ms: smoothed,
                                        });
                                    }
                                    Message::Goodbye { reason } => {
                                        disconnect_reason = reason;
                                        break;
                                    }
                                    other => {
                                        let _ = mgr.event_tx.send(NetworkEvent::ControlMessage {
                                            from: peer_id.clone(),
                                            msg: other,
                                        });
                                    }
                                }
                            }
                            Ok(Some(WireFrame::Chunk(chunk))) => {
                                last_rx = Instant::now();
                                let _ = mgr.event_tx.send(NetworkEvent::DataChunk {
                                    from: peer_id.clone(),
                                    chunk,
                                });
                            }
                            Ok(None) => {
                                disconnect_reason = "peer closed stream".into();
                                break;
                            }
                            Err(e) => {
                                disconnect_reason = format!("read error: {e}");
                                break;
                            }
                        }
                    }
                }
            }

            writer_task.abort();
            let should_notify = {
                let mut guard = mgr.sessions.lock().unwrap();
                if guard
                    .get(&peer_id)
                    .is_some_and(|s| s.session_nonce == session_nonce)
                {
                    guard.remove(&peer_id);
                    true
                } else {
                    false
                }
            };
            if should_notify {
                let _ = mgr.event_tx.send(NetworkEvent::PeerDisconnected {
                    device_id: peer_id,
                    reason: disconnect_reason,
                });
            }
        });
    }

    async fn run_data_loop<S>(
        &self,
        tls: S,
        peer_id: DeviceId,
        session_nonce: u64,
    ) -> Result<()>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (tx, mut rx) = mpsc::channel::<WireFrame>(64);
        {
            let guard = self.sessions.lock().unwrap();
            let Some(sess) = guard.get(&peer_id) else {
                return Ok(());
            };
            if sess.session_nonce != session_nonce {
                return Ok(());
            }
            *sess.data_tx.lock().unwrap() = Some(tx);
        }

        let (mut reader, mut writer) = tokio::io::split(tls);
        let writer_task = tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if write_wire_frame(&mut writer, &frame).await.is_err() {
                    break;
                }
            }
        });

        while let Ok(Some(frame)) = read_wire_frame(&mut reader).await {
            match frame {
                WireFrame::Chunk(chunk) => {
                    let _ = self.event_tx.send(NetworkEvent::DataChunk {
                        from: peer_id.clone(),
                        chunk,
                    });
                }
                WireFrame::Message(msg) => {
                    let _ = self.event_tx.send(NetworkEvent::ControlMessage {
                        from: peer_id.clone(),
                        msg,
                    });
                }
            }
        }

        writer_task.abort();
        let guard = self.sessions.lock().unwrap();
        if let Some(sess) = guard.get(&peer_id) {
            if sess.session_nonce == session_nonce {
                *sess.data_tx.lock().unwrap() = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn two_peers_pair_over_tls_and_exchange_authenticated_messages() {
        let dir_a = std::env::temp_dir().join(format!("mf-net-a-{}", uuid::Uuid::new_v4()));
        let dir_b = std::env::temp_dir().join(format!("mf-net-b-{}", uuid::Uuid::new_v4()));

        let id_a = DeviceIdentity::load_or_generate(&dir_a).unwrap();
        let id_b = DeviceIdentity::load_or_generate(&dir_b).unwrap();
        let store_a = TrustedPeerStore::load_or_create(&dir_a).unwrap();
        let store_b = TrustedPeerStore::load_or_create(&dir_b).unwrap();

        let mut cfg_a = AppConfig::default();
        cfg_a.device_name = "peer-a".into();
        cfg_a.bind_ip = "127.0.0.1".into();
        cfg_a.listen_port = 0;
        let mut cfg_b = AppConfig::default();
        cfg_b.device_name = "peer-b".into();
        cfg_b.bind_ip = "127.0.0.1".into();
        cfg_b.listen_port = 0;

        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();

        let pair_b = Arc::new(PairingManager::new());
        let pin = pair_b.open_window(None, Duration::from_secs(60)).unwrap();

        let mgr_a = NetworkManager::new(
            id_a.clone(),
            store_a.clone(),
            Arc::new(PairingManager::new()),
            Arc::new(Mutex::new(cfg_a)),
            LocalAdvertiseState {
                name: "peer-a".into(),
                monitors: vec![],
                capabilities: PeerCapabilities::default(),
                active_controller: None,
            },
            tx_a,
        );
        let mgr_b = NetworkManager::new(
            id_b.clone(),
            store_b.clone(),
            pair_b,
            Arc::new(Mutex::new(cfg_b)),
            LocalAdvertiseState {
                name: "peer-b".into(),
                monitors: vec![],
                capabilities: PeerCapabilities::default(),
                active_controller: None,
            },
            tx_b,
        );

        let (_shut_tx, shut_rx) = tokio::sync::watch::channel(false);
        let addr_b = mgr_b.start_listener(shut_rx.clone()).await.unwrap();
        let _addr_a = mgr_a.start_listener(shut_rx).await.unwrap();

        // Untrusted connect attempt must fail before pairing.
        assert!(mgr_a
            .connect_trusted_peer(id_b.device_id.clone(), addr_b)
            .await
            .is_err());

        // Wrong PIN must be rejected.
        let wrong_pin = if pin == "123456" { "654321" } else { "123456" };
        assert!(mgr_a
            .pair_with_addr(addr_b, Some(id_b.device_id.clone()), wrong_pin)
            .await
            .is_err());

        // Correct PIN pairs both directions cleanly.
        let trusted_b = mgr_a
            .pair_with_addr(addr_b, Some(id_b.device_id.clone()), &pin)
            .await
            .unwrap();
        assert_eq!(trusted_b.device_id, id_b.device_id);
        assert!(store_b.is_trusted_and_enabled(&id_a.device_id));

        // Now authenticated mTLS connection succeeds.
        let mgr_a_conn = mgr_a.clone();
        let target_b = id_b.device_id.clone();
        tokio::spawn(async move {
            let _ = mgr_a_conn.connect_trusted_peer(target_b, addr_b).await;
        });

        // Wait for PeerConnected on both sides.
        let mut connected_b = false;
        for _ in 0..30 {
            if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(200), rx_b.recv()).await
            {
                if matches!(ev, NetworkEvent::PeerConnected(_)) {
                    connected_b = true;
                    break;
                }
            }
        }
        assert!(connected_b);
        while let Ok(Some(_)) = tokio::time::timeout(Duration::from_millis(50), rx_a.recv()).await {}

        // Send a high-priority input event A -> B.
        assert!(mgr_a.send_high(&id_b.device_id, Message::Enter { x: 42, y: 84 }));

        let mut got_enter = false;
        for _ in 0..20 {
            if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(200), rx_b.recv()).await
            {
                if matches!(
                    ev,
                    NetworkEvent::ControlMessage {
                        msg: Message::Enter { x: 42, y: 84 },
                        ..
                    }
                ) {
                    got_enter = true;
                    break;
                }
            }
        }
        assert!(got_enter);

        // Revoking A disconnects and blocks reconnect.
        mgr_b.disconnect_peer(&id_a.device_id, "revoked");
        store_b.revoke(&id_a.device_id).unwrap();

        // Wait for A to observe B's Goodbye/disconnect before testing reconnect rejection.
        let mut a_saw_disconnect = false;
        for _ in 0..20 {
            if let Ok(Some(ev)) = tokio::time::timeout(Duration::from_millis(100), rx_a.recv()).await
            {
                if matches!(ev, NetworkEvent::PeerDisconnected { .. }) {
                    a_saw_disconnect = true;
                    break;
                }
            }
        }
        assert!(a_saw_disconnect);

        assert!(mgr_a
            .connect_trusted_peer(id_b.device_id.clone(), addr_b)
            .await
            .is_err());

        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }
}
