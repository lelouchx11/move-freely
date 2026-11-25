//! Universal bidirectional clipboard synchronization, loop/duplicate
//! prevention, Lamport conflict resolution, and secure ephemeral file staging.

pub mod platform;

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use mf_protocol::{
    validate_rel_path, BinaryChunk, DeviceId, Message, SharedFileEntry,
};
use ring::digest::{Context as DigestCtx, SHA256};

use crate::config::ClipboardSettings;
use crate::security::sha256_bytes;

pub const CHUNK_SIZE: usize = 64 * 1024;
const MAX_FILES_PER_CLIPBOARD: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardPayload {
    Text {
        plain: String,
        html: Option<String>,
        rtf: Option<String>,
    },
    Image {
        png_bytes: Vec<u8>,
        width: u32,
        height: u32,
    },
    Files {
        paths: Vec<PathBuf>,
    },
}

impl ClipboardPayload {
    pub fn content_hash(&self) -> [u8; 32] {
        let mut ctx = DigestCtx::new(&SHA256);
        match self {
            ClipboardPayload::Text { plain, html, rtf } => {
                ctx.update(b"TEXT\0");
                ctx.update(plain.as_bytes());
                ctx.update(b"\0");
                if let Some(h) = html {
                    ctx.update(h.as_bytes());
                }
                ctx.update(b"\0");
                if let Some(r) = rtf {
                    ctx.update(r.as_bytes());
                }
            }
            ClipboardPayload::Image {
                png_bytes,
                width,
                height,
            } => {
                ctx.update(b"IMG\0");
                ctx.update(&width.to_be_bytes());
                ctx.update(&height.to_be_bytes());
                ctx.update(png_bytes);
            }
            ClipboardPayload::Files { paths } => {
                ctx.update(b"FILES\0");
                for p in paths {
                    ctx.update(p.to_string_lossy().as_bytes());
                    ctx.update(b"\0");
                    if let Ok(meta) = fs::metadata(p) {
                        ctx.update(&meta.len().to_be_bytes());
                    }
                }
            }
        }
        let d = ctx.finish();
        let mut out = [0u8; 32];
        out.copy_from_slice(d.as_ref());
        out
    }
}

/// Manages clipboard synchronization state, loop prevention, deduplication,
/// conflict resolution, and ephemeral staging for incoming files/images.
pub struct ClipboardSyncCoordinator {
    local_id: DeviceId,
    lamport_seq: u64,
    latest_version: (u64, DeviceId),
    last_local_hash: Option<[u8; 32]>,
    last_injected_hash: Option<[u8; 32]>,
    staging_root: PathBuf,
    active_staging_dirs: Vec<PathBuf>,
    next_transfer_id: u64,
    inbound_transfers: HashMap<u64, InboundTransfer>,
}

struct InboundTransfer {
    seq: u64,
    origin: DeviceId,
    expected_sha256: [u8; 32],
    kind: InboundKind,
    received_bytes: u64,
    max_bytes: u64,
}

enum InboundKind {
    Image {
        width: u32,
        height: u32,
        total_bytes: u64,
        buf: Vec<u8>,
    },
    Files {
        staging_dir: PathBuf,
        entries: Vec<SharedFileEntry>,
        #[allow(dead_code)]
        total_bytes: u64,
        open_file: Option<(u32, File, DigestCtx)>,
        completed_hashes: HashMap<u32, [u8; 32]>,
    },
}

impl ClipboardSyncCoordinator {
    pub fn new(local_id: DeviceId, staging_root: PathBuf) -> Self {
        let _ = create_private_dir(&staging_root);
        Self {
            local_id: local_id.clone(),
            lamport_seq: 0,
            latest_version: (0, local_id),
            last_local_hash: None,
            last_injected_hash: None,
            staging_root,
            active_staging_dirs: Vec::new(),
            next_transfer_id: 1,
            inbound_transfers: HashMap::new(),
        }
    }

