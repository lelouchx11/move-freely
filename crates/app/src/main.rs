//! `mf` — the move freely software KVM. One binary, two roles.

mod client;
mod config;
mod server;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mf", version, about = "move freely — share one keyboard + mouse across machines")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run as the server (the machine with the physical keyboard + mouse).
    Serve {
        /// Path to the TOML layout config.
        #[arg(short, long, default_value = "config/layout.toml")]
        config: PathBuf,
        /// Once a client connects, send a scripted sequence of input events.
        /// Used to verify the wire pipe before any OS capture backend exists.
        #[arg(long)]
        demo: bool,
    },
    /// Run as a client, connecting to a server and acting on its input.
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
        /// Log received input events instead of injecting them (no OS
        /// permissions required). Useful for verifying connectivity.
        #[arg(long)]
        echo: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mf=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve { config, demo } => {
            let cfg = config::Config::load(&config)?;
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
    }
}
