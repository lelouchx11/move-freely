//! Client role: connect to a server, complete the handshake, and act on the
//! input events it streams. v1 supports an `--echo` mode that logs events; the
//! real OS injection backend lands in milestone 2.

use std::time::Duration;

use anyhow::{bail, Result};
use mf_protocol::{read_frame, write_frame, Message, PROTOCOL_VERSION};
use tokio::net::TcpStream;

pub struct ConnectOptions {
    pub addr: String,
    pub name: String,
    pub token: String,
    pub width: u32,
    pub height: u32,
    pub echo: bool,
}

pub async fn run(opts: ConnectOptions) -> Result<()> {
    let mut stream = TcpStream::connect(&opts.addr).await?;
    stream.set_nodelay(true).ok();
    tracing::info!(addr = %opts.addr, name = %opts.name, "connected, sending hello");

    write_frame(
        &mut stream,
        &Message::Hello {
            name: opts.name.clone(),
            width: opts.width,
            height: opts.height,
            token: opts.token.clone(),
            version: PROTOCOL_VERSION,
        },
    )
    .await?;

    match read_frame(&mut stream).await? {
        Some(Message::Welcome { position }) => {
            tracing::info!(?position, "accepted by server");
        }
        Some(Message::Reject { reason }) => bail!("server rejected connection: {reason}"),
        Some(other) => bail!("expected Welcome/Reject, got {other:?}"),
        None => bail!("server closed connection during handshake"),
    }

    if !opts.echo {
        tracing::warn!(
            "no input backend yet (milestone 2) — run with --echo to log received events"
        );
    }

    let (mut reader, mut writer) = stream.into_split();

    // Background heartbeat so the server can detect a dead client.
    let heartbeat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        loop {
            tick.tick().await;
            if write_frame(&mut writer, &Message::Heartbeat).await.is_err() {
                break;
            }
        }
    });

    let mut active = false; // whether focus currently sits on this screen
    while let Some(msg) = read_frame(&mut reader).await? {
        match msg {
            Message::Enter { x, y } => {
                active = true;
                log_event(opts.echo, format!("ENTER at ({x}, {y}) — focus now here"));
            }
            Message::Leave => {
                active = false;
                log_event(opts.echo, "LEAVE — focus left this screen".into());
            }
            Message::MouseMoveRel { dx, dy } => {
                log_event(opts.echo, format!("mouse move rel ({dx}, {dy}) active={active}"));
            }
            Message::MouseButton { button, pressed } => {
                log_event(opts.echo, format!("mouse button {button:?} pressed={pressed}"));
            }
            Message::Scroll { dx, dy } => {
                log_event(opts.echo, format!("scroll ({dx}, {dy})"));
            }
            Message::KeyEvent { key, pressed } => {
                log_event(opts.echo, format!("key {key:?} pressed={pressed}"));
            }
            Message::Heartbeat => tracing::debug!("server heartbeat"),
            other => tracing::warn!(?other, "unexpected message from server"),
        }
    }

    heartbeat.abort();
    tracing::info!("server closed connection");
    Ok(())
}

/// In `--echo` mode, surface events at info level; otherwise they would be
/// injected by the (not-yet-implemented) OS backend, so just trace them.
fn log_event(echo: bool, what: String) {
    if echo {
        tracing::info!("{what}");
    } else {
        tracing::debug!("{what}");
    }
}