    /// Process a newly observed local clipboard snapshot.
    /// Returns `Some((seq, hash, payload))` if it is genuinely new local content
    /// that should be synchronized to peers (suppressing echo loops and duplicates).
    pub fn on_local_change(
        &mut self,
        payload: ClipboardPayload,
        cfg: &ClipboardSettings,
    ) -> Option<(u64, [u8; 32], ClipboardPayload)> {
        if !cfg.enabled {
            return None;
        }
        let max_payload_bytes = (cfg.max_payload_mb as usize) * 1024 * 1024;
        let filtered = match payload {
            ClipboardPayload::Text { plain, html, rtf } => {
                if !cfg.sync_text {
                    return None;
                }
                let html = if cfg.sync_rich_text { html } else { None };
                let rtf = if cfg.sync_rich_text { rtf } else { None };
                let total = plain.len()
                    + html.as_ref().map(|s| s.len()).unwrap_or(0)
                    + rtf.as_ref().map(|s| s.len()).unwrap_or(0);
                if total > max_payload_bytes || plain.is_empty() && html.is_none() && rtf.is_none()
                {
                    return None;
                }
                ClipboardPayload::Text { plain, html, rtf }
            }
            ClipboardPayload::Image {
                png_bytes,
                width,
                height,
            } => {
                if !cfg.sync_images || png_bytes.len() > max_payload_bytes || png_bytes.is_empty() {
                    return None;
                }
                ClipboardPayload::Image {
                    png_bytes,
                    width,
                    height,
                }
            }
            ClipboardPayload::Files { paths } => {
                if !cfg.sync_files || paths.is_empty() {
                    return None;
                }
                ClipboardPayload::Files { paths }
            }
        };

        let hash = filtered.content_hash();
        if self.last_injected_hash == Some(hash) || self.last_local_hash == Some(hash) {
            self.last_local_hash = Some(hash);
            return None;
        }
        self.last_local_hash = Some(hash);
        self.lamport_seq = self.lamport_seq.max(self.latest_version.0) + 1;
        self.latest_version = (self.lamport_seq, self.local_id.clone());
        Some((self.lamport_seq, hash, filtered))
    }

    pub fn alloc_transfer_id(&mut self) -> u64 {
        let id = self.next_transfer_id;
        self.next_transfer_id += 1;
        id
    }

    /// Build outbound file-transfer manifest + chunked payloads for `Files` clipboard.
    pub fn prepare_files_offer(
        &mut self,
        seq: u64,
        paths: &[PathBuf],
        cfg: &ClipboardSettings,
    ) -> Result<(Message, Vec<(u32, PathBuf)>)> {
        let max_bytes = (cfg.max_file_mb as u64) * 1024 * 1024;
        let mut entries = Vec::new();
        let mut sources = Vec::new();
        let mut total_bytes: u64 = 0;

        for root in paths {
            let root_name = root
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow::anyhow!("invalid file name in clipboard"))?;
            collect_entries(
                root,
                root_name,
                &mut entries,
                &mut sources,
                &mut total_bytes,
                max_bytes,
            )?;
        }

        if entries.is_empty() {
            anyhow::bail!("no transferable files in clipboard");
        }

        let mut bundle_ctx = DigestCtx::new(&SHA256);
        for e in &entries {
            bundle_ctx.update(e.rel_path.as_bytes());
            bundle_ctx.update(&e.size.to_be_bytes());
            bundle_ctx.update(&[u8::from(e.is_dir)]);
            bundle_ctx.update(&e.sha256);
        }
        let d = bundle_ctx.finish();
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(d.as_ref());

