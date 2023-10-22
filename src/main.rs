use std::env;

use clap::{Args, Parser, Subcommand};
use ra_multiplex::{ext, proxy, server};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// No command defaults to client
    #[command(subcommand)]
    command: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Connect to a ra-mux server [default]
    Client {
        /// Path to the LSP server executable
        #[arg(
            long,
            alias = "ra-mux-server",
            env = "RA_MUX_SERVER",
            default_value = "rust-analyzer"
        )]
        server_path: String,

        /// Arguments passed to the LSP server
        server_args: Vec<String>,
    },

    /// Start a ra-mux server
    Server {},

    /// Print server status
    Status {},
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Cmd::Server {}) => server::run().await,
        Some(Cmd::Client {
            server_path,
            server_args,
        }) => proxy::run(server_path, server_args).await,
        Some(Cmd::Status {}) => ext::status().await,
        None => {
            let server_path = env::var("RA_MUX_SERVER").unwrap_or_else(|_| "rust-analyzer".into());
            proxy::run(server_path, vec![]).await
        }
    }
}
