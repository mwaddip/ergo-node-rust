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
    let validator = Box::new(HeaderValidator::new());

    let p2p = enr_p2p::node::P2pNode::start(config, Some(validator)).await?;

    tracing::info!("Ergo node running");

    // Run until interrupted
    tokio::signal::ctrl_c().await?;

    if let Some(height) = p2p.peer_count().await.checked_sub(0) {
        tracing::info!(peers = height, "Shutting down");
    }

    Ok(())
}
