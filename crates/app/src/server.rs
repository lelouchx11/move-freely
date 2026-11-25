//! Legacy v1-compatible server command (`mf serve --config ... [--demo]`),
//! backed by `mf-core` native capture and `mf-protocol` v2 wire frames.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use mf_core::config::LegacyConfig;
use mf_core::input::{start_native_capture, InputEvent};
use mf_protocol::{
    read_frame, write_frame, ArchKind, Button, ChannelRole, DeviceId, KeyCode, Message,
    ModifierFlags, Monitor, OsKind, PeerCapabilities, Position, PROTOCOL_VERSION,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

struct ClientHandle {
    tx: mpsc::UnboundedSender<Message>,
    width: u32,
    height: u32,
    name: String,
}

type SharedClients = Arc<Mutex<HashMap<Position, ClientHandle>>>;

pub async fn run(cfg: LegacyConfig, demo: bool) -> Result<()> {
    let layout = cfg.layout();
    let listener = TcpListener::bind(&cfg.server.bind)
        .await
        .with_context(|| format!("binding {}", cfg.server.bind))?;
    tracing::info!(addr = %cfg.server.bind, screens = layout.screens.len(), "server listening");

    let clients: SharedClients = Arc::new(Mutex::new(HashMap::new()));

    if !demo {
        match start_native_capture() {
            Ok(cap) => {
                tracing::info!("input capture active — press F12 to switch the active screen");
                tokio::spawn(route_input(cap, clients.clone()));
            }
            Err(e) => tracing::error!(error = %e, "could not start input capture"),
        }
    }

    loop {
        let (stream, peer) = listener.accept().await?;
        let cfg = cfg.clone();
        let clients = clients.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, cfg, demo, clients).await {
                tracing::warn!(%peer, error = %e, "client connection ended");
            } else {
                tracing::info!(%peer, "client disconnected");
            }
        });
    }
}