        let transfer_id = self.alloc_transfer_id();
        let msg = Message::ClipboardFilesOffer {
            transfer_id,
            seq,
            origin: self.local_id.clone(),
            sha256,
            total_bytes,
            entries,
        };
        Ok((msg, sources))
    }

    /// Handle an incoming `ClipboardText` message. Returns `Some(payload)` when
    /// it should be written into the local OS clipboard.
    pub fn on_remote_text(
        &mut self,
        seq: u64,
        origin: DeviceId,
        sha256: [u8; 32],
        plain: String,
        html: Option<String>,
        rtf: Option<String>,
        cfg: &ClipboardSettings,
    ) -> Option<ClipboardPayload> {
        if !cfg.enabled || !cfg.sync_text {
            return None;
        }
        let max_bytes = (cfg.max_payload_mb as usize) * 1024 * 1024;
        let total = plain.len()
            + html.as_ref().map(|s| s.len()).unwrap_or(0)
            + rtf.as_ref().map(|s| s.len()).unwrap_or(0);
        if total > max_bytes {
            tracing::warn!(total, max_bytes, "dropped oversized remote clipboard text");
            return None;
        }
        if !self.accept_version(seq, &origin) {
            return None;
        }
        let payload = ClipboardPayload::Text {
            plain,
            html: if cfg.sync_rich_text { html } else { None },
            rtf: if cfg.sync_rich_text { rtf } else { None },
        };
        let computed = payload.content_hash();
        if computed != sha256 {
            tracing::warn!("remote clipboard text checksum mismatch; ignoring");
            return None;
        }
        if self.last_local_hash == Some(computed) {
            return None;
        }
        self.last_injected_hash = Some(computed);
        self.last_local_hash = Some(computed);
        Some(payload)
    }

    pub fn on_remote_image_offer(
        &mut self,
        transfer_id: u64,
        seq: u64,
        origin: DeviceId,
        sha256: [u8; 32],
        total_bytes: u64,
        width: u32,
        height: u32,
        cfg: &ClipboardSettings,
    ) -> bool {
        if !cfg.enabled || !cfg.sync_images {
            return false;
        }
        let max_bytes = (cfg.max_payload_mb as u64) * 1024 * 1024;
        if total_bytes == 0 || total_bytes > max_bytes {
            return false;
        }
        if !self.accept_version(seq, &origin) {
            return false;
        }
        if self.last_local_hash == Some(sha256) {
            return false;
        }
        self.inbound_transfers.insert(
            transfer_id,
            InboundTransfer {
                seq,
                origin,
                expected_sha256: sha256,
                received_bytes: 0,
                max_bytes,
                kind: InboundKind::Image {
                    width,
                    height,
                    total_bytes,
                    buf: Vec::with_capacity((total_bytes as usize).min(4 * 1024 * 1024)),
                },
            },
        );
        true
    }

    pub fn on_remote_files_offer(
        &mut self,
        transfer_id: u64,
        seq: u64,
        origin: DeviceId,
        sha256: [u8; 32],
        total_bytes: u64,
        entries: Vec<SharedFileEntry>,
        cfg: &ClipboardSettings,
    ) -> Result<bool> {
        if !cfg.enabled || !cfg.sync_files {
            return Ok(false);
        }
        let max_bytes = (cfg.max_file_mb as u64) * 1024 * 1024;
        if total_bytes > max_bytes || entries.is_empty() || entries.len() > MAX_FILES_PER_CLIPBOARD
        {
            return Ok(false);
        }
        for e in &entries {
            if let Err(reason) = validate_rel_path(&e.rel_path) {
                bail!("rejected unsafe clipboard file path {:?}: {reason}", e.rel_path);
            }
        }
        if !self.accept_version(seq, &origin) {
            return Ok(false);
        }

        // Remove older ephemeral staged clipboard files before creating a new batch.
        self.clear_old_staging();
        let staging_dir = self
            .staging_root
            .join(format!("clip-{}-{}", origin.short(), transfer_id));
        create_private_dir(&staging_dir)?;

        // Pre-create directory entries.
        for e in &entries {
            if e.is_dir {
                let target = staging_dir.join(&e.rel_path);
                fs::create_dir_all(&target)?;
            }
        }
        self.active_staging_dirs.push(staging_dir.clone());

        self.inbound_transfers.insert(
            transfer_id,
            InboundTransfer {
                seq,
                origin,
                expected_sha256: sha256,
                received_bytes: 0,
                max_bytes,
                kind: InboundKind::Files {
                    staging_dir,
                    entries,
                    total_bytes,
                    open_file: None,
                    completed_hashes: HashMap::new(),
                },
            },
        );
        Ok(true)
    }

    /// Consume an inbound binary chunk. Returns `Ok(Some(payload))` once the entire
    /// image or file bundle completes and passes SHA-256 verification.
    pub fn on_data_chunk(&mut self, chunk: BinaryChunk) -> Result<Option<ClipboardPayload>> {
        let Some(xfer) = self.inbound_transfers.get_mut(&chunk.transfer_id) else {
            return Ok(None);
        };
        xfer.received_bytes = xfer.received_bytes.saturating_add(chunk.data.len() as u64);
        if xfer.received_bytes > xfer.max_bytes {
            self.inbound_transfers.remove(&chunk.transfer_id);
            bail!("transfer {} exceeded configured size limit", chunk.transfer_id);
        }

        match &mut xfer.kind {
            InboundKind::Image {
                width: _,
                height: _,
                total_bytes,
                buf,
            } => {
                buf.extend_from_slice(&chunk.data);
                if chunk.is_last || buf.len() as u64 >= *total_bytes {
                    let xfer = self.inbound_transfers.remove(&chunk.transfer_id).unwrap();
                    let InboundKind::Image {
                        width, height, buf, ..
                    } = xfer.kind
                    else {
                        unreachable!();
                    };
                    let payload = ClipboardPayload::Image {
                        png_bytes: buf,
                        width,
                        height,
                    };
                    let actual = payload.content_hash();
                    if actual != xfer.expected_sha256 {
                        bail!("clipboard image SHA-256 verification failed");
                    }
                    self.last_injected_hash = Some(actual);
                    self.last_local_hash = Some(actual);
                    self.latest_version = (xfer.seq, xfer.origin);
                    return Ok(Some(payload));
                }
                Ok(None)
            }
            InboundKind::Files {
                staging_dir,
                entries,
                total_bytes: _,
                open_file,
                completed_hashes,
            } => {
                let entry = entries
                    .get(chunk.file_index as usize)
                    .ok_or_else(|| anyhow::anyhow!("invalid file_index {}", chunk.file_index))?
                    .clone();
                if entry.is_dir {
                    return Ok(None);
                }

                let needs_open = match open_file {
                    Some((idx, _, _)) => *idx != chunk.file_index,
                    None => true,
                };
                if needs_open {
                    if let Some((prev_idx, f, ctx)) = open_file.take() {
                        drop(f);
                        let d = ctx.finish();
                        let mut h = [0u8; 32];
                        h.copy_from_slice(d.as_ref());
                        completed_hashes.insert(prev_idx, h);
                    }
                    let target = staging_dir.join(&entry.rel_path);
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    let f = File::create(&target)?;
                    *open_file = Some((chunk.file_index, f, DigestCtx::new(&SHA256)));
                }

                if let Some((_, f, ctx)) = open_file.as_mut() {
                    if !chunk.data.is_empty() {
                        f.write_all(&chunk.data)?;
                        ctx.update(&chunk.data);
                    }
                }

                if chunk.is_last {
                    if let Some((prev_idx, mut f, ctx)) = open_file.take() {
                        f.flush()?;
                        drop(f);
                        let d = ctx.finish();
                        let mut h = [0u8; 32];
                        h.copy_from_slice(d.as_ref());
                        completed_hashes.insert(prev_idx, h);
                    }
                    // Verify every file entry hash!
                    for (idx, e) in entries.iter().enumerate() {
                        if e.is_dir {
                            continue;
                        }
                        if e.size == 0 {
                            let target = staging_dir.join(&e.rel_path);
                            if !target.exists() {
                                if let Some(p) = target.parent() {
                                    fs::create_dir_all(p)?;
                                }
                                File::create(&target)?;
                            }
                            let empty_hash = sha256_bytes(&[]);
                            if e.sha256 != empty_hash {
                                bail!("empty file hash mismatch for {}", e.rel_path);
                            }
                            continue;
                        }
                        let Some(actual) = completed_hashes.get(&(idx as u32)) else {
                            bail!("missing file data for {}", e.rel_path);
                        };
                        if actual != &e.sha256 {
                            bail!("file SHA-256 verification failed for {}", e.rel_path);
                        }
                    }

                    let xfer = self.inbound_transfers.remove(&chunk.transfer_id).unwrap();
                    let InboundKind::Files {
                        staging_dir,
                        entries,
                        ..
                    } = xfer.kind
                    else {
                        unreachable!();
                    };

                    // Collect top-level staged paths for OS clipboard (`CF_HDROP` / `NSURL` / `uri-list`).
                    let mut top_names: Vec<PathBuf> = Vec::new();
                    for e in &entries {
                        let top = e.rel_path.split('/').next().unwrap_or(&e.rel_path);
                        let p = staging_dir.join(top);
                        if !top_names.contains(&p) {
                            top_names.push(p);
                        }
                    }
                    let payload = ClipboardPayload::Files { paths: top_names };
                    let h = payload.content_hash();
                    self.last_injected_hash = Some(h);
                    self.last_local_hash = Some(h);
                    self.latest_version = (xfer.seq, xfer.origin);
                    return Ok(Some(payload));
                }
                Ok(None)
            }
        }
    }

    pub fn clear_old_staging(&mut self) {
        for dir in self.active_staging_dirs.drain(..) {
            let _ = fs::remove_dir_all(dir);
        }
    }

    pub fn clear_all_cache(&mut self) {
        self.clear_old_staging();
        let _ = fs::remove_dir_all(&self.staging_root);
        let _ = create_private_dir(&self.staging_root);
    }

    fn accept_version(&mut self, seq: u64, origin: &DeviceId) -> bool {
        let candidate = (seq, origin.clone());
        if candidate < self.latest_version {
            return false;
        }
        self.lamport_seq = self.lamport_seq.max(seq);
        self.latest_version = candidate;
        true
    }
}

