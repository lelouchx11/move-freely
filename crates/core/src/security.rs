//! Cryptographic device identity, mTLS verification, PIN pairing with channel
//! binding, trusted peer persistence, and LAN IP enforcement.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use mf_protocol::{ArchKind, DeviceId, OsKind};
use rcgen::{CertificateParams, KeyPair};
use ring::digest::{digest, SHA256};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::ring::default_provider;
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig, SignatureScheme};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

pub const TLS_EKM_LABEL: &[u8] = b"EXPORTER-move-freely-pair-v2";

/// Persistent X.509 identity for the local machine.
#[derive(Clone)]
pub struct DeviceIdentity {
    pub device_id: DeviceId,
    pub cert_sha256: String,
    pub cert_der: CertificateDer<'static>,
    key_der: Zeroizing<Vec<u8>>,
}

impl DeviceIdentity {
    /// Load an existing identity from `data_dir` or generate a new ECDSA P-256 identity.
    pub fn load_or_generate(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let cert_path = data_dir.join("identity.crt.pem");
        let key_path = data_dir.join("identity.key.pem");

        if cert_path.exists() && key_path.exists() {
            let cert_pem = std::fs::read(&cert_path)?;
            let key_pem = Zeroizing::new(std::fs::read(&key_path)?);
            let mut cert_reader = std::io::BufReader::new(cert_pem.as_slice());
            let cert_der = rustls_pemfile::certs(&mut cert_reader)
                .next()
                .ok_or_else(|| anyhow!("identity.crt.pem contains no certificate"))??
                .into_owned();

            let mut key_reader = std::io::BufReader::new(key_pem.as_slice());
            let pk = rustls_pemfile::private_key(&mut key_reader)?
                .ok_or_else(|| anyhow!("identity.key.pem contains no private key"))?;
            let key_der = Zeroizing::new(pk.secret_der().to_vec());

            let cert_sha256 = sha256_hex(cert_der.as_ref());
            let device_id = DeviceId::from(cert_sha256.clone());
            return Ok(Self {
                device_id,
                cert_sha256,
                cert_der,
                key_der,
            });
        }

        let key_pair = KeyPair::generate()?;
        let mut params = CertificateParams::new(vec![
            "move-freely.local".to_string(),
            "localhost".to_string(),
        ])?;
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "move-freely-peer");
        let cert = params.self_signed(&key_pair)?;
        let cert_pem = cert.pem();
        let key_pem = Zeroizing::new(key_pair.serialize_pem());
        let cert_der = cert.der().clone();
        let key_der = Zeroizing::new(key_pair.serialize_der());

        write_secret_file(&cert_path, cert_pem.as_bytes())?;
        write_secret_file(&key_path, key_pem.as_bytes())?;

        let cert_sha256 = sha256_hex(cert_der.as_ref());
        let device_id = DeviceId::from(cert_sha256.clone());
        Ok(Self {
            device_id,
            cert_sha256,
            cert_der,
            key_der,
        })
    }

    /// Build a [`rustls::ServerConfig`] that verifies peer cert signatures and
    /// presents our identity certificate. Authorization of `DeviceId` is enforced
    /// immediately post-handshake (or paired via PIN consent).
    pub fn tls_server_config(&self) -> Result<Arc<ServerConfig>> {
        let provider = Arc::new(default_provider());
        let verifier = Arc::new(PeerCertVerifier::new(provider.clone()));
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.to_vec()));
        let cfg = ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![self.cert_der.clone()], key)?;
        Ok(Arc::new(cfg))
    }

    /// Build a [`rustls::ClientConfig`] that presents our identity cert and
    /// validates the server cert signature (plus optional pinned expected fingerprint).
    pub fn tls_client_config(&self, expected_peer: Option<DeviceId>) -> Result<Arc<ClientConfig>> {
        let provider = Arc::new(default_provider());
        let verifier = Arc::new(PinnedOrSelfSignedServerVerifier {
            provider: provider.clone(),
            expected_peer,
        });
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.to_vec()));
        let cfg = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(vec![self.cert_der.clone()], key)?;
        Ok(Arc::new(cfg))
    }
}

/// Custom TLS verifier for peer-to-peer self-signed certs: checks cryptographic
/// handshake ownership (`verify_tls*_signature`) while leaving trust-store vs
/// pairing-mode policy to the session state machine.
#[derive(Debug)]
struct PeerCertVerifier {
    provider: Arc<rustls::crypto::CryptoProvider>,
    subjects: Vec<DistinguishedName>,
}

