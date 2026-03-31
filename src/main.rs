use ergo_node_rust::HeaderValidator;

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
        enr_p2p::types::Network::Testnet => enr_chain::ChainConfig::testnet(),
        enr_p2p::types::Network::Mainnet => enr_chain::ChainConfig::mainnet(),
    };
    let validator = Box::new(HeaderValidator::new(chain_config));

    let p2p = enr_p2p::node::P2pNode::start(config, Some(validator)).await?;

    tracing::info!("Ergo node running");

    // Run until interrupted
    tokio::signal::ctrl_c().await?;

    if let Some(height) = p2p.peer_count().await.checked_sub(0) {
        tracing::info!(peers = height, "Shutting down");
    }

    Ok(())
}
