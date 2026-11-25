# Move Freely Wire Protocol & Security

## Framing (`mf-protocol::frame`)

Every frame on both Control and Data TLS streams uses:

```
+------------------+-----------+------------------------+
| length (u32 BE)  | kind (u8) | payload (length - 1 B) |
+------------------+-----------+------------------------+
```

- Maximum frame length (`MAX_FRAME_LEN`): `8 MiB`.
- `kind = 0x01`: UTF-8 JSON-encoded `Message` enum.
- `kind = 0x02`: Raw binary bulk chunk (`transfer_id: u64 BE | file_index: u32 BE | offset: u64 BE | is_last: u8 | data: [u8]`).

## Security & Pairing Handshake

1. **Identity**: Each device generates a persistent ECDSA P-256 certificate + PKCS#8 private key (`identity.crt.pem`, `identity.key.pem`, mode `0600`). Its `DeviceId` is the SHA-256 hex fingerprint of its DER certificate.
2. **Transport Encryption**: All connections require TLS 1.2/1.3 (`rustls` + `ring`) with client and server certificates verified cryptographically during TLS handshake (`CertificateVerify`).
3. **LAN-Only Policy**: By default (`lan_only_ips = true`), peers outside RFC1918, loopback, link-local, ULA, and CGNAT (`100.64.0.0/10`) ranges are rejected before TLS negotiation.
4. **Channel-Bound PIN Pairing**:
   - Initiator sends `PairInit { device_id, name, os, arch, version, client_nonce }`.
   - Responder requires an active 6-digit PIN window (120s TTL, max 5 failed attempts before 30s lockout) and explicit user consent, replying `PairChallenge { server_nonce, ... }`.
   - Both sides compute `HMAC-SHA256(pin, role_tag || tls_ekm || client_nonce || server_nonce || client_fp || server_fp)` bound to TLS Exported Keying Material (`EXPORTER-move-freely-pair-v2`), verified in constant time via `PairProof` / `PairAccept`.
   - Once accepted, both sides persist the peer fingerprint in `trusted_peers.json`.
5. **Authenticated Sessions**: Every subsequent `Hello` requires the peer's certificate fingerprint to match an `enabled` entry in `trusted_peers.json`; unknown or revoked peers receive `Reject` and are dropped immediately.

## 2D Multi-Monitor Geometry (`mf-protocol::layout`)

- Each device reports its OS monitors (`x, y, width, height, physical_width, physical_height, scale_factor, is_primary`).
- Global arrangement uses DPI-normalized logical dimensions (`physical / scale_factor`) with gap-free multi-monitor packing (`normalize_device_monitors`), preserving exact OS coordinate mappings inside each monitor.
- Edge crossing supports `Overlapping` (macOS Display Settings-style vertical/horizontal span overlap) and `Proportional` boundary modes, plus configurable `edge_resistance_ms`, `edge_barrier_px`, `corner_deadzone_px`, and `pointer_sensitivity`.