impl PeerCertVerifier {
    fn new(provider: Arc<rustls::crypto::CryptoProvider>) -> Self {
        Self {
            provider,
            subjects: Vec::new(),
        }
    }
}

impl ClientCertVerifier for PeerCertVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &self.subjects
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedOrSelfSignedServerVerifier {
    provider: Arc<rustls::crypto::CryptoProvider>,
    expected_peer: Option<DeviceId>,
}

impl ServerCertVerifier for PinnedOrSelfSignedServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if let Some(expected) = &self.expected_peer {
            let fp = sha256_hex(end_entity.as_ref());
            if &fp != expected.as_str() {
                return Err(rustls::Error::General(
                    "peer certificate fingerprint mismatch".into(),
                ));
            }
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Extract the [`DeviceId`] (SHA-256 hex fingerprint) from a peer TLS cert.
pub fn cert_fingerprint(cert: &CertificateDer<'_>) -> DeviceId {
    DeviceId::from(sha256_hex(cert.as_ref()))
}

pub fn sha256_bytes(data: &[u8]) -> [u8; 32] {
    let d = digest(&SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

pub fn sha256_hex(data: &[u8]) -> String {
    let bytes = sha256_bytes(data);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn random_bytes<const N: usize>() -> Result<[u8; N]> {
    let rng = SystemRandom::new();
    let mut buf = [0u8; N];
    rng.fill(&mut buf)
        .map_err(|_| anyhow!("system CSPRNG failure"))?;
    Ok(buf)
}

pub fn random_u64() -> Result<u64> {
    let bytes = random_bytes::<8>()?;
    Ok(u64::from_le_bytes(bytes))
}

/// Compute a channel-bound pairing proof HMAC-SHA256.
pub fn compute_pair_proof(
    pin: &str,
    role_tag: &[u8],
    ekm: &[u8],
    client_nonce: &[u8; 16],
    server_nonce: &[u8; 16],
    client_fp: &DeviceId,
    server_fp: &DeviceId,
) -> [u8; 32] {
    let key = hmac::Key::new(hmac::HMAC_SHA256, pin.as_bytes());
    let mut ctx = hmac::Context::with_key(&key);
    ctx.update(role_tag);
    ctx.update(&(ekm.len() as u32).to_be_bytes());
    ctx.update(ekm);
    ctx.update(client_nonce);
    ctx.update(server_nonce);
    ctx.update(client_fp.as_str().as_bytes());
    ctx.update(server_fp.as_str().as_bytes());
    let tag = ctx.sign();
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
    out
}

pub fn verify_pair_proof(expected: &[u8; 32], candidate: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for (a, b) in expected.iter().zip(candidate.iter()) {
        diff |= a ^ b;
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// Pairing PIN State & Rate Limiter
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ActivePairingWindow {
    pub pin: String,
    pub expires_at: Instant,
    pub failed_attempts: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PairingStatus {
    pub active: bool,
    pub pin: Option<String>,
    pub expires_in_secs: u64,
}

pub struct PairingManager {
    state: Mutex<Option<ActivePairingWindow>>,
    locked_until: Mutex<Option<Instant>>,
}

impl Default for PairingManager {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
            locked_until: Mutex::new(None),
        }
    }

    /// Open a fresh 6-digit pairing PIN valid for `ttl` (default 120s).
    pub fn open_window(&self, custom_pin: Option<String>, ttl: Duration) -> Result<String> {
        let pin = match custom_pin {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => {
                let raw = random_u64()? % 1_000_000;
                format!("{raw:06}")
            }
        };
        let mut guard = self.state.lock().unwrap();
        *guard = Some(ActivePairingWindow {
            pin: pin.clone(),
            expires_at: Instant::now() + ttl,
            failed_attempts: 0,
        });
        Ok(pin)
    }

    pub fn close_window(&self) {
        *self.state.lock().unwrap() = None;
    }

    pub fn status(&self) -> PairingStatus {
        let mut guard = self.state.lock().unwrap();
        if let Some(win) = guard.as_ref() {
            if let Some(rem) = win.expires_at.checked_duration_since(Instant::now()) {
                return PairingStatus {
                    active: true,
                    pin: Some(win.pin.clone()),
                    expires_in_secs: rem.as_secs(),
                };
            }
            *guard = None;
        }
        PairingStatus {
            active: false,
            pin: None,
            expires_in_secs: 0,
        }
    }

    /// Return active PIN if open and not rate-limited.
    pub fn check_rate_limit(&self) -> Result<()> {
        let mut lock = self.locked_until.lock().unwrap();
        if let Some(until) = *lock {
            if Instant::now() < until {
                anyhow::bail!("too many failed pairing attempts; try again shortly");
            }
            *lock = None;
        }
        Ok(())
    }

    pub fn current_pin(&self) -> Option<String> {
        let mut guard = self.state.lock().unwrap();
        if let Some(win) = guard.as_ref() {
            if Instant::now() < win.expires_at {
                return Some(win.pin.clone());
            }
            *guard = None;
        }
        None
    }

    pub fn record_failure(&self) {
        let mut guard = self.state.lock().unwrap();
        if let Some(win) = guard.as_mut() {
            win.failed_attempts += 1;
            if win.failed_attempts >= 5 {
                *guard = None;
                *self.locked_until.lock().unwrap() = Some(Instant::now() + Duration::from_secs(30));
            }
        }
    }

    pub fn record_success(&self) {
        *self.state.lock().unwrap() = None;
        *self.locked_until.lock().unwrap() = None;
    }
}

// ---------------------------------------------------------------------------
// Trusted Peer Store
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrustedPeer {
    pub device_id: DeviceId,
    pub name: String,
    pub alias: Option<String>,
    pub cert_sha256: String,
    pub os: OsKind,
    pub arch: ArchKind,
    pub enabled: bool,
    pub last_addrs: Vec<SocketAddr>,
    pub paired_at_unix: u64,
    pub last_seen_unix: u64,
}

impl TrustedPeer {
    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.name)
    }
}

#[derive(Clone)]
pub struct TrustedPeerStore {
    path: PathBuf,
    peers: Arc<RwLock<HashMap<DeviceId, TrustedPeer>>>,
}

impl TrustedPeerStore {
    pub fn load_or_create(data_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(data_dir)?;
        let path = data_dir.join("trusted_peers.json");
        let map: HashMap<DeviceId, TrustedPeer> = if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            let list: Vec<TrustedPeer> = serde_json::from_str(&raw)?;
            list.into_iter()
                .map(|p| (p.device_id.clone(), p))
                .collect()
        } else {
            HashMap::new()
        };
        Ok(Self {
            path,
            peers: Arc::new(RwLock::new(map)),
        })
    }

    pub fn list(&self) -> Vec<TrustedPeer> {
        let guard = self.peers.read().unwrap();
        let mut out: Vec<_> = guard.values().cloned().collect();
        out.sort_by(|a, b| a.display_name().cmp(b.display_name()));
        out
    }

    pub fn get(&self, id: &DeviceId) -> Option<TrustedPeer> {
        self.peers.read().unwrap().get(id).cloned()
    }

    pub fn is_trusted_and_enabled(&self, id: &DeviceId) -> bool {
        self.peers
            .read()
            .unwrap()
            .get(id)
            .is_some_and(|p| p.enabled)
    }

    pub fn upsert(&self, peer: TrustedPeer) -> Result<()> {
        {
            let mut guard = self.peers.write().unwrap();
            guard.insert(peer.device_id.clone(), peer);
        }
        self.persist()
    }

    pub fn set_enabled(&self, id: &DeviceId, enabled: bool) -> Result<bool> {
        let changed = {
            let mut guard = self.peers.write().unwrap();
            if let Some(p) = guard.get_mut(id) {
                p.enabled = enabled;
                true
            } else {
                false
            }
        };
        if changed {
            self.persist()?;
        }
        Ok(changed)
    }

    pub fn rename(&self, id: &DeviceId, alias: Option<String>) -> Result<bool> {
        let changed = {
            let mut guard = self.peers.write().unwrap();
            if let Some(p) = guard.get_mut(id) {
                p.alias = alias.filter(|s| !s.trim().is_empty());
                true
            } else {
                false
            }
        };
        if changed {
            self.persist()?;
        }
        Ok(changed)
    }

    pub fn revoke(&self, id: &DeviceId) -> Result<bool> {
        let removed = self.peers.write().unwrap().remove(id).is_some();
        if removed {
            self.persist()?;
        }
        Ok(removed)
    }

    pub fn update_seen(&self, id: &DeviceId, name: &str, addr: Option<SocketAddr>) -> Result<()> {
        {
            let mut guard = self.peers.write().unwrap();
            if let Some(p) = guard.get_mut(id) {
                p.name = name.to_string();
                p.last_seen_unix = now_unix_secs();
                if let Some(a) = addr {
                    p.last_addrs.retain(|existing| existing != &a);
                    p.last_addrs.insert(0, a);
                    p.last_addrs.truncate(4);
                }
            }
        }
        self.persist()
    }

    fn persist(&self) -> Result<()> {
        let list = self.list();
        let data = serde_json::to_vec_pretty(&list)?;
        let tmp = self.path.with_extension("json.tmp");
        write_secret_file(&tmp, &data)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Enforce local-network-only policy (RFC1918, loopback, link-local, ULA, CGNAT/mesh).
pub fn is_allowed_peer_ip(ip: IpAddr, lan_only: bool, allow_routable: bool) -> bool {
    if !lan_only || allow_routable {
        return true;
    }
    match ip {
        IpAddr::V4(v4) => is_local_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                is_local_v4(mapped)
            } else {
                v6.is_loopback() || is_ula_v6(v6) || is_link_local_v6(v6)
            }
        }
    }
}

fn is_local_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        // Shared address space / CGNAT (often used by Tailscale/ZeroTier local mesh)
        || (o[0] == 100 && (o[1] & 0b1100_0000) == 64)
}

fn is_ula_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xfe00) == 0xfc00
}

