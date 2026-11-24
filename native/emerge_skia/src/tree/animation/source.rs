//! First-write presentation preimages. This is pending frame work, not idle target history.
use super::super::layout::dimensions::{Axis, AxisFootprint, capture_axis};
use super::super::{
    attrs::SharedAttrs,
    element::{ElementKind, ElementTree, NearbySlot, NodeId, ParentLink},
    invalidation::TreeInvalidation,
};
use std::{collections::HashMap, sync::Arc};
type AttachmentPath = Vec<(NodeId, u64, ElementKind, Option<NearbySlot>)>;

#[derive(Clone, Debug)]
pub struct PresentationSource {
    pub mounted_at: u64,
    pub model: u64,
    pub declared: SharedAttrs,
    /// Existing effective coordinates, before patch or scale preparation.
    pub effective: SharedAttrs,
    pub scale: f32,
    pub dimensions: [Option<AxisFootprint>; 2],
    pub sampled: [bool; 2],
    pub path: Arc<AttachmentPath>,
    transport_origin: Arc<TransportOrigin>,
}

#[derive(Clone, Debug)]
struct TransportOrigin {
    mounted_at: u64,
    model: u64,
    scale: f32,
    dimensions: [Option<AxisFootprint>; 2],
    path: Arc<AttachmentPath>,
}
impl PresentationSource {
    pub(crate) fn attachment_path(tree: &ElementTree, id: &NodeId) -> AttachmentPath {
        std::iter::successors(tree.ix_of(id), |ix| {
            if tree.id_of(*ix) == tree.root_id() {
                return None;
            }
            match tree.parent_link_of(*ix)? {
                ParentLink::Child { parent } => Some(parent),
                ParentLink::Nearby { host, .. } => Some(host),
            }
        })
        .take(tree.len())
        .filter_map(|ix| {
            let node = tree.get_ix(ix)?;
            let slot = match tree.parent_link_of(ix) {
                Some(ParentLink::Nearby { slot, .. }) if Some(node.id) != tree.root_id() => {
                    Some(slot)
                }
                _ => None,
            };
            Some((
                node.id,
                node.lifecycle.mounted_at_revision,
                node.spec.kind,
                slot,
            ))
        })
        .collect()
    }
    /// The first entry of `attachment_path`.
    fn attachment_entry(
        tree: &ElementTree,
        id: &NodeId,
    ) -> Option<(NodeId, u64, ElementKind, Option<NearbySlot>)> {
        let ix = tree.ix_of(id)?;
        let node = tree.get_ix(ix)?;
        let slot = match tree.parent_link_of(ix) {
            Some(ParentLink::Nearby { slot, .. }) if Some(node.id) != tree.root_id() => Some(slot),
            _ => None,
        };
        Some((
            node.id,
            node.lifecycle.mounted_at_revision,
            node.spec.kind,
            slot,
        ))
    }
    pub(crate) fn attachment_changed(&self, tree: &ElementTree, id: &NodeId, axis: Axis) -> bool {
        tree.get(id)
            .is_some_and(|node| node.lifecycle.mounted_at_revision == self.mounted_at)
            && (*self.path != Self::attachment_path(tree, id)
                || self.dimensions[if axis == Axis::Width { 0 } else { 1 }].is_some_and(|fp| {
                    fp.scope
                        != super::super::layout::dimensions::declared_allocation_scope(
                            tree, id, axis,
                        )
                }))
    }
    pub(crate) fn valid_transport_origin(&self) -> bool {
        self.mounted_at == self.transport_origin.mounted_at
            && self.model == self.transport_origin.model
            && self.scale == self.transport_origin.scale
            && self.dimensions == self.transport_origin.dimensions
            && Arc::ptr_eq(&self.path, &self.transport_origin.path)
    }
    pub fn capture(tree: &ElementTree, id: &NodeId) -> Option<Self> {
        Self::capture_with_path(tree, id, || Arc::new(Self::attachment_path(tree, id)))
    }

