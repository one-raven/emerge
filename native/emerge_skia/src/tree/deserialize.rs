//! Deserialization of the EMRG binary format.
//!
//! EMRG binary format:
//! - Header: "EMRG" (4 bytes) + version (1 byte) + node_count (4 bytes BE)
//! - Per node:
//!   - id (8 bytes BE)
//!   - type_tag (1 byte)
//!   - attr_len (4 bytes BE) + attr_bytes (typed attribute block)
//!   - child_count (2 bytes BE)
//!   - For each child: child_id (8 bytes BE)
//!   - nearby_count (2 bytes BE)
//!   - For each mounted nearby root in definition order:
//!     slot_tag (1 byte) + id (8 bytes BE)
//!
//! Attribute block format:
//!   - attr_count (2 bytes BE)
//!   - For each attr: tag (1 byte) + value (varies by tag)

use super::attrs::{SharedAttrs, decode_attrs_shared};
#[cfg(test)]
use super::element::NearbyMount;
use super::element::{Element, ElementKind, ElementTree, NearbyMounts, NearbySlot, NodeId};

const MAGIC: &[u8] = b"EMRG";
const VERSION: u8 = 9;

/// Error type for deserialization failures.
#[derive(Debug, Clone)]
pub enum DecodeError {
    InvalidMagic,
    UnsupportedVersion(u8),
    UnexpectedEof,
    InvalidTypeTag(u8),
    InvalidStructure(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMagic => write!(f, "invalid magic bytes, expected 'EMRG'"),
            Self::UnsupportedVersion(v) => write!(f, "unsupported version: {}", v),
            Self::UnexpectedEof => write!(f, "unexpected end of input"),
            Self::InvalidTypeTag(t) => write!(f, "invalid type tag: {}", t),
            Self::InvalidStructure(msg) => write!(f, "invalid structure: {}", msg),
        }
    }
}

impl std::error::Error for DecodeError {}

