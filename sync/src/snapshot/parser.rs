use blake2::digest::{FixedOutput, Update};

type Blake2b256 = blake2::Blake2b<blake2::digest::consts::U32>;

/// Prefix bytes in the label hash computation (ergo_avltree_rust convention).
/// Note: these are the OPPOSITE of the serialization prefixes.
/// Label hash: 0x00 = leaf, 0x01 = internal.
/// Serialization: 0x00 = internal, 0x01 = leaf.
const LABEL_LEAF_PREFIX: u8 = 0x00;
const LABEL_INTERNAL_PREFIX: u8 = 0x01;

/// Packed serialization prefix bytes.
pub const PACKED_INTERNAL_PREFIX: u8 = 0x00;
pub const PACKED_LEAF_PREFIX: u8 = 0x01;

/// Compute the Blake2b256 label for a leaf node.
///
/// Formula (matching ergo_avltree_rust): H(0x00 || key || value || next_key)
pub fn compute_leaf_label(key: &[u8], value: &[u8], next_key: &[u8]) -> [u8; 32] {
    let mut hasher = Blake2b256::default();
    hasher.update(&[LABEL_LEAF_PREFIX]);
    hasher.update(key);
    hasher.update(value);
    hasher.update(next_key);
    let mut label = [0u8; 32];
    label.copy_from_slice(&hasher.finalize_fixed());
    label
}

/// Compute the Blake2b256 label for an internal node.
///
/// Formula (matching ergo_avltree_rust): H(0x01 || balance_byte || left_label || right_label)
/// Note: the key is NOT included in the label (matches JVM scorex behavior).
pub fn compute_internal_label(
    balance: i8,
    left_label: &[u8; 32],
    right_label: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = Blake2b256::default();
    hasher.update(&[LABEL_INTERNAL_PREFIX]);
    hasher.update(&[balance as u8]);
    hasher.update(left_label);
    hasher.update(right_label);
    let mut label = [0u8; 32];
    label.copy_from_slice(&hasher.finalize_fixed());
    label
}

/// A parsed AVL+ node with its computed label and raw packed bytes.
#[derive(Debug, Clone)]
pub enum ParsedNode {
    Internal {
        label: [u8; 32],
        balance: i8,
        key: [u8; 32],
        left_label: [u8; 32],
        right_label: [u8; 32],
        packed_bytes: Vec<u8>,
    },
    Leaf {
        label: [u8; 32],
        key: [u8; 32],
        value: Vec<u8>,
        next_key: [u8; 32],
        packed_bytes: Vec<u8>,
    },
}

impl ParsedNode {
    pub fn label(&self) -> &[u8; 32] {
        match self {
            ParsedNode::Internal { label, .. } => label,
            ParsedNode::Leaf { label, .. } => label,
        }
    }

    pub fn packed_bytes(&self) -> &[u8] {
        match self {
            ParsedNode::Internal { packed_bytes, .. } => packed_bytes,
            ParsedNode::Leaf { packed_bytes, .. } => packed_bytes,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ParseError {
    #[error("unexpected end of data")]
    UnexpectedEof,
    #[error("invalid node prefix byte: 0x{0:02x}")]
    InvalidPrefix(u8),
}

/// Parse a single AVL+ node from a byte slice. Returns the parsed node and
/// number of bytes consumed.
///
/// The key_length is always 32 for Ergo (ADKey = box ID).
/// Value length is variable (no fixed length).
pub fn parse_node(data: &[u8], key_length: usize) -> Result<(ParsedNode, usize), ParseError> {
    if data.is_empty() {
        return Err(ParseError::UnexpectedEof);
    }

    match data[0] {
        PACKED_INTERNAL_PREFIX => {
            // 0x00 + balance(1) + key(key_length) + left_label(32) + right_label(32)
            let expected = 1 + 1 + key_length + 32 + 32;
            if data.len() < expected {
                return Err(ParseError::UnexpectedEof);
            }

            let balance = data[1] as i8;
            let mut key = [0u8; 32];
            key.copy_from_slice(&data[2..2 + key_length]);
            let mut left_label = [0u8; 32];
            left_label.copy_from_slice(&data[2 + key_length..2 + key_length + 32]);
            let mut right_label = [0u8; 32];
            right_label.copy_from_slice(&data[2 + key_length + 32..expected]);

            let label = compute_internal_label(balance, &left_label, &right_label);
            let packed_bytes = data[..expected].to_vec();

            Ok((
                ParsedNode::Internal {
                    label,
                    balance,
                    key,
                    left_label,
                    right_label,
                    packed_bytes,
                },
                expected,
            ))
        }
        PACKED_LEAF_PREFIX => {
            // 0x01 + key(key_length) + value_len(4 BE) + value + next_key(key_length)
            let header_len = 1 + key_length + 4;
            if data.len() < header_len {
                return Err(ParseError::UnexpectedEof);
            }

            let mut key = [0u8; 32];
            key.copy_from_slice(&data[1..1 + key_length]);

            let value_len = u32::from_be_bytes([
                data[1 + key_length],
                data[2 + key_length],
                data[3 + key_length],
                data[4 + key_length],
            ]) as usize;

            let total = header_len + value_len + key_length;
            if data.len() < total {
                return Err(ParseError::UnexpectedEof);
            }

            let value = data[header_len..header_len + value_len].to_vec();
            let mut next_key = [0u8; 32];
            next_key.copy_from_slice(&data[header_len + value_len..total]);

            let label = compute_leaf_label(&key, &value, &next_key);
            let packed_bytes = data[..total].to_vec();

            Ok((
                ParsedNode::Leaf {
                    label,
                    key,
                    value,
                    next_key,
                    packed_bytes,
                },
                total,
            ))
        }
        other => Err(ParseError::InvalidPrefix(other)),
    }
}

/// Parse a complete DFS byte stream into a sequence of parsed nodes.
/// Used for both manifest and chunk reconstruction.
pub fn parse_dfs_stream(
    data: &[u8],
    key_length: usize,
) -> Result<Vec<ParsedNode>, ParseError> {
    let mut nodes = Vec::new();
    let mut offset = 0;

    while offset < data.len() {
        let (node, consumed) = parse_node(&data[offset..], key_length)?;
        nodes.push(node);
        offset += consumed;
    }

    Ok(nodes)
}
