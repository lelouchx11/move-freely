//! Client role: connect to a server, complete the handshake, and act on the
//! input events it streams. With `--echo` it logs events instead of injecting
//! them (no OS permissions needed); otherwise it injects via the OS backend.

use std::time::Duration;

use anyhow::{bail, Result};
use mf_protocol::{read_frame, write_frame, Message, PROTOCOL_VERSION};
use tokio::net::TcpStream;

use crate::input::{new_injector, InputEvent, Injector};

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

    // In echo mode we only log; otherwise we build an OS injector up front so a
    // permission failure surfaces immediately rather than on the first event.
    let mut injector: Option<Box<dyn Injector>> = if opts.echo {
        tracing::info!("echo mode: logging received events instead of injecting");
        None
    } else {
        Some(new_injector()?)
    };

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
        // Control messages adjust focus; input messages map to an InputEvent.
        let event = match msg {
            Message::Enter { x, y } => {
                active = true;
                log_event(opts.echo, format!("ENTER at ({x}, {y}) — focus now here"));
                continue;
            }
            Message::Leave => {
                active = false;
                log_event(opts.echo, "LEAVE — focus left this screen".into());
                continue;
            }
            Message::MouseMoveRel { dx, dy } => InputEvent::MouseMoveRel { dx, dy },
            Message::MouseButton { button, pressed } => {
                InputEvent::MouseButton { button, pressed }
            }
            Message::Scroll { dx, dy } => InputEvent::Scroll { dx, dy },
            Message::KeyEvent { key, pressed } => InputEvent::Key { key, pressed },
            Message::Heartbeat => {
                tracing::debug!("server heartbeat");
                continue;
            }
            other => {
                tracing::warn!(?other, "unexpected message from server");
                continue;
            }
        };

        match injector.as_mut() {
            Some(inj) => {
                if let Err(e) = inj.inject(&event) {
                    tracing::warn!(error = %e, ?event, "injection failed");
                }
            }
            None => log_event(opts.echo, format!("{event:?} active={active}")),
        }
    }

    heartbeat.abort();
    tracing::info!("server closed connection");
    Ok(())
}

/// Surface an event in echo mode at info level (otherwise trace).
fn log_event(echo: bool, what: String) {
    if echo {
        tracing::info!("{what}");
    } else {
        tracing::debug!("{what}");
    }
}