/// A cursor for reading binary data.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read_bytes(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        if self.pos + len > self.data.len() {
            return Err(DecodeError::UnexpectedEof);
        }
        let bytes = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(bytes)
    }

    fn read_u8(&mut self) -> Result<u8, DecodeError> {
        let bytes = self.read_bytes(1)?;
        Ok(bytes[0])
    }

    fn read_u16_be(&mut self) -> Result<u16, DecodeError> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn read_u32_be(&mut self) -> Result<u32, DecodeError> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_u64_be(&mut self) -> Result<u64, DecodeError> {
        let bytes = self.read_bytes(8)?;
        Ok(u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn read_length_prefixed(&mut self) -> Result<Vec<u8>, DecodeError> {
        let len = self.read_u32_be()? as usize;
        let bytes = self.read_bytes(len)?;
        Ok(bytes.to_vec())
    }
}

/// Intermediate node representation during parsing.
struct RawNode {
    id: NodeId,
    kind: ElementKind,
    attrs_raw: Vec<u8>,
    attrs: SharedAttrs,
    child_ids: Vec<NodeId>,
    nearby: NearbyMounts,
}

/// Decode a full tree from the EMRG binary format.
pub fn decode_tree(data: &[u8]) -> Result<ElementTree, DecodeError> {
    let mut cursor = Cursor::new(data);

    // Read and validate header
    let magic = cursor.read_bytes(4)?;
    if magic != MAGIC {
        return Err(DecodeError::InvalidMagic);
    }

    let version = cursor.read_u8()?;
    if version != VERSION {
        return Err(DecodeError::UnsupportedVersion(version));
    }

    let node_count = cursor.read_u32_be()? as usize;

    if node_count == 0 {
        return Ok(ElementTree::new());
    }

    // One Raven patch: build each element in its box (see `ElementTree::nodes`)
    // as it is parsed, in an arena sized once, instead of collecting ~7.9 KB
    // elements and their 2.5 KB attrs in vectors and moving them again.
    let mut tree = ElementTree::new();
    tree.nodes.reserve(node_count);
    tree.id_to_ix.reserve(node_count);
    let mut root_id = None;
    // Insert all nodes first so child/nearby references can resolve in a second pass.
    let mut topology_updates = Vec::with_capacity(node_count);
    for _ in 0..node_count {
        let raw = decode_node(&mut cursor)?;
        let id = raw.id;
        // First node is the root
        root_id.get_or_insert(id);
        let element = Box::new(Element::with_attrs(id, raw.kind, raw.attrs_raw, raw.attrs));
        topology_updates.push((id, raw.child_ids, raw.nearby.mounts));
        tree.insert_boxed(element);
    }

    // Verify we consumed all data
    if cursor.remaining() > 0 {
        return Err(DecodeError::InvalidStructure(format!(
            "{} bytes remaining after parsing",
            cursor.remaining()
        )));
    }

    // One Raven patch: link the decoded nodes without `set_children`'s
    // invalidation. Every node of a decoded tree is new, so already measure-,
    // render- and registry-dirty; `set_children` still walked each parent's
    // ancestor chain marking it dirty again (a fifth of decoding a 2,300-node
    // screen). Topology versions are bumped as before.
    for (id, child_ids, nearby_mounts) in topology_updates {
        tree.set_inserted_node_topology(&id, child_ids.clone(), child_ids, nearby_mounts)
            .map_err(DecodeError::InvalidStructure)?;
    }

    if let Some(root_id) = root_id {
        tree.set_root_id(root_id);
    }

    Ok(tree)
}

/// Decode a single node from the cursor.
fn decode_node(cursor: &mut Cursor) -> Result<RawNode, DecodeError> {
    // Read ID
    let id = NodeId::from_wire_u64(cursor.read_u64_be()?);

    // Read type tag
    let type_tag = cursor.read_u8()?;
    let kind = ElementKind::from_tag(type_tag).ok_or(DecodeError::InvalidTypeTag(type_tag))?;

    // Read attributes
    let attrs_raw = cursor.read_length_prefixed()?;
    let attrs = decode_attrs_shared(&attrs_raw)?;

    // Read children
    let child_count = cursor.read_u16_be()? as usize;
    let mut child_ids = Vec::with_capacity(child_count);
    for _ in 0..child_count {
        child_ids.push(NodeId::from_wire_u64(cursor.read_u64_be()?));
    }

    let mut nearby = NearbyMounts::default();
    let nearby_count = cursor.read_u16_be()? as usize;
    for _ in 0..nearby_count {
        let slot_tag = cursor.read_u8()?;
        let slot = NearbySlot::from_tag(slot_tag).ok_or_else(|| {
            DecodeError::InvalidStructure(format!("unknown nearby slot tag: {}", slot_tag))
        })?;
        let nearby_id = NodeId::from_wire_u64(cursor.read_u64_be()?);
        nearby.push(slot, nearby_id);
    }

    Ok(RawNode {
        id,
        kind,
        attrs_raw,
        attrs,
        child_ids,
        nearby,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decoding as upstream linked the nodes: `set_children` and
    /// `set_nearby_mounts` per node, with their invalidation.
    fn decode_tree_linking_with_set_children(data: &[u8]) -> ElementTree {
        let mut cursor = Cursor::new(data);
        cursor.read_bytes(5).unwrap();
        let node_count = cursor.read_u32_be().unwrap() as usize;
        let mut tree = ElementTree::new();
        let mut root_id = None;
        let mut topology = Vec::new();
        for _ in 0..node_count {
            let raw = decode_node(&mut cursor).unwrap();
            root_id.get_or_insert(raw.id);
            topology.push((raw.id, raw.child_ids, raw.nearby.mounts));
            tree.insert(Element::with_attrs(
                raw.id,
                raw.kind,
                raw.attrs_raw,
                raw.attrs,
            ));
        }
        for (id, child_ids, mounts) in topology {
            tree.set_children(&id, child_ids).unwrap();
            tree.set_nearby_mounts(&id, mounts).unwrap();
        }
        tree.set_root_id(root_id.unwrap());
        tree
    }

    fn node_state(tree: &ElementTree, id: &NodeId) -> String {
        let element = tree.get(id).unwrap();
        let ix = tree.ix_of(id).unwrap();
        format!(
            "{:?} layout=({} {} {} {}) refresh=({} {} {} {} gen={} reg_cache={} frag={}) \
             versions={:?} key={:?} parent={:?} children={:?} paint={:?} nearby={:?}",
            element.spec.kind,
            element.layout.measure_dirty,
            element.layout.measure_descendant_dirty,
            element.layout.resolve_dirty,
            element.layout.resolve_descendant_dirty,
            element.refresh.render_dirty,
            element.refresh.render_descendant_dirty,
            element.refresh.registry_dirty,
            element.refresh.registry_descendant_dirty,
            element.refresh.paint_generation,
            element.refresh.registry_cache.is_some(),
            element.refresh.render_fragment_cache.borrow().is_some(),
            element.layout.topology_versions,
            tree.topology_dependency_key_for(id),
            tree.parent_link_of(ix).map(|link| match link {
                super::super::element::ParentLink::Child { parent } => (tree.id_of(parent), None),
                super::super::element::ParentLink::Nearby { host, slot } =>
                    (tree.id_of(host), Some(slot)),
            }),
            tree.child_ids(id),
            tree.paint_child_ids_for(id),
            tree.nearby_mounts_for(id),
        )
    }

    // One Raven patch: decoding links nodes without re-marking them dirty;
    // the result must be the tree upstream's linking produced.
    #[test]
    fn decoded_tree_matches_linking_with_set_children() {
        let id = NodeId::from_wire_u64;
        let mut source = ElementTree::new();
        let nodes = [
            (1, ElementKind::Column),
            (2, ElementKind::Row),
            (3, ElementKind::Text),
            (4, ElementKind::El),
            (5, ElementKind::El),
            (6, ElementKind::Text),
            (7, ElementKind::El),
            (8, ElementKind::Text),
        ];
        for (n, kind) in nodes {
            source.insert(Element::with_attrs(
                id(n),
                kind,
                Vec::new(),
                crate::tree::attrs::Attrs::default(),
            ));
        }
        source.set_root_id(id(1));
        source.set_children(&id(1), vec![id(2), id(4)]).unwrap();
        source.set_children(&id(2), vec![id(3)]).unwrap();
        source.set_children(&id(4), vec![id(7)]).unwrap();
        source.set_children(&id(5), vec![id(6)]).unwrap();
        source.set_children(&id(7), vec![id(8)]).unwrap();
        source
            .set_nearby_mounts(
                &id(4),
                vec![NearbyMount {
                    slot: NearbySlot::InFront,
                    id: id(5),
                }],
            )
            .unwrap();
        let bytes = super::super::serialize::encode_tree(&source);

        super::super::element::REFRESH_DIRTY_MARKS.with(|count| count.set(0));
        let decoded = decode_tree(&bytes).unwrap();
        assert_eq!(
            super::super::element::REFRESH_DIRTY_MARKS.with(|count| count.get()),
            0,
            "decoding re-marked new nodes dirty"
        );
        let expected = decode_tree_linking_with_set_children(&bytes);
        assert_eq!(decoded.len(), 8);
        assert_eq!(decoded.root_id(), expected.root_id());
        for (n, _) in nodes {
            assert_eq!(
                node_state(&decoded, &id(n)),
                node_state(&expected, &id(n)),
                "node {n}"
            );
        }
    }

    fn make_header(node_count: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(MAGIC);
        buf.push(VERSION);
        buf.extend_from_slice(&node_count.to_be_bytes());
        buf
    }

    #[test]
    fn test_decode_empty_tree() {
        let data = make_header(0);
        let tree = decode_tree(&data).unwrap();
        assert!(tree.is_empty());
    }

    #[test]
    fn test_invalid_magic() {
        let data = b"XXXX\x02\x00\x00\x00\x00";
        let err = decode_tree(data).unwrap_err();
        assert!(matches!(err, DecodeError::InvalidMagic));
    }

    #[test]
    fn test_unsupported_version() {
        let mut data = Vec::new();
        data.extend_from_slice(MAGIC);
        data.push(99); // unsupported version
        data.extend_from_slice(&0u32.to_be_bytes());

        let err = decode_tree(&data).unwrap_err();
        assert!(matches!(err, DecodeError::UnsupportedVersion(99)));
    }

    #[test]
    fn test_decode_single_node() {
        let mut data = make_header(1);

        let id = 0x010203_u64;

        // attrs: empty block (0 attrs)
        let attrs_block: Vec<u8> = vec![0, 0]; // attr_count = 0

        data.extend_from_slice(&id.to_be_bytes());

        // type tag
        data.push(4); // el

        // attr_len + attrs
        data.extend_from_slice(&(attrs_block.len() as u32).to_be_bytes());
        data.extend_from_slice(&attrs_block);

        // child_count = 0
        data.extend_from_slice(&0u16.to_be_bytes());
        data.extend_from_slice(&0u16.to_be_bytes());

        let tree = decode_tree(&data).unwrap();
        assert_eq!(tree.len(), 1);

        let root_id = tree.root_id().unwrap();
        let root = tree.get(&root_id).unwrap();
        assert_eq!(root.spec.kind, ElementKind::El);
        assert!(root.children.is_empty());
    }

    #[test]
    fn test_decode_with_attrs() {
        let mut data = make_header(1);

        let id = 0x010203_u64;

        // attrs: 1 attr, tag=4 (spacing), f64=10.0
        let mut attrs_block = vec![0, 1]; // attr_count = 1
        attrs_block.push(4); // tag = spacing
        attrs_block.extend_from_slice(&10.0_f64.to_be_bytes());

        data.extend_from_slice(&id.to_be_bytes());

        // type tag
        data.push(3); // column

        // attr_len + attrs
        data.extend_from_slice(&(attrs_block.len() as u32).to_be_bytes());
        data.extend_from_slice(&attrs_block);

        // child_count = 0
        data.extend_from_slice(&0u16.to_be_bytes());
        data.extend_from_slice(&0u16.to_be_bytes());

        let tree = decode_tree(&data).unwrap();
        let root_id = tree.root_id().unwrap();
        let root = tree.get(&root_id).unwrap();

        assert_eq!(root.spec.kind, ElementKind::Column);
        assert_eq!(root.layout.effective.spacing, Some(10.0));
    }
}
