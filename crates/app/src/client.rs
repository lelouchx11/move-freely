//! Legacy v1-compatible client command (`mf connect ... [--echo]`),
//! backed by `mf-core` native input injection.

use std::time::Duration;

use anyhow::{bail, Result};
use mf_core::input::headless::HeadlessInjector;
use mf_core::input::{new_native_injector, PlatformInjector};
use mf_protocol::{
    read_frame, write_frame, ArchKind, ChannelRole, DeviceId, Message, Monitor, OsKind,
    PeerCapabilities, PROTOCOL_VERSION,
};
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
            device_id: DeviceId::from(opts.token.clone()),
            name: opts.name.clone(),
            os: OsKind::current(),
            arch: ArchKind::current(),
            version: PROTOCOL_VERSION,
            channel: ChannelRole::Control,
            session_nonce: 1,
            monitors: vec![Monitor {
                id: "primary".into(),
                name: opts.name.clone(),
                x: 0,
                y: 0,
                width: opts.width,
                height: opts.height,
                physical_width: opts.width,
                physical_height: opts.height,
                scale_factor: 1.0,
                is_primary: true,
            }],
            capabilities: PeerCapabilities::default(),
        },
    )
    .await?;

    match read_frame(&mut stream).await? {
        Some(Message::Welcome { name, .. }) => {
            tracing::info!(server = %name, "accepted by server");
        }
        Some(Message::Reject { reason }) => bail!("server rejected connection: {reason}"),
        Some(other) => bail!("expected Welcome/Reject, got {other:?}"),
        None => bail!("server closed connection during handshake"),
    }

    let mut injector: Box<dyn PlatformInjector> = if opts.echo {
        tracing::info!("echo mode: logging received events instead of injecting");
        Box::new(HeadlessInjector::new(true))
    } else {
        new_native_injector()?
    };

    let (mut reader, mut writer) = stream.into_split();

    let heartbeat = tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let mut seq = 0u64;
        loop {
            tick.tick().await;
            seq += 1;
            if write_frame(&mut writer, &Message::Heartbeat { seq, sent_ms: 0 })
                .await
                .is_err()
            {
                break;
            }
        }
    });

    while let Some(msg) = read_frame(&mut reader).await? {
        match msg {
            Message::Enter { x, y } => {
                if opts.echo {
                    tracing::info!(x, y, "ENTER");
                }
                let _ = injector.warp_cursor(x, y);
            }
            Message::Leave | Message::ReleaseAllInput => {
                if opts.echo {
                    tracing::info!("LEAVE");
                }
                let _ = injector.release_all();
            }
            Message::PointerMoveAbs { x, y, dx, dy } => {
                let _ = injector.inject_pointer_abs(x, y, dx, dy);
            }
            Message::PointerMoveRel { dx, dy } => {
                let _ = injector.inject_pointer_rel(dx, dy);
            }
            Message::MouseButton { button, pressed } => {
                let _ = injector.inject_button(button, pressed);
            }
            Message::Scroll { dx, dy, continuous } => {
                let _ = injector.inject_scroll(dx, dy, continuous);
            }
            Message::KeyEvent {
                key,
                pressed,
                text,
                modifiers,
            } => {
                let _ = injector.inject_key(key, pressed, text.as_deref(), modifiers);
            }
            Message::Heartbeat { .. } => {}
            other => tracing::debug!(?other, "ignored message"),
        }
    }

    let _ = injector.release_all();
    heartbeat.abort();
    tracing::info!("server closed connection");
    Ok(())
}
