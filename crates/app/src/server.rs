//! Server role: accept client connections, run the handshake, and (for now)
//! route a scripted demo stream. Real input capture + edge switching arrive in
//! later milestones; the connection plumbing established here is what they build on.

use std::time::Duration;

use anyhow::{Context, Result};
use mf_protocol::{read_frame, write_frame, Button, Key, Message, PROTOCOL_VERSION};
use tokio::net::{TcpListener, TcpStream};

use crate::config::Config;

pub async fn run(cfg: Config, demo: bool) -> Result<()> {
    let layout = cfg.layout();
    let listener = TcpListener::bind(&cfg.server.bind)
        .await
        .with_context(|| format!("binding {}", cfg.server.bind))?;
    tracing::info!(addr = %cfg.server.bind, screens = layout.screens.len(), "server listening");

    loop {
        let (stream, peer) = listener.accept().await?;
        let cfg = cfg.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, cfg, demo).await {
                tracing::warn!(%peer, error = %e, "client connection ended");
            } else {
                tracing::info!(%peer, "client disconnected");
            }
        });
    }
}

async fn handle_client(mut stream: TcpStream, cfg: Config, demo: bool) -> Result<()> {
    let peer = stream.peer_addr().ok();
    stream.set_nodelay(true).ok(); // input latency matters more than throughput

    // --- Handshake: expect Hello, validate, reply Welcome/Reject. ---
    let hello = match read_frame(&mut stream).await? {
        Some(Message::Hello { name, width, height, token, version }) => {
            (name, width, height, token, version)
        }
        Some(other) => {
            anyhow::bail!("expected Hello, got {:?}", other);
        }
        None => return Ok(()), // connected then closed before saying anything
    };
    let (name, width, height, token, version) = hello;

    if version != PROTOCOL_VERSION {
        reject(&mut stream, format!(
            "protocol version mismatch: server {PROTOCOL_VERSION}, client {version}"
        )).await?;
        return Ok(());
    }
    if token != cfg.server.token {
        reject(&mut stream, "invalid token".into()).await?;
        return Ok(());
    }
    let layout = cfg.layout();
    let Some(screen) = layout.screens.iter().find(|s| s.name == name) else {
        reject(&mut stream, format!("unknown screen name {name:?}")).await?;
        return Ok(());
    };

    write_frame(&mut stream, &Message::Welcome { position: screen.position }).await?;
    tracing::info!(
        ?peer, name, width, height, position = ?screen.position,
        "client accepted"
    );

    // --- Steady state: heartbeat + (optionally) a scripted demo stream. ---
    let (mut reader, mut writer) = stream.into_split();

    let demo_task = demo.then(|| {
        tokio::spawn(async move {
            if let Err(e) = run_demo(&mut writer).await {
                tracing::warn!(error = %e, "demo stream ended");
            }
        })
    });

    while let Some(msg) = read_frame(&mut reader).await? {
        match msg {
            Message::Heartbeat => tracing::debug!(?peer, "heartbeat"),
            other => tracing::info!(?peer, ?other, "from client"),
        }
    }

    if let Some(task) = demo_task {
        task.abort();
    }
    Ok(())
}

async fn reject<W>(stream: &mut W, reason: String) -> Result<()>
where
    W: tokio::io::AsyncWrite + Unpin,
{
    tracing::warn!(%reason, "rejecting client");
    write_frame(stream, &Message::Reject { reason }).await?;
    Ok(())
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
