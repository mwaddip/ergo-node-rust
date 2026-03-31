use std::sync::Arc;

use enr_chain::{ChainConfig, HeaderChain};
use ergo_node_rust::{HeaderValidator, P2pTransport, SharedChain};
use ergo_sync::HeaderSync;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let config_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "ergo.toml".to_string());

    let config = enr_p2p::config::Config::load(&config_path)?;

    // Derive chain config from P2P network setting
    let chain_config = match config.proxy.network {
        enr_p2p::types::Network::Testnet => ChainConfig::testnet(),
        enr_p2p::types::Network::Mainnet => ChainConfig::mainnet(),
    };

    // Shared header chain: validator writes, sync reads
    let chain = Arc::new(Mutex::new(HeaderChain::new(chain_config)));

    // Validator for the P2P layer
    let validator = Box::new(HeaderValidator::new(chain.clone()));

    // Start P2P
    let p2p = Arc::new(enr_p2p::node::P2pNode::start(config, Some(validator)).await?);

    // Subscribe to events for the sync machine
    let events = p2p.subscribe().await;

    // Bridge implementations
    let transport = P2pTransport::new(p2p.clone(), events);
    let sync_chain = SharedChain::new(chain.clone());

    // Start sync in a background task
    tokio::spawn(async move {
        let mut sync = HeaderSync::new(transport, sync_chain);
        sync.run().await;
    });

    tracing::info!("Ergo node running");

    // Run until interrupted
    tokio::signal::ctrl_c().await?;

    let height = chain.lock().await.height();
    let peers = p2p.peer_count().await;
    tracing::info!(chain_height = height, peers, "Shutting down");

    Ok(())
}
