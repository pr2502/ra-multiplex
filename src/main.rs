use anyhow::Result;
use clap::{Parser, Subcommand};

mod client;
mod config;
mod proto;
mod server;

#[derive(Parser, Debug)]
struct Opts {
    /// Show version and exit
    #[arg(long)]
    version: bool,

    /// Path to the LSP server executable
    #[arg(
        long,
        alias = "ra-mux-server",
        env = "RA_MUX_SERVER",
        default_value = "rust-analyzer"
    )]
    server_path: String,

    /// No command defaults to client
    #[command(subcommand)]
    command: Option<Command>,

    /// Arguments passed to the LSP server
    rest: Vec<String>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Connect to a ra-mux server
    Client,

    /// Start a ra-mux server
    Server,

    /// Start a LSP server and dump all communication with the client
    LspDump,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let opts = Opts::parse();
    println!("{opts:#?}");

    if opts.version {
        // print version and exit instead of trying to connect to a server
        // see <https://github.com/pr2502/ra-multiplex/issues/4>
        println!("ra-mux {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    match opts.command.unwrap_or(Command::Client) {
        Command::Client => client::main(opts.server_path, opts.rest).await,
        Command::Server => server::main().await,
        Command::LspDump => {
            todo!("lsp-dump command");
        }
    }
}