fn is_link_local_v6(ip: Ipv6Addr) -> bool {
    (ip.segments()[0] & 0xffc0) == 0xfe80
}

fn write_secret_file(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_generation_and_persistence_works() {
        let dir = std::env::temp_dir().join(format!("mf-sec-test-{}", uuid::Uuid::new_v4()));
        let id1 = DeviceIdentity::load_or_generate(&dir).unwrap();
        let id2 = DeviceIdentity::load_or_generate(&dir).unwrap();
        assert_eq!(id1.device_id, id2.device_id);
        assert_eq!(id1.cert_der, id2.cert_der);
        assert!(id1.tls_server_config().is_ok());
        assert!(id1.tls_client_config(Some(id2.device_id.clone())).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn pair_proof_and_rate_limiting_work() {
        let mgr = PairingManager::new();
        let pin = mgr.open_window(None, Duration::from_secs(60)).unwrap();
        assert_eq!(pin.len(), 6);
        let ekm = [9u8; 32];
        let cn = [1u8; 16];
        let sn = [2u8; 16];
        let cfp = DeviceId::from("client");
        let sfp = DeviceId::from("server");
        let p1 = compute_pair_proof(&pin, b"client", &ekm, &cn, &sn, &cfp, &sfp);
        let p2 = compute_pair_proof(&pin, b"client", &ekm, &cn, &sn, &cfp, &sfp);
        assert!(verify_pair_proof(&p1, &p2));
        let wrong = compute_pair_proof("000000", b"client", &ekm, &cn, &sn, &cfp, &sfp);
        if pin != "000000" {
            assert!(!verify_pair_proof(&p1, &wrong));
        }

        for _ in 0..5 {
            mgr.record_failure();
        }
        assert!(mgr.check_rate_limit().is_err());
    }

    #[test]
    fn lan_ip_filter_rejects_public_ips() {
        assert!(is_allowed_peer_ip("127.0.0.1".parse().unwrap(), true, false));
        assert!(is_allowed_peer_ip("192.168.1.42".parse().unwrap(), true, false));
        assert!(is_allowed_peer_ip("10.0.0.5".parse().unwrap(), true, false));
        assert!(is_allowed_peer_ip("172.16.5.9".parse().unwrap(), true, false));
        assert!(is_allowed_peer_ip("169.254.1.2".parse().unwrap(), true, false));
        assert!(!is_allowed_peer_ip("8.8.8.8".parse().unwrap(), true, false));
        assert!(!is_allowed_peer_ip("1.1.1.1".parse().unwrap(), true, false));
        assert!(is_allowed_peer_ip("8.8.8.8".parse().unwrap(), true, true));
    }

    #[test]
    fn trusted_peer_store_persists_and_revokes() {
        let dir = std::env::temp_dir().join(format!("mf-trust-test-{}", uuid::Uuid::new_v4()));
        let store = TrustedPeerStore::load_or_create(&dir).unwrap();
        let peer_id = DeviceId::from("deadbeef");
        store
            .upsert(TrustedPeer {
                device_id: peer_id.clone(),
                name: "winbox".into(),
                alias: None,
                cert_sha256: "deadbeef".into(),
                os: OsKind::Windows,
                arch: ArchKind::X86_64,
                enabled: true,
                last_addrs: vec![],
                paired_at_unix: 100,
                last_seen_unix: 100,
            })
            .unwrap();
        assert!(store.is_trusted_and_enabled(&peer_id));
        store.set_enabled(&peer_id, false).unwrap();
        assert!(!store.is_trusted_and_enabled(&peer_id));
        store.revoke(&peer_id).unwrap();
        assert!(store.get(&peer_id).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
