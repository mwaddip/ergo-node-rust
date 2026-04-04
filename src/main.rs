use std::sync::Arc;

use enr_chain::{ChainConfig, HeaderChain, StateType, HEADER_TYPE_ID};
use ergo_chain_types::ADDigest;
use enr_store::{ModifierStore, RedbModifierStore};
use ergo_node_rust::{P2pTransport, SharedChain, SharedStore, ValidationPipeline};
use ergo_sync::{HeaderSync, SyncConfig};
use ergo_validation::DigestValidator;
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

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            state_type: default_state_type(),
            verify_transactions: default_verify_transactions(),
            blocks_to_keep: default_blocks_to_keep(),
        }
    }
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
    let network = config.proxy.network;
    let chain_config = match network {
        enr_p2p::types::Network::Testnet => ChainConfig::testnet(),
        enr_p2p::types::Network::Mainnet => ChainConfig::mainnet(),
    };

    // Parse node config from the same TOML file
    let config_content = std::fs::read_to_string(&config_path)?;
    let root_config: RootConfig = toml::from_str(&config_content)?;
    let node_config = root_config.node.unwrap_or_default();
    let state_type = match node_config.state_type.as_str() {
        "utxo" => StateType::Utxo,
        "digest" => StateType::Digest,
        other => {
            return Err(format!("unknown state_type '{}' (expected 'utxo' or 'digest')", other).into());
        }
    };
    let verify_transactions = node_config.verify_transactions;
    let blocks_to_keep = node_config.blocks_to_keep;
    tracing::info!(state_type = ?state_type, verify_transactions, blocks_to_keep, "node config");
    let data_dir = std::path::PathBuf::from(node_config.data_dir);
    std::fs::create_dir_all(&data_dir)?;
    let store = Arc::new(RedbModifierStore::new(&data_dir.join("modifiers.redb"))?);

    // Shared header chain: pipeline writes, sync reads
    let mut chain = HeaderChain::new(chain_config);

    // Restore chain from stored headers (using best chain index)
    if let Some((tip_height, _)) = store.best_header_tip()? {
        let mut loaded = 0u32;
        for height in 1..=tip_height {
            let id = match store.best_header_at(height)? {
                Some(id) => id,
                None => {
                    tracing::warn!(height, "gap in best chain, stopping load");
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
            match chain.try_append(header) {
                Ok(enr_chain::AppendResult::Extended) => {}
                Ok(enr_chain::AppendResult::Forked { .. }) => {
                    tracing::error!(height, "stored best-chain header detected as fork — store corrupted?");
                    break;
                }
                Err(e) => {
                    tracing::error!(height, "stored header chain failed: {e}, stopping load");
                    break;
                }
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

    // Build Mode feature from node config — tells peers what we can serve
    let mode_config = enr_p2p::transport::handshake::ModeConfig {
        state_type_id: match state_type {
            StateType::Utxo => 0,
            StateType::Digest => 1,
        },
        verifying: verify_transactions,
        blocks_to_keep: blocks_to_keep as i32,
    };

    // Start P2P with modifier sink (no validator)
    let p2p = Arc::new(enr_p2p::node::P2pNode::start(config, Some(modifier_tx), mode_config).await?);

    // Validation pipeline — progress channel feeds sync, delivery channel feeds tracker
    let pipeline_chain = chain.clone();
    let sync_store = SharedStore::new(store.clone());
    let (progress_tx, progress_rx) = tokio::sync::mpsc::channel(4);
    // Control channel: unbounded — Reorg/NeedModifier must never be dropped
    let (delivery_control_tx, delivery_control_rx) = tokio::sync::mpsc::unbounded_channel();
    // Data channel: bounded — Received/Evicted are lossy, ok to drop
    let (delivery_data_tx, delivery_data_rx) = tokio::sync::mpsc::channel(64);
    tokio::spawn(async move {
        let mut pipeline =
            ValidationPipeline::new(modifier_rx, pipeline_chain, store, progress_tx, delivery_control_tx, delivery_data_tx);
        pipeline.run().await;
    });

    // Subscribe to events for the sync machine
    let events = p2p.subscribe().await;

    // Bridge implementations
    let transport = P2pTransport::new(p2p.clone(), events);
    let sync_chain = SharedChain::new(chain.clone());

    // Create block validator (digest mode)
    // If we restored a chain from store, start validation from the tip's state root.
    // Otherwise start from the genesis digest.
    let chain_guard = chain.lock().await;
    let validator = if chain_guard.height() > 0 {
        let tip = chain_guard.tip();
        let height = chain_guard.height();
        let digest = tip.state_root;
        tracing::info!(
            height,
            digest = ?digest,
            "block validator resuming from stored chain tip (digest mode)"
        );
        DigestValidator::from_state(digest, height, 0)
    } else {
        let genesis_digest_hex = match network {
            enr_p2p::types::Network::Testnet =>
                "cb63aa99a3060f341781d8662b58bf18b9ad258db4fe88d09f8f71cb668cad4502",
            enr_p2p::types::Network::Mainnet =>
                "a5df145d41ab15a01e0cd3ffbab046f0d029e5412293072ad0f5827428589b9302",
        };
        let genesis_bytes = hex::decode(genesis_digest_hex).expect("invalid genesis digest hex");
        let genesis_digest = ADDigest::try_from(genesis_bytes.as_slice())
            .expect("invalid genesis digest length");
        tracing::info!(genesis_digest = genesis_digest_hex, "block validator starting from genesis (digest mode)");
        DigestValidator::new(genesis_digest, 0)
    };
    drop(chain_guard);

    // Build sync config from P2P network settings
    let net = net_settings;
    let sync_config = SyncConfig {
        delivery_timeout: std::time::Duration::from_secs(net.delivery_timeout_secs),
        max_delivery_checks: net.max_delivery_checks,
        state_type,
        ..SyncConfig::default()
    };

    // Start sync in a background task (with delivery tracker channel)
    tokio::spawn(async move {
        let mut sync = HeaderSync::new(sync_config, transport, sync_chain, sync_store, validator, progress_rx, delivery_control_rx, delivery_data_rx);
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