impl Drop for ClipboardSyncCoordinator {
    fn drop(&mut self) {
        self.clear_old_staging();
    }
}

fn collect_entries(
    path: &Path,
    rel_path: &str,
    entries: &mut Vec<SharedFileEntry>,
    sources: &mut Vec<(u32, PathBuf)>,
    total_bytes: &mut u64,
    max_bytes: u64,
) -> Result<()> {
    if entries.len() >= MAX_FILES_PER_CLIPBOARD {
        bail!("clipboard file count exceeds maximum ({MAX_FILES_PER_CLIPBOARD})");
    }
    let meta = fs::symlink_metadata(path)
        .with_context(|| format!("inspecting clipboard path {}", path.display()))?;
    if meta.file_type().is_symlink() {
        bail!("skipping symlink in clipboard transfer: {}", path.display());
    }
    let norm_rel = rel_path.replace('\\', "/");
    validate_rel_path(&norm_rel).map_err(|e| anyhow::anyhow!("{e}"))?;

    if meta.is_dir() {
        entries.push(SharedFileEntry {
            rel_path: norm_rel.clone(),
            size: 0,
            is_dir: true,
            sha256: [0u8; 32],
        });
        for child in fs::read_dir(path)? {
            let child = child?;
            let name = child.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let child_rel = format!("{norm_rel}/{name_str}");
            collect_entries(
                &child.path(),
                &child_rel,
                entries,
                sources,
                total_bytes,
                max_bytes,
            )?;
        }
    } else if meta.is_file() {
        let size = meta.len();
        *total_bytes = total_bytes.saturating_add(size);
        if *total_bytes > max_bytes {
            bail!("copied files exceed maximum transfer limit ({max_bytes} bytes)");
        }
        let hash = hash_file_sha256(path)?;
        let idx = entries.len() as u32;
        entries.push(SharedFileEntry {
            rel_path: norm_rel,
            size,
            is_dir: false,
            sha256: hash,
        });
        sources.push((idx, path.to_path_buf()));
    }
    Ok(())
}

