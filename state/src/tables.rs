use redb::TableDefinition;

pub const NODES_TABLE: TableDefinition<&[u8], &[u8]> = TableDefinition::new("nodes");
pub const UNDO_TABLE: TableDefinition<u64, &[u8]> = TableDefinition::new("undo");
pub const META_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");

pub const META_TOP_NODE_HASH: &str = "top_node_hash";
pub const META_TOP_NODE_HEIGHT: &str = "top_node_height";
pub const META_CURRENT_VERSION: &str = "current_version";
pub const META_LSN: &str = "lsn";
pub const META_VERSIONS: &str = "versions";
pub const META_BLOCK_HEIGHT: &str = "block_height";