async fn handle_client(
    mut stream: TcpStream,
    cfg: LegacyConfig,
    demo: bool,
    clients: SharedClients,
) -> Result<()> {
    let peer = stream.peer_addr().ok();
    stream.set_nodelay(true).ok();

    let (name, width, height) = match handshake(&mut stream, &cfg).await? {
        Some(accepted) => accepted,
        None => return Ok(()),
    };
    let position = cfg
        .layout()
        .screens
        .iter()
        .find(|s| s.name == name)
        .map(|s| s.position)
        .expect("handshake validated the name");
    tracing::info!(?peer, name, ?position, "client accepted");

    let (mut reader, mut writer) = stream.into_split();

    if demo {
        if let Err(e) = run_demo(&mut writer).await {
            tracing::warn!(error = %e, "demo stream ended");
        }
        while read_frame(&mut reader).await?.is_some() {}
        return Ok(());
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    clients.lock().unwrap().insert(
        position,
        ClientHandle {
            tx,
            width,
            height,
            name: name.clone(),
        },
    );

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    let read_result = async {
        while let Some(msg) = read_frame(&mut reader).await? {
            if let Message::Heartbeat { seq, sent_ms } = msg {
                tracing::debug!(?peer, seq, sent_ms, "heartbeat");
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    clients.lock().unwrap().remove(&position);
    writer_task.abort();
    read_result
}

async fn handshake(stream: &mut TcpStream, cfg: &LegacyConfig) -> Result<Option<(String, u32, u32)>> {
    let (name, width, height, token_matches, version) = match read_frame(stream).await? {
        Some(Message::Hello {
            name,
            version,
            monitors,
            device_id,
            ..
        }) => {
            let (w, h) = monitors
                .first()
                .map(|m| (m.width, m.height))
                .unwrap_or((1920, 1080));
            let ok = device_id.as_str() == cfg.server.token;
            (name, w, h, ok, version)
        }
        Some(other) => anyhow::bail!("expected Hello, got {other:?}"),
        None => return Ok(None),
    };

    if version != PROTOCOL_VERSION {
        reject(
            stream,
            format!("protocol version mismatch: server {PROTOCOL_VERSION}, client {version}"),
        )
        .await?;
        return Ok(None);
    }
    if !token_matches {
        reject(stream, "invalid token".into()).await?;
        return Ok(None);
    }
    let layout = cfg.layout();
    if !layout.screens.iter().any(|s| s.name == name) {
        reject(stream, format!("unknown screen name {name:?}")).await?;
        return Ok(None);
    }

    write_frame(
        stream,
        &Message::Welcome {
            device_id: DeviceId::from("legacy-server"),
            name: "server".into(),
            os: OsKind::current(),
            arch: ArchKind::current(),
            session_nonce: 1,
            monitors: vec![Monitor {
                id: "center".into(),
                name: "Center".into(),
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
                physical_width: 1920,
                physical_height: 1080,
                scale_factor: 1.0,
                is_primary: true,
            }],
            capabilities: PeerCapabilities::default(),
            active_controller: None,
        },
    )
    .await?;
    Ok(Some((name, width, height)))
}

async fn reject(stream: &mut TcpStream, reason: String) -> Result<()> {
    tracing::warn!(%reason, "rejecting client");
    write_frame(stream, &Message::Reject { reason }).await?;
    Ok(())
}

async fn route_input(mut cap: mf_core::input::CaptureSession, clients: SharedClients) {
    let ctrl = cap.controller;
    let mut active = Position::Center;

    while let Some(event) = cap.events.recv().await {
        match event {
            InputEvent::Key {
                key: KeyCode::F12,
                pressed: true,
                ..
            } => {
                active = switch_focus(active, &clients, &*ctrl);
            }
            other if active != Position::Center => {
                let msg = match other {
                    InputEvent::RemotePointerDelta { dx, dy } => {
                        Message::PointerMoveRel { dx, dy }
                    }
                    InputEvent::MouseButton { button, pressed } => {
                        Message::MouseButton { button, pressed }
                    }
                    InputEvent::Scroll { dx, dy, continuous } => {
                        Message::Scroll { dx, dy, continuous }
                    }
                    InputEvent::Key {
                        key,
                        pressed,
                        text,
                        modifiers,
                    } => Message::KeyEvent {
                        key,
                        pressed,
                        text,
                        modifiers,
                    },
                    _ => continue,
                };
                let mut guard = clients.lock().unwrap();
                let dead = match guard.get(&active) {
                    Some(h) => h.tx.send(msg).is_err(),
                    None => true,
                };
                if dead {
                    guard.remove(&active);
                    drop(guard);
                    ctrl.set_grabbing(false, None);
                    tracing::warn!(?active, "active client gone — focus returned to local");
                    active = Position::Center;
                }
            }
            _ => {}
        }
    }
}

fn switch_focus(
    current: Position,
    clients: &SharedClients,
    ctrl: &dyn mf_core::input::CaptureController,
) -> Position {
    let guard = clients.lock().unwrap();
    let mut ring = vec![Position::Center];
    for pos in [Position::Left, Position::Right] {
        if guard.contains_key(&pos) {
            ring.push(pos);
        }
    }
    let idx = ring.iter().position(|&p| p == current).unwrap_or(0);
    let next = ring[(idx + 1) % ring.len()];
    if next == current {
        return current;
    }

    if current != Position::Center {
        if let Some(h) = guard.get(&current) {
            let _ = h.tx.send(Message::Leave);
        }
    }

    match next {
        Position::Center => {
            ctrl.set_grabbing(false, None);
            tracing::info!("focus → local screen");
        }
        remote => {
            if let Some(h) = guard.get(&remote) {
                ctrl.set_grabbing(true, None);
                let _ = h.tx.send(Message::Enter {
                    x: h.width as i32 / 2,
                    y: h.height as i32 / 2,
                });
                tracing::info!(name = %h.name, ?remote, "focus → remote screen");
            }
        }
    }
    next
}

async fn run_demo<W>(writer: &mut W) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    tokio::time::sleep(Duration::from_millis(300)).await;
    let script = [
        Message::Enter { x: 0, y: 100 },
        Message::PointerMoveRel { dx: 10, dy: 0 },
        Message::PointerMoveRel { dx: 0, dy: 10 },
        Message::MouseButton {
            button: Button::Left,
            pressed: true,
        },
        Message::MouseButton {
            button: Button::Left,
            pressed: false,
        },
        Message::KeyEvent {
            key: KeyCode::KeyH,
            pressed: true,
            text: Some("h".into()),
            modifiers: ModifierFlags::default(),
        },
        Message::KeyEvent {
            key: KeyCode::KeyH,
            pressed: false,
            text: None,
            modifiers: ModifierFlags::default(),
        },
        Message::KeyEvent {
            key: KeyCode::KeyI,
            pressed: true,
            text: Some("i".into()),
            modifiers: ModifierFlags::default(),
        },
        Message::KeyEvent {
            key: KeyCode::KeyI,
            pressed: false,
            text: None,
            modifiers: ModifierFlags::default(),
        },
        Message::Scroll {
            dx: 0.0,
            dy: -2.0,
            continuous: false,
        },
        Message::Leave,
    ];
    for msg in script {
        write_frame(writer, &msg).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    tracing::info!("demo stream complete");
    Ok(())
}

// Keep ChannelRole referenced cleanly if needed by tests
#[allow(dead_code)]
fn _channel_role() -> ChannelRole {
    ChannelRole::Control
}