fn hash_file_sha256(path: &Path) -> Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut ctx = DigestCtx::new(&SHA256);
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        ctx.update(&buf[..n]);
    }
    let d = ctx.finish();
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    Ok(out)
}

pub fn build_chunks_for_bytes(transfer_id: u64, bytes: &[u8]) -> Vec<BinaryChunk> {
    if bytes.is_empty() {
        return vec![BinaryChunk {
            transfer_id,
            file_index: 0,
            offset: 0,
            is_last: true,
            data: Vec::new(),
        }];
    }
    let total_chunks = bytes.len().div_ceil(CHUNK_SIZE);
    bytes
        .chunks(CHUNK_SIZE)
        .enumerate()
        .map(|(i, slice)| BinaryChunk {
            transfer_id,
            file_index: 0,
            offset: (i * CHUNK_SIZE) as u64,
            is_last: i + 1 == total_chunks,
            data: slice.to_vec(),
        })
        .collect()
}

pub fn create_private_dir(dir: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

pub type SharedClipboardCoordinator = Arc<Mutex<ClipboardSyncCoordinator>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suppresses_clipboard_loops_and_resolves_conflicts() {
        let dev_a = DeviceId::from("aaaa");
        let dev_b = DeviceId::from("bbbb");
        let root_a = std::env::temp_dir().join(format!("mf-clip-a-{}", uuid::Uuid::new_v4()));
        let root_b = std::env::temp_dir().join(format!("mf-clip-b-{}", uuid::Uuid::new_v4()));
        let mut a = ClipboardSyncCoordinator::new(dev_a.clone(), root_a.clone());
        let mut b = ClipboardSyncCoordinator::new(dev_b.clone(), root_b.clone());
        let cfg = ClipboardSettings::default();

        // User copies text on A
        let p = ClipboardPayload::Text {
            plain: "hello 🌍".into(),
            html: Some("<b>hello</b>".into()),
            rtf: None,
        };
        let (seq, hash, out) = a.on_local_change(p.clone(), &cfg).unwrap();

        // Duplicate local poll returns None
        assert!(a.on_local_change(p.clone(), &cfg).is_none());

        // B receives it
        let ClipboardPayload::Text { plain, html, rtf } = out else {
            panic!()
        };
        let to_inject = b
            .on_remote_text(seq, dev_a.clone(), hash, plain, html, rtf, &cfg)
            .unwrap();

        // B's local clipboard watcher detects the just-injected content -> must NOT echo back!
        assert!(b.on_local_change(to_inject, &cfg).is_none());

        // Stale update with lower seq must be ignored
        let stale_hash = ClipboardPayload::Text {
            plain: "stale".into(),
            html: None,
            rtf: None,
        }
        .content_hash();
        assert!(b
            .on_remote_text(0, dev_a, stale_hash, "stale".into(), None, None, &cfg)
            .is_none());

        let _ = fs::remove_dir_all(root_a);
        let _ = fs::remove_dir_all(root_b);
    }

    #[test]
    fn transfers_and_verifies_files_safely() {
        let src_dir = std::env::temp_dir().join(format!("mf-src-{}", uuid::Uuid::new_v4()));
        let stage_b = std::env::temp_dir().join(format!("mf-stage-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(src_dir.join("sub")).unwrap();
        let file_path = src_dir.join("sub/notes.txt");
        fs::write(&file_path, b"cross-platform file payload").unwrap();

        let dev_a = DeviceId::from("aaaa");
        let dev_b = DeviceId::from("bbbb");
        let mut a = ClipboardSyncCoordinator::new(dev_a.clone(), src_dir.join("unused"));
        let mut b = ClipboardSyncCoordinator::new(dev_b, stage_b.clone());
        let cfg = ClipboardSettings::default();

        let (seq, _, payload) = a
            .on_local_change(
                ClipboardPayload::Files {
                    paths: vec![src_dir.join("sub")],
                },
                &cfg,
            )
            .unwrap();
        let ClipboardPayload::Files { paths } = payload else {
            panic!()
        };
        let (offer, sources) = a.prepare_files_offer(seq, &paths, &cfg).unwrap();
        let Message::ClipboardFilesOffer {
            transfer_id,
            seq,
            origin,
            sha256,
            total_bytes,
            entries,
        } = offer
        else {
            panic!()
        };

        assert!(b
            .on_remote_files_offer(transfer_id, seq, origin, sha256, total_bytes, entries, &cfg)
            .unwrap());

        for (i, (file_idx, src)) in sources.iter().enumerate() {
            let bytes = fs::read(src).unwrap();
            let res = b
                .on_data_chunk(BinaryChunk {
                    transfer_id,
                    file_index: *file_idx,
                    offset: 0,
                    is_last: i + 1 == sources.len(),
                    data: bytes,
                })
                .unwrap();
            if i + 1 == sources.len() {
                let Some(ClipboardPayload::Files { paths }) = res else {
                    panic!("expected completed files payload");
                };
                assert_eq!(paths.len(), 1);
                let copied = fs::read(paths[0].join("notes.txt")).unwrap();
                assert_eq!(copied, b"cross-platform file payload");
            }
        }

        b.clear_all_cache();
        let _ = fs::remove_dir_all(src_dir);
        let _ = fs::remove_dir_all(stage_b);
    }
}
