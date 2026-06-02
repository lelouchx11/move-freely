//! Server role: own the physical keyboard + mouse, capture local input, and
//! route it to whichever screen currently has focus.
//!
//! Focus starts on the local (`Center`) screen. Pressing the hotkey ([`HOTKEY`])
//! cycles focus through the connected remote clients and back to local. While a
//! remote screen has focus the capture layer grabs (swallows) local input and we
//! forward it to that client; on the local screen input passes through untouched.
//!
//! `--demo` skips capture entirely and replays a scripted stream per connection,
//! so the wire path can be exercised without any OS permissions.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use mf_protocol::{read_frame, write_frame, Button, Key, Message, Position, PROTOCOL_VERSION};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::config::Config;
use crate::input::{capture, InputEvent};

/// Key that cycles focus between the local screen and connected clients.
const HOTKEY: rdev::Key = rdev::Key::F12;

/// A connected client we can route input to.
struct ClientHandle {
    tx: mpsc::UnboundedSender<Message>,
    width: u32,
    height: u32,
    name: String,
}

type SharedClients = Arc<Mutex<HashMap<Position, ClientHandle>>>;

pub async fn run(cfg: Config, demo: bool) -> Result<()> {
    let layout = cfg.layout();
    let listener = TcpListener::bind(&cfg.server.bind)
        .await
        .with_context(|| format!("binding {}", cfg.server.bind))?;
    tracing::info!(addr = %cfg.server.bind, screens = layout.screens.len(), "server listening");

    let clients: SharedClients = Arc::new(Mutex::new(HashMap::new()));

    if !demo {
        match capture::start(HOTKEY) {
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

/// Per-connection handler: handshake, then either the demo stream or register +
/// serve the client until it disconnects.
async fn handle_client(
    mut stream: TcpStream,
    cfg: Config,
    demo: bool,
    clients: SharedClients,
) -> Result<()> {
    let peer = stream.peer_addr().ok();
    stream.set_nodelay(true).ok();

    let (name, width, height) = match handshake(&mut stream, &cfg).await? {
        Some(accepted) => accepted,
        None => return Ok(()), // rejected or closed; handshake already replied
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

    // Register a channel the router can push routed input into; a writer task
    // drains it to the socket.
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    clients.lock().unwrap().insert(
        position,
        ClientHandle { tx, width, height, name: name.clone() },
    );

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if write_frame(&mut writer, &msg).await.is_err() {
                break;
            }
        }
    });

    // Read loop: clients only send heartbeats today.
    let read_result = async {
        while let Some(msg) = read_frame(&mut reader).await? {
            match msg {
                Message::Heartbeat => tracing::debug!(?peer, "heartbeat"),
                other => tracing::info!(?peer, ?other, "from client"),
            }
        }
        Ok::<_, anyhow::Error>(())
    }
    .await;

    clients.lock().unwrap().remove(&position);
    writer_task.abort();
    read_result
}

/// Read the client's `Hello`, validate it, and reply `Welcome`/`Reject`.
/// Returns the accepted `(name, width, height)` or `None` if rejected/closed.
async fn handshake(stream: &mut TcpStream, cfg: &Config) -> Result<Option<(String, u32, u32)>> {
    let (name, width, height, token, version) = match read_frame(stream).await? {
        Some(Message::Hello { name, width, height, token, version }) => {
            (name, width, height, token, version)
        }
        Some(other) => anyhow::bail!("expected Hello, got {other:?}"),
        None => return Ok(None),
    };

    if version != PROTOCOL_VERSION {
        reject(stream, format!(
            "protocol version mismatch: server {PROTOCOL_VERSION}, client {version}"
        )).await?;
        return Ok(None);
    }
    if token != cfg.server.token {
        reject(stream, "invalid token".into()).await?;
        return Ok(None);
    }
    let layout = cfg.layout();
    let Some(screen) = layout.screens.iter().find(|s| s.name == name) else {
        reject(stream, format!("unknown screen name {name:?}")).await?;
        return Ok(None);
    };

    write_frame(stream, &Message::Welcome { position: screen.position }).await?;
    Ok(Some((name, width, height)))
}

async fn reject(stream: &mut TcpStream, reason: String) -> Result<()> {
    tracing::warn!(%reason, "rejecting client");
    write_frame(stream, &Message::Reject { reason }).await?;
    Ok(())
}

/// Consume captured input events and route them to the active screen.
async fn route_input(cap: capture::Capture, clients: SharedClients) {
    let grab = cap.grab_handle();
    let mut events = cap.events;
    let mut active = Position::Center; // start on the local screen

    while let Some(event) = events.recv().await {
        match event {
            InputEvent::ToggleFocus => {
                active = switch_focus(active, &clients, &grab);
            }
            other if active != Position::Center => {
                let Some(msg) = other.to_message() else { continue };
                let mut guard = clients.lock().unwrap();
                let dead = match guard.get(&active) {
                    Some(h) => h.tx.send(msg).is_err(),
                    None => true,
                };
                if dead {
                    // The active client vanished; fall back to local control.
                    guard.remove(&active);
                    drop(guard);
                    grab.set(false);
                    tracing::warn!(?active, "active client gone — focus returned to local");
                    active = Position::Center;
                }
            }
            _ => {} // input while local-focused never reaches here (passed through)
        }
    }
}

/// Advance focus to the next connected screen in the ring `Center -> Left ->
/// Right -> Center`, skipping disconnected ones. Sends `Leave`/`Enter` and
/// toggles grabbing as needed. Returns the new active position.
fn switch_focus(
    current: Position,
    clients: &SharedClients,
    grab: &capture::GrabHandle,
) -> Position {
    let guard = clients.lock().unwrap();

    // Build the ring from what's actually connected.
    let mut ring = vec![Position::Center];
    for pos in [Position::Left, Position::Right] {
        if guard.contains_key(&pos) {
            ring.push(pos);
        }
    }
    let idx = ring.iter().position(|&p| p == current).unwrap_or(0);
    let next = ring[(idx + 1) % ring.len()];
    if next == current {
        return current; // nothing else connected
    }

    // Leaving a remote screen: tell it focus is gone.
    if current != Position::Center {
        if let Some(h) = guard.get(&current) {
            let _ = h.tx.send(Message::Leave);
        }
    }

    match next {
        Position::Center => {
            grab.set(false);
            tracing::info!("focus → local screen");
        }
        remote => {
            if let Some(h) = guard.get(&remote) {
                grab.set(true);
                let (x, y) = (h.width as i32 / 2, h.height as i32 / 2);
                let _ = h.tx.send(Message::Enter { x, y });
                tracing::info!(name = %h.name, ?remote, "focus → remote screen");
            }
        }
    }
    next
}

/// Send a fixed sequence of input events so a client running `--echo` can prove
/// the full handshake → framing → routing pipe without any OS capture backend.
async fn run_demo<W>(writer: &mut W) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    tokio::time::sleep(Duration::from_millis(300)).await;
    let script = [
        Message::Enter { x: 0, y: 100 },
        Message::MouseMoveRel { dx: 10, dy: 0 },
        Message::MouseMoveRel { dx: 0, dy: 10 },
        Message::MouseButton { button: Button::Left, pressed: true },
        Message::MouseButton { button: Button::Left, pressed: false },
        Message::KeyEvent { key: Key::Char('h'), pressed: true },
        Message::KeyEvent { key: Key::Char('h'), pressed: false },
        Message::KeyEvent { key: Key::Char('i'), pressed: true },
        Message::KeyEvent { key: Key::Char('i'), pressed: false },
        Message::Scroll { dx: 0, dy: -2 },
        Message::Leave,
    ];
    for msg in script {
        write_frame(writer, &msg).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    tracing::info!("demo stream complete");
    Ok(())
}