    fn capture_with_path(
        tree: &ElementTree,
        id: &NodeId,
        path: impl FnOnce() -> Arc<AttachmentPath>,
    ) -> Option<Self> {
        let node = tree.get(id)?;
        // A mount that has never had layout has no presentation to interrupt.
        node.layout.frame?;
        let published = tree.publication.0?;
        if node.lifecycle.mounted_at_revision > published.revision {
            return None;
        }
        let dimensions = [Axis::Width, Axis::Height].map(|axis| capture_axis(tree, id, axis));
        let path = path();
        if path.last().map(|(id, mount, ..)| (*id, *mount)) != published.root {
            return None;
        }
        let transport_origin = Arc::new(TransportOrigin {
            mounted_at: node.lifecycle.mounted_at_revision,
            model: published.model,
            scale: node.layout.dimension_facts.scale,
            dimensions,
            path: Arc::clone(&path),
        });
        Some(Self {
            mounted_at: node.lifecycle.mounted_at_revision,
            model: published.model,
            declared: node.spec.declared.clone(),
            effective: node.layout.effective.clone(),
            scale: node.layout.dimension_facts.scale,
            path,
            dimensions,
            transport_origin,
            sampled: [Axis::Width, Axis::Height].map(|axis| {
                node.layout
                    .dimension_samples
                    .as_deref()
                    .is_some_and(|samples| samples.get(axis).is_some())
            }),
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct PatchEffects {
    pub invalidation: TreeInvalidation,
    pub model_changed: bool,
    pub sources: HashMap<NodeId, PresentationSource>,
}

impl ElementTree {
    /// Capture once before geometry/attrs are overwritten. A reused id with a new
    /// mount is a different source. The current declaration is the latest target;
    /// keeping a second target copy in this journal would be redundant history.
    pub(crate) fn capture_animation_source(&mut self, id: &NodeId) {
        self.capture_animation_source_with(id, |tree| {
            Arc::new(PresentationSource::attachment_path(tree, id))
        });
    }

    fn capture_animation_source_with(
        &mut self,
        id: &NodeId,
        path: impl FnOnce(&ElementTree) -> Arc<AttachmentPath>,
    ) {
        if self
            .pending_patch_effects
            .sources
            .get(id)
            .is_some_and(|source| {
                self.get(id)
                    .is_some_and(|node| node.lifecycle.mounted_at_revision == source.mounted_at)
            })
        {
            return;
        }
        if let Some(source) = PresentationSource::capture_with_path(self, id, || path(self)) {
            self.invalidate_unapplied_frame();
            self.pending_patch_effects.sources.insert(*id, source);
        }
    }

    // One Raven patch: a re-sort moves whole rows, and every node of each
    // moved subtree is captured. Each capture walked up to the tree root for
    // its attachment path, and the source map grew (rehashing its large
    // entries) as it filled: half of the capture time. A node's path is its
    // own entry followed by its parent's, so derive it top-down (the same
    // path, checked against the walk in tests), and size the map once.
    pub(crate) fn capture_animation_subtree(&mut self, root: &NodeId) {
        self.capture_subtree_sources(root, |_, _| true);
    }

    /// One Raven patch: capture for a subtree a Remove patch is about to drop.
    ///
    /// Every consumer reads the source map either as a whole (`is_empty`, and
    /// `context_release`'s "all sources still valid") or by the id of a node
    /// in the tree, checked against that node's mount; the one by-iteration
    /// read of a source's own fields asks for `animate_change`. Once the root's
    /// source is in the map (and invalid after the removal, as every removed
    /// source is), a removed descendant's source can change a result only if
    /// it has `animate_change` or its id comes back in this batch
    /// (`reinserted`). Capture just those: a 1,440-node row list re-sorted on
    /// the Stress screen cloned two `Attrs` and built a path for every node it
    /// dropped, 26% of the patch. (A frame that fails to commit keeps its
    /// sources for the next batch; an id reinserted there, before any commit,
    /// would miss its old source. Emerge never reuses a removed id.)
    pub(crate) fn capture_removed_animation_subtree(
        &mut self,
        root: &NodeId,
        reinserted: impl Fn(&NodeId) -> bool,
    ) {
        self.capture_animation_source(root);
        let root_captured = self
            .pending_patch_effects
            .sources
            .get(root)
            .zip(self.get(root))
            .is_some_and(|(source, node)| source.mounted_at == node.lifecycle.mounted_at_revision);
        if !root_captured {
            self.capture_animation_subtree(root);
            return;
        }
        self.capture_subtree_sources(root, |tree, id| {
            reinserted(id)
                || tree
                    .get(id)
                    .is_some_and(|node| node.spec.declared.animate_change.is_some())
        });
    }

    fn capture_subtree_sources(
        &mut self,
        root: &NodeId,
        wanted: impl Fn(&ElementTree, &NodeId) -> bool,
    ) {
        let mut nodes: Vec<(NodeId, Option<usize>)> = Vec::new();
        let mut pending = vec![(*root, None)];
        while let Some((id, parent)) = pending.pop() {
            let index = nodes.len();
            pending.extend(
                self.child_ids(&id)
                    .into_iter()
                    .map(|child| (child, Some(index))),
            );
            pending.extend(
                self.nearby_mounts_for(&id)
                    .into_iter()
                    .map(|mount| (mount.id, Some(index))),
            );
            nodes.push((id, parent));
        }
        // One Raven patch: a node that can't be captured (no frame yet, mounted
        // after the published frame, already captured for this mount) needs
        // no path, and a freshly inserted subtree is all such nodes; paths are
        // built only for the nodes captured, from the nearest built ancestor.
        let Some(published) = self.publication.0 else {
            return;
        };
        let capturable = |tree: &ElementTree, id: &NodeId| {
            tree.get(id).is_some_and(|node| {
                node.layout.frame.is_some()
                    && node.lifecycle.mounted_at_revision <= published.revision
                    && tree
                        .pending_patch_effects
                        .sources
                        .get(id)
                        .is_none_or(|source| {
                            source.mounted_at != node.lifecycle.mounted_at_revision
                        })
            })
        };
        let mut paths: Vec<Option<Arc<AttachmentPath>>> = vec![None; nodes.len()];
        let mut reserved = false;
        for index in 0..nodes.len() {
            let id = nodes[index].0;
            if !wanted(self, &id) || !capturable(self, &id) {
                continue;
            }
            #[cfg(any(test, feature = "bench-diagnostics"))]
            SUBTREE_CAPTURE_VISITS.with(|count| count.set(count.get() + 1));
            if !reserved {
                self.pending_patch_effects
                    .sources
                    .reserve(nodes.len() - index);
                reserved = true;
            }
            let path = self.subtree_attachment_path(&nodes, &mut paths, index);
            debug_assert_eq!(*path, PresentationSource::attachment_path(self, &id));
            self.capture_animation_source_with(&id, |_| path);
        }
    }

    /// The attachment path of `nodes[index]`, built on its nearest ancestor's
    /// (memoized in `paths`) as the walk to the root would build it.
    fn subtree_attachment_path(
        &self,
        nodes: &[(NodeId, Option<usize>)],
        paths: &mut [Option<Arc<AttachmentPath>>],
        index: usize,
    ) -> Arc<AttachmentPath> {
        let mut chain = vec![index];
        while let Some(&top) = chain.last() {
            match nodes[top].1 {
                Some(parent) if paths[top].is_none() => chain.push(parent),
                _ => break,
            }
        }
        for &at in chain.iter().rev() {
            if paths[at].is_some() {
                continue;
            }
            let (id, parent) = nodes[at];
            let entry = PresentationSource::attachment_entry(self, &id);
            let path = match (parent, entry) {
                (Some(parent), Some(entry)) if self.root_id() != Some(id) => {
                    let parent_path = paths[parent].as_ref().expect("ancestor path built first");
                    let mut path = Vec::with_capacity(parent_path.len() + 1);
                    path.push(entry);
                    path.extend(parent_path.iter().copied());
                    Arc::new(path)
                }
                _ => Arc::new(PresentationSource::attachment_path(self, &id)),
            };
            paths[at] = Some(path);
        }
        Arc::clone(paths[index].as_ref().expect("path built"))
    }

    pub(crate) fn finish_patch_frame(&mut self) {
        self.pending_patch_effects = PatchEffects::default();
    }
}

#[cfg(any(test, feature = "bench-diagnostics"))]
thread_local! {
    /// One Raven patch: nodes `capture_subtree_sources` built a source for.
    pub(crate) static SUBTREE_CAPTURE_VISITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests;
