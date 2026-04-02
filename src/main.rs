use std::sync::Arc;

use enr_chain::{ChainConfig, HeaderChain, HEADER_TYPE_ID};
use enr_store::{ModifierStore, RedbModifierStore};
use ergo_node_rust::{P2pTransport, SharedChain, SharedStore, ValidationPipeline};
use ergo_sync::{HeaderSync, SyncConfig};
use serde::Deserialize;
use tokio::sync::Mutex;

/// Node-level config parsed from the `[node]` section of ergo.toml.
#[derive(Debug, Deserialize)]
struct NodeConfig {
    #[serde(default = "default_data_dir")]
    data_dir: String,
    #[serde(default = "default_state_type")]
    state_type: String,
    #[serde(default = "default_verify_transactions")]
    verify_transactions: bool,
    #[serde(default = "default_blocks_to_keep")]
    blocks_to_keep: i64,
}

fn default_data_dir() -> String {
    "/var/lib/ergo-node/data".to_string()
}
fn default_state_type() -> String {
    "utxo".to_string()
}
fn default_verify_transactions() -> bool {
    true
}
fn default_blocks_to_keep() -> i64 {
    -1
}

/// Top-level config wrapper — just the [node] section, P2P is parsed separately.
#[derive(Debug, Deserialize)]
struct RootConfig {
    #[serde(default)]
    node: Option<NodeConfig>,
}

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

    // Parse node config from the same TOML file
    let config_content = std::fs::read_to_string(&config_path)?;
    let root_config: RootConfig = toml::from_str(&config_content)?;
    let data_dir = std::path::PathBuf::from(
        root_config.node.map(|n| n.data_dir).unwrap_or_else(default_data_dir),
    );
    std::fs::create_dir_all(&data_dir)?;
    let store = Arc::new(RedbModifierStore::new(&data_dir.join("modifiers.redb"))?);

    // Shared header chain: pipeline writes, sync reads
    let mut chain = HeaderChain::new(chain_config);

    // Restore chain from stored headers
    if let Some((tip_height, _)) = store.tip(HEADER_TYPE_ID)? {
        let mut loaded = 0u32;
        for height in 1..=tip_height {
            let id = match store.get_id_at(HEADER_TYPE_ID, height)? {
                Some(id) => id,
                None => {
                    tracing::warn!(height, "gap in stored headers, stopping load");
                    break;
                }
            };
            let data = match store.get(HEADER_TYPE_ID, &id)? {
                Some(d) => d,
                None => {
                    tracing::warn!(height, "stored header ID but no data, stopping load");
                    break;
                }
            };
            let header = match enr_chain::parse_header(&data) {
                Ok(h) => h,
                Err(e) => {
                    tracing::error!(height, "stored header parse failed: {e}, stopping load");
                    break;
                }
            };
            if let Err(e) = chain.try_append(header) {
                tracing::error!(height, "stored header chain failed: {e}, stopping load");
                break;
            }
            loaded += 1;
        }
        tracing::info!(loaded, tip = chain.height(), "restored header chain from store");
    }

    let chain = Arc::new(Mutex::new(chain));

    // Modifier channel — P2P produces, pipeline consumes
    let (modifier_tx, modifier_rx) = tokio::sync::mpsc::channel(4096);

    // Grab network settings before P2P takes ownership of config
    let net_settings = config.network_settings();

    // Start P2P with modifier sink (no validator)
    let p2p = Arc::new(enr_p2p::node::P2pNode::start(config, Some(modifier_tx)).await?);

    // Validation pipeline — progress channel feeds sync, delivery channel feeds tracker
    let pipeline_chain = chain.clone();
    let sync_store = SharedStore::new(store.clone());
    let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(4);
    let (delivery_tx, delivery_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        let mut pipeline =
            ValidationPipeline::new(modifier_rx, pipeline_chain, store, progress_tx, delivery_tx);
        pipeline.run().await;
    });

    // Subscribe to events for the sync machine
    let events = p2p.subscribe().await;

    // Bridge implementations
    let transport = P2pTransport::new(p2p.clone(), events);
    let sync_chain = SharedChain::new(chain.clone());

    // Build sync config from P2P network settings
    let net = net_settings;
    let sync_config = SyncConfig {
        delivery_timeout: std::time::Duration::from_secs(net.delivery_timeout_secs),
        max_delivery_checks: net.max_delivery_checks,
        ..SyncConfig::default()
    };

    // Start sync in a background task (with delivery tracker channel)
    tokio::spawn(async move {
        let mut sync = HeaderSync::new(sync_config, transport, sync_chain, sync_store, progress_rx, delivery_rx);
        sync.run().await;
    });

    tracing::info!("Ergo node running");

    // Run until interrupted
    tokio::signal::ctrl_c().await?;

    let height = chain.lock().await.height();
    let peers = p2p.peer_count().await;
    tracing::info!(chain_height = height, peers, "Shutting down");

    // Drop P2P node to close event streams, triggering task shutdown.
    // The pipeline exits when its modifier channel closes (sender dropped
    // with the P2P node). The sync task exits when its event stream ends.
    drop(p2p);
    // Brief grace period for tasks to finish in-flight work
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    Ok(())
}
