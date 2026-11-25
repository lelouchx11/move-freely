//! `mf` — the move freely software KVM CLI & headless peer daemon.

mod client;
mod server;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use mf_core::{
    check_permissions, init_platform_process, AppConfig, EngineEvent, EngineMode, LegacyConfig,
    MoveFreelyEngine,
};

#[derive(Parser)]
#[command(
    name = "mf",
    version,
    about = "move freely — cross-platform multi-device keyboard, mouse & clipboard sharing"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run a full encrypted peer-to-peer Move Freely node (with optional pairing PIN).
    Daemon {
        /// Override data/config directory (useful for running isolated local instances).
        #[arg(long)]
        data_dir: Option<PathBuf>,
        /// Override listening port.
        #[arg(short, long)]
        port: Option<u16>,
        /// Override device display name.
        #[arg(short, long)]
        name: Option<String>,
        /// Open a pairing PIN window on startup (6-digit code, or auto-generated with `--pin auto`).
        #[arg(long)]
        pin: Option<String>,
        /// Use headless virtual input/clipboard backends (no OS permissions required).
        #[arg(long)]
        headless: bool,
        /// Log received input events in headless mode.
        #[arg(long)]
        echo: bool,
    },
    /// Pair with another Move Freely peer at <addr> using its 6-digit PIN.
    Pair {
        /// Peer socket address, e.g. `192.168.1.42:24800`.
        addr: SocketAddr,
        /// 6-digit pairing PIN displayed on the remote peer.
        #[arg(long)]
        pin: String,
        /// Override data/config directory.
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },
    /// Legacy v1-compatible server role (supports `--demo` scripted testing).
    Serve {
        /// Path to the TOML layout config.
        #[arg(short, long, default_value = "config/layout.toml")]
        config: PathBuf,
        /// Send a scripted sequence of input events once a client connects.
        #[arg(long)]
        demo: bool,
    },
    /// Legacy v1-compatible client role (supports `--echo` testing).
    Connect {
        /// Server address, e.g. `192.168.1.10:24800`.
        addr: String,
        /// This screen's name; must match a `[[screen]]` in the server config.
        #[arg(short, long)]
        name: String,
        /// Shared secret matching the server's `token`.
        #[arg(short, long)]
        token: String,
        /// This screen's width in pixels.
        #[arg(long)]
        width: u32,
        /// This screen's height in pixels.
        #[arg(long)]
        height: u32,
        /// Log received input events instead of injecting them.
        #[arg(long)]
        echo: bool,
    },
    /// Inject a small test gesture locally to verify OS injection + permissions.
    Selftest,
    /// Inspect OS permissions and session capabilities.
    Permissions {
        /// Prompt the OS to request missing permissions where supported.
        #[arg(long)]
        prompt: bool,
        /// Output JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    init_platform_process();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mf=info,mf_core=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Daemon {
            data_dir,
            port,
            name,
            pin,
            headless,
            echo,
        } => {
            let dir = data_dir.unwrap_or_else(mf_core::config::default_data_dir);
            let mut cfg = AppConfig::load_or_create(&dir.join("config.toml"))?;
            if let Some(p) = port {
                cfg.listen_port = p;
            }
            if let Some(n) = name {
                cfg.device_name = n;
            }
            let mode = if headless {
                EngineMode::Headless {
                    echo,
                    monitors: vec![],
                }
            } else {
                EngineMode::Native
            };
            let (handle, _) = MoveFreelyEngine::start(Some(dir), Some(cfg), mode).await?;
            let snap = handle.snapshot().await?;
            tracing::info!(
                name = %snap.local.name,
                id = %snap.local.device_id.short(),
                port = snap.local.listen_port,
                ips = ?snap.local.local_ips,
                session = %snap.local.permissions.session_type,
                "Move Freely peer node started"
            );
            if !snap.local.permissions.can_capture || !snap.local.permissions.can_inject {
                tracing::warn!(
                    summary = %snap.local.permissions.summary,
                    hint = ?snap.local.permissions.action_hint,
                    "OS permission notice"
                );
            }
            if let Some(pin_arg) = pin {
                let custom = if pin_arg.eq_ignore_ascii_case("auto") {
                    None
                } else {
                    Some(pin_arg)
                };
                let status = handle.open_pairing_window(custom).await?;
                if let Some(code) = status.pin {
                    tracing::info!(pin = %code, expires_in_secs = status.expires_in_secs, "Pairing window open — share this PIN with the connecting device");
                }
            }

            let mut events = handle.subscribe();
            tokio::spawn(async move {
                while let Ok(ev) = events.recv().await {
                    if let EngineEvent::IncomingPair(req) = ev {
                        tracing::info!(
                            from = %req.name,
                            addr = %req.addr,
                            pin = %req.pin,
                            "Auto-accepting incoming pairing request matching active CLI session"
                        );
                        handle.respond_pair_request(req.request_id, true);
                    }
                }
            });

            tokio::signal::ctrl_c().await?;
            tracing::info!("shutting down");
            tokio::time::sleep(Duration::from_millis(100)).await;
            Ok(())
        }
        Command::Pair {
            addr,
            pin,
            data_dir,
        } => {
            let dir = data_dir.unwrap_or_else(mf_core::config::default_data_dir);
            let mut cfg = AppConfig::load_or_create(&dir.join("config.toml"))?;
            cfg.listen_port = 0;
            let (handle, _) = MoveFreelyEngine::start(
                Some(dir),
                Some(cfg),
                EngineMode::Headless {
                    echo: false,
                    monitors: vec![],
                },
            )
            .await?;
            handle.pair_with_addr(addr, None, pin).await?;
            tracing::info!(%addr, "Successfully paired and trusted peer");
            handle.shutdown();
            Ok(())
        }
        Command::Serve { config, demo } => {
            let cfg = LegacyConfig::load(&config)?;
            server::run(cfg, demo).await
        }
        Command::Connect {
            addr,
            name,
            token,
            width,
            height,
            echo,
        } => {
            let opts = client::ConnectOptions {
                addr,
                name,
                token,
                width,
                height,
                echo,
            };
            client::run(opts).await
        }
        Command::Selftest => mf_core::input::selftest(),
        Command::Permissions { prompt, json } => {
            let status = check_permissions(prompt);
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Session: {}", status.session_type);
                println!("Can capture: {}", status.can_capture);
                println!("Can inject: {}", status.can_inject);
                println!("Summary: {}", status.summary);
                if let Some(hint) = status.action_hint {
                    println!("Hint: {hint}");
                }
            }
            Ok(())
        }
    }
}
