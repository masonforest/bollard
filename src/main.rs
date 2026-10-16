mod api;
mod apply;
mod bitcoin_watch;
mod chain;
mod client;
mod config;
mod crypto;
mod db;
mod enter;
mod fb;
mod fbio;
mod ledger;
mod lottery;
mod node;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "bollard", version, about = "Bollard: a cross-chain order book finalized on Bitcoin")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run a node. It follows by default and leads when its lottery entry wins.
    Run(config::RunArgs),
    /// Generate a key. The same key is a node key, a Bollard account and a Bitcoin address.
    Keygen,
    /// Broadcast a lottery entry on Bitcoin carrying this node's address and log tip.
    Enter(enter::EnterArgs),
    /// Build, sign and submit a transaction to a node.
    Tx(client::TxArgs),
    /// Print the Bitcoin and Ethereum addresses a secp256k1 account key controls.
    Addresses {
        #[arg(long)]
        pubkey: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info,sqlx=warn,hyper=warn".into()))
        .init();
    match Cli::parse().cmd {
        Cmd::Run(args) => node::run(args).await,
        Cmd::Keygen => client::keygen(),
        Cmd::Enter(args) => enter::enter(args).await,
        Cmd::Tx(args) => client::tx(args).await,
        Cmd::Addresses { pubkey } => {
            let pk = crypto::pubkey_from_hex(&pubkey)?;
            let cpk = bitcoin::CompressedPublicKey(bitcoin::secp256k1::PublicKey::from_slice(&pk)?);
            println!("bitcoin  {}", bitcoin::Address::p2wpkh(&cpk, bitcoin::Network::Bitcoin));
            use sha3::{Digest, Keccak256};
            let unc = cpk.0.serialize_uncompressed();
            let h = Keccak256::digest(&unc[1..]);
            println!("ethereum 0x{}", hex::encode(&h[12..]));
            Ok(())
        }
    }
}
