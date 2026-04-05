use super::parser::{parse_node, ParseError, ParsedNode};

/// Manifest header size: root_height (1 byte) + manifest_depth (1 byte).
const HEADER_SIZE: usize = 2;

/// Parse the 2-byte manifest header.
///
/// Returns `(root_height, manifest_depth)`.
pub fn parse_manifest_header(data: &[u8]) -> Result<(u8, u8), ParseError> {
    if data.len() < HEADER_SIZE {
        return Err(ParseError::UnexpectedEof);
    }
    Ok((data[0], data[1]))
}

/// Walk the manifest DFS stream and extract subtree IDs.
///
/// Internal nodes at `manifest_depth` are boundary nodes — their `left_label`
/// and `right_label` are the 32-byte IDs of the subtree chunks to download.
/// Leaf nodes terminate the walk at any depth (they have no children).
///
/// The manifest bytes must include the 2-byte header.
pub fn extract_subtree_ids(
    manifest_bytes: &[u8],
    key_length: usize,
) -> Result<Vec<[u8; 32]>, ParseError> {
    let (_, manifest_depth) = parse_manifest_header(manifest_bytes)?;

    let mut subtree_ids = Vec::new();
    let mut offset = HEADER_SIZE;
    // Stack of pending right-child visits: each entry is the level of the right child.
    let mut depth_stack: Vec<u8> = Vec::new();
    // JVM starts the DFS at level = 1 (not 0). Root is level 1, boundary at manifest_depth.
    let mut current_depth: u8 = 1;

    while offset < manifest_bytes.len() {
        let (node, consumed) = parse_node(&manifest_bytes[offset..], key_length)?;
        offset += consumed;

        match node {
            ParsedNode::Internal {
                left_label,
                right_label,
                ..
            } => {
                if current_depth == manifest_depth {
                    // Boundary node: children are subtree chunks, not serialized here.
                    subtree_ids.push(left_label);
                    subtree_ids.push(right_label);
                    // This node is fully consumed; pop to next pending.
                    current_depth = match depth_stack.pop() {
                        Some(d) => d,
                        None => break,
                    };
                } else {
                    // Above boundary: children are inline. Push right child depth,
                    // descend into left child.
                    depth_stack.push(current_depth + 1);
                    current_depth += 1;
                }
            }
            ParsedNode::Leaf { .. } => {
                // Leaf terminates this branch. Pop to next pending.
                current_depth = match depth_stack.pop() {
                    Some(d) => d,
                    None => break,
                };
            }
        }
    }

    Ok(subtree_ids)
}
