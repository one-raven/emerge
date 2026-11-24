//! Render an ElementTree into a render scene.
//!
//! Reads from pre-scaled attrs (scaling is applied in the layout pass).

mod box_model;
mod color;
mod paint;
mod registry_walk;
mod text;

use self::paint::{
    build_background_nodes, collect_border_nodes, collect_box_shadow_nodes,
    collect_scrollbar_nodes, render_image_nodes, render_video_nodes,
};
use self::registry_walk::{
    BuildRegistryTraversal, HostRegistryTraversalSink, RegistryTraversalSink,
    ReuseCleanRegistryTraversal, children_scene_context, nearby_scene_context,
};
use self::text::{
    TextDecorationSpec, render_multiline_text_input_items, render_text_input_items,
    render_text_items, text_decoration_items,
};
use super::animation::animation_spec_is_compositor_only;
use super::attrs::{Attrs, effective_scrollbar_x, effective_scrollbar_y};
use super::element::{
    Element, ElementKind, ElementTree, Frame, LayerRewrite, LayerRewriteOp, NearbySlot, NodeIx,
    RenderFragmentCache,
    RenderFragmentCacheKey, RenderFragmentCacheKind, RetainedChildMode, RetainedLocalBranchRef,
};
use super::geometry::{ClipShape, Rect, host_clip_shape, self_shape as geometry_self_shape};
use super::layout::FontContext;
use super::scene::{ResolvedNodeState, SceneContext, resolve_node_state};
use super::transform::{Affine2, element_transform};
use super::viewport_culling::{
    should_skip_render_viewport_subtree, should_skip_resolved_viewport_subtree,
};
#[cfg(test)]
use crate::events::registry_builder;
use crate::events::{RegistryRebuildPayload, registry_builder::RegistryRefreshCollector};
use crate::render_scene::{
    DrawPrimitive, PaintLayerHashFloat, PaintLayerId, PaintLayerPlacement, PaintLayerPolicy,
    PaintLayerReason, RenderNode, RenderPaintLayer, RenderPaintLayerBuildParts,
    RenderPaintLayerContent, RenderPaintLayerContentNode, RenderScene, hash_paint_layer_render_nodes,
    paint_layer_bounds_from_visual_bounds, paint_layer_own_content_visual_bounds,
};
use crate::renderer::{make_font_with_style, measure_text_visual_metrics_with_font};
#[cfg(any(test, feature = "bench-diagnostics"))]
use std::cell::Cell;
use std::cell::RefCell;
// One Raven patch: the fragment keys (built for every element on every
// refresh: the context's clips, bounds, font) and layer content generations
// use the crate's Fx hasher, as the renderer's per-frame keys do; SipHash was
// ~10% of building a newly inserted screen's scene.
use crate::fast_hash::FastHasher;
use std::hash::Hasher;
use std::rc::Rc;
use std::sync::Arc;

#[cfg(any(test, feature = "bench-diagnostics"))]
thread_local! {
    static RENDER_TRAVERSAL_DIAGNOSTICS_ENABLED: Cell<bool> = const { Cell::new(false) };
    static RENDER_TRAVERSAL_DIAGNOSTICS: Cell<RenderTraversalDiagnostics> = const {
        Cell::new(RenderTraversalDiagnostics::empty())
    };
}

#[cfg(any(test, feature = "bench-diagnostics"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderTraversalDiagnostics {
    pub element_visits: u64,
    pub culled_subtrees: u64,
}

#[cfg(any(test, feature = "bench-diagnostics"))]
impl RenderTraversalDiagnostics {
    const fn empty() -> Self {
        Self {
            element_visits: 0,
            culled_subtrees: 0,
        }
    }

    fn with_element_visit(mut self) -> Self {
        self.element_visits = self.element_visits.saturating_add(1);
        self
    }

    fn with_culled_subtree(mut self) -> Self {
        self.culled_subtrees = self.culled_subtrees.saturating_add(1);
        self
    }
}

#[cfg(any(test, feature = "bench-diagnostics"))]
#[doc(hidden)]
pub fn reset_render_traversal_diagnostics_for_benchmark() {
    RENDER_TRAVERSAL_DIAGNOSTICS.with(|diagnostics| {
        diagnostics.set(RenderTraversalDiagnostics::empty());
    });
    RENDER_TRAVERSAL_DIAGNOSTICS_ENABLED.with(|enabled| enabled.set(true));
}

#[cfg(any(test, feature = "bench-diagnostics"))]
#[doc(hidden)]
pub fn take_render_traversal_diagnostics_for_benchmark() -> RenderTraversalDiagnostics {
    RENDER_TRAVERSAL_DIAGNOSTICS_ENABLED.with(|enabled| enabled.set(false));
    RENDER_TRAVERSAL_DIAGNOSTICS.with(Cell::get)
}

#[cfg(any(test, feature = "bench-diagnostics"))]
fn update_render_traversal_diagnostics(
    update: impl FnOnce(RenderTraversalDiagnostics) -> RenderTraversalDiagnostics,
) {
    RENDER_TRAVERSAL_DIAGNOSTICS_ENABLED.with(|enabled| {
        if enabled.get() {
            RENDER_TRAVERSAL_DIAGNOSTICS.with(|diagnostics| {
                diagnostics.set(update(diagnostics.get()));
            });
        }
    });
}

#[cfg(any(test, feature = "bench-diagnostics"))]
fn record_render_traversal_element_visit() {
    update_render_traversal_diagnostics(RenderTraversalDiagnostics::with_element_visit);
}

#[cfg(not(any(test, feature = "bench-diagnostics")))]
fn record_render_traversal_element_visit() {}

#[cfg(any(test, feature = "bench-diagnostics"))]
fn record_render_traversal_culled_subtree() {
    update_render_traversal_diagnostics(RenderTraversalDiagnostics::with_culled_subtree);
}

#[cfg(not(any(test, feature = "bench-diagnostics")))]
fn record_render_traversal_culled_subtree() {}

#[cfg(test)]
pub(crate) struct RenderOutput {
    pub scene: RenderScene,
    pub event_rebuild: RegistryRebuildPayload,
    pub text_input_focused: bool,
    pub text_input_cursor_area: Option<(f32, f32, f32, f32)>,
}

#[cfg(test)]
pub(crate) struct RenderSceneOutput {
    pub scene: RenderScene,
    pub text_input_focused: bool,
    pub text_input_cursor_area: Option<(f32, f32, f32, f32)>,
}

pub(crate) struct RefreshBuildOutput {
    pub scene: RenderScene,
    pub registry: RefreshRegistryOutput,
    pub text_input_focused: bool,
    pub text_input_cursor_area: Option<(f32, f32, f32, f32)>,
}

#[allow(clippy::large_enum_variant)]
pub(crate) enum RefreshRegistryOutput {
    Rebuilt(RegistryRebuildPayload),
    ReusedClean,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum RefreshRegistryMode {
    #[allow(dead_code)]
    Rebuild,
    ReuseClean,
}

#[derive(Clone, Copy, Debug)]
struct HostClipDescriptor {
    clip: ClipShape,
    scroll_x: bool,
    scroll_y: bool,
}

#[derive(Clone, Debug, Default)]
struct RenderBuildContext {
    scene_bounds: Rect,
    inherited_host_clips: Arc<[HostClipDescriptor]>,
    inherited_clip_shapes: Arc<[ClipShape]>,
    inherited_self_clips: Arc<[ClipShape]>,
    nearby_host_clips: Arc<[HostClipDescriptor]>,
    nearby_clip_shapes: Arc<[ClipShape]>,
    nearby_self_clips: Arc<[ClipShape]>,
}

fn shadow_boundary_clip(clip: HostClipDescriptor, scene_bounds: Rect) -> ClipShape {
    match (clip.scroll_x, clip.scroll_y) {
        (true, true) => clip.clip,
        (true, false) => ClipShape {
            rect: Rect {
                x: clip.clip.rect.x,
                y: scene_bounds.y,
                width: clip.clip.rect.width,
                height: scene_bounds.height,
            },
            radii: None,
        },
        (false, true) => ClipShape {
            rect: Rect {
                x: scene_bounds.x,
                y: clip.clip.rect.y,
                width: scene_bounds.width,
                height: clip.clip.rect.height,
            },
            radii: None,
        },
        (false, false) => clip.clip,
    }
}

fn arc_slice_with<T: Clone>(items: &[T], item: T) -> Arc<[T]> {
    let mut next = Vec::with_capacity(items.len() + 1);
    next.extend_from_slice(items);
    next.push(item);
    Arc::from(next.into_boxed_slice())
}

impl RenderBuildContext {
    fn with_host_clip(
        &self,
        clip: HostClipDescriptor,
        self_clip: ClipShape,
        clip_nearby: bool,
    ) -> Self {
        let inherited_host_clips = arc_slice_with(&self.inherited_host_clips, clip);
        let inherited_clip_shapes = arc_slice_with(&self.inherited_clip_shapes, clip.clip);
        let inherited_self_clips = arc_slice_with(&self.inherited_self_clips, self_clip);
        let nearby_host_clips = if clip_nearby {
            arc_slice_with(&self.nearby_host_clips, clip)
        } else {
            self.nearby_host_clips.clone()
        };
        let nearby_clip_shapes = if clip_nearby {
            arc_slice_with(&self.nearby_clip_shapes, clip.clip)
        } else {
            self.nearby_clip_shapes.clone()
        };
        let nearby_self_clips = if clip_nearby {
            arc_slice_with(&self.nearby_self_clips, self_clip)
        } else {
            self.nearby_self_clips.clone()
        };
        Self {
            scene_bounds: self.scene_bounds,
            inherited_host_clips,
            inherited_clip_shapes,
            inherited_self_clips,
            nearby_host_clips,
            nearby_clip_shapes,
            nearby_self_clips,
        }
    }

    fn without_host_clips(&self) -> Self {
        Self {
            scene_bounds: self.scene_bounds,
            inherited_host_clips: self.nearby_host_clips.clone(),
            inherited_clip_shapes: self.nearby_clip_shapes.clone(),
            inherited_self_clips: self.nearby_self_clips.clone(),
            nearby_host_clips: self.nearby_host_clips.clone(),
            nearby_clip_shapes: self.nearby_clip_shapes.clone(),
            nearby_self_clips: self.nearby_self_clips.clone(),
        }
    }

    fn within_local_transform(&self) -> Self {
        Self {
            scene_bounds: self.scene_bounds,
            ..Self::default()
        }
    }

    fn full_clip_shapes(&self) -> &[ClipShape] {
        &self.inherited_clip_shapes
    }

    fn shadow_clip_shapes(&self) -> Vec<ClipShape> {
        self.inherited_host_clips
            .iter()
            .filter(|clip| clip.scroll_x || clip.scroll_y)
            .map(|clip| shadow_boundary_clip(*clip, self.scene_bounds))
            .collect()
    }

    fn nearest_self_clip(&self) -> Option<ClipShape> {
        self.inherited_self_clips.last().copied()
    }
}

struct RenderOutputs<'a> {
    text_input_focused: &'a mut bool,
    text_input_cursor_area: &'a mut Option<(f32, f32, f32, f32)>,
}

impl<'a> RenderOutputs<'a> {
    fn reborrow(&mut self) -> RenderOutputs<'_> {
        let text_input_focused = &mut *self.text_input_focused;
        let text_input_cursor_area = &mut *self.text_input_cursor_area;

        RenderOutputs {
            text_input_focused,
            text_input_cursor_area,
        }
    }
}

#[derive(Clone)]
struct RenderTraversal<'a> {
    scene_ctx: SceneContext,
    render_ctx: &'a RenderBuildContext,
    disable_viewport_culling: bool,
    inside_cacheable_semantic_layer: bool,
    is_root: bool,
    /// One Raven patch: set while building the content of a scroll container
    /// (see `ScrollCanon`); the element's local nodes come back canonical.
    scroll_canon: Option<Rc<ScrollCanon>>,
}

impl<'a> RenderTraversal<'a> {
    fn for_host_content<'b>(
        &self,
        render_ctx: &'b RenderBuildContext,
        disable_viewport_culling: bool,
        inside_cacheable_semantic_layer: bool,
    ) -> RenderTraversal<'b> {
        RenderTraversal {
            scene_ctx: self.scene_ctx.clone(),
            render_ctx,
            disable_viewport_culling,
            inside_cacheable_semantic_layer,
            is_root: false,
            scroll_canon: self.scroll_canon.clone(),
        }
    }

    fn for_child<'b>(
        &self,
        scene_ctx: SceneContext,
        render_ctx: &'b RenderBuildContext,
    ) -> RenderTraversal<'b> {
        RenderTraversal {
            scene_ctx,
            render_ctx,
            disable_viewport_culling: self.disable_viewport_culling,
            inside_cacheable_semantic_layer: self.inside_cacheable_semantic_layer,
            is_root: false,
            scroll_canon: self.scroll_canon.clone(),
        }
    }

    fn for_branch<'b>(&self, render_ctx: &'b RenderBuildContext) -> RenderTraversal<'b> {
        self.for_child(self.scene_ctx.clone(), render_ctx)
    }

    fn for_scene_context(&self, scene_ctx: SceneContext) -> RenderTraversal<'a> {
        self.for_child(scene_ctx, self.render_ctx)
    }

    fn within_cacheable_semantic_layer(mut self) -> Self {
        self.inside_cacheable_semantic_layer = true;
        self.is_root = false;
        self
    }

    fn with_scroll_canon(mut self, scroll_canon: Option<Rc<ScrollCanon>>) -> Self {
        self.scroll_canon = scroll_canon;
        self
    }

    /// Raw nodes, for a branch whose output is canonicalized as a whole.
    fn raw(self) -> Self {
        self.with_scroll_canon(None)
    }
}

struct HostContentBuild<'a> {
    element: &'a Element,
    element_ix: NodeIx,
    render_frame: Frame,
    element_context: &'a FontContext,
    scene_state: Option<ResolvedNodeState>,
}

#[derive(Clone, Default)]
struct RenderSubtree {
    local: Vec<RenderNode>,
    escapes: Vec<RenderNode>,
    text_input_focused: bool,
    text_input_cursor_area: Option<(f32, f32, f32, f32)>,
}

impl RenderSubtree {
    fn extend_local(&mut self, subtree: RenderSubtree) {
        self.merge_outputs(&subtree);
        let RenderSubtree { local, escapes, .. } = subtree;
        self.local.extend(local);
        self.escapes.extend(escapes);
    }

    fn extend_escape(&mut self, subtree: RenderSubtree) {
        self.merge_outputs(&subtree);
        let RenderSubtree { local, escapes, .. } = subtree;
        self.escapes.extend(local);
        self.escapes.extend(escapes);
    }

    fn merge_outputs(&mut self, subtree: &RenderSubtree) {
        self.text_input_focused |= subtree.text_input_focused;
        if self.text_input_cursor_area.is_none() {
            self.text_input_cursor_area = subtree.text_input_cursor_area;
        }
    }

    fn into_nodes(self) -> Vec<RenderNode> {
        let mut nodes = self.local;
        nodes.extend(self.escapes);
        nodes
    }

    fn from_fragment_cache(cache: &RenderFragmentCache) -> Self {
        Self {
            local: cache.local.clone(),
            escapes: cache.escapes.clone(),
            text_input_focused: cache.text_input_focused,
            text_input_cursor_area: cache.text_input_cursor_area,
        }
    }

    fn to_fragment_cache(&self, key: RenderFragmentCacheKey) -> RenderFragmentCache {
        RenderFragmentCache {
            key,
            local: self.local.clone(),
            escapes: self.escapes.clone(),
            text_input_focused: self.text_input_focused,
            text_input_cursor_area: self.text_input_cursor_area,
        }
    }
}

fn apply_subtree_outputs(outputs: &mut RenderOutputs<'_>, subtree: &RenderSubtree) {
    if subtree.text_input_focused {
        *outputs.text_input_focused = true;
    }
    if outputs.text_input_cursor_area.is_none() {
        *outputs.text_input_cursor_area = subtree.text_input_cursor_area;
    }
}

/// Render the tree without rebuilding event registry metadata and without using
/// paint-layer caches.
///
/// Reads from pre-scaled attrs (layout pass must run first). This is kept as a
/// safe baseline for dirty animation refreshes, correctness tests, and
/// performance regression benchmarks.
#[cfg(test)]
pub(crate) fn render_tree_scene(tree: &ElementTree) -> RenderSceneOutput {
    render_tree_scene_with_scroll_layers(tree)
}

#[cfg(test)]
pub(crate) fn render_tree_scene_with_scroll_layers(tree: &ElementTree) -> RenderSceneOutput {
    let output = build_refresh_output_with_semantic_layers(tree, RefreshRegistryMode::ReuseClean);
    RenderSceneOutput {
        scene: output.scene,
        text_input_focused: output.text_input_focused,
        text_input_cursor_area: output.text_input_cursor_area,
    }
}

pub(crate) fn build_refresh_output_with_scroll_layers(
    tree: &ElementTree,
    registry_mode: RefreshRegistryMode,
) -> RefreshBuildOutput {
    build_refresh_output_with_semantic_layers(tree, registry_mode)
}

fn build_refresh_output_with_semantic_layers(
    tree: &ElementTree,
    registry_mode: RefreshRegistryMode,
) -> RefreshBuildOutput {
    let _asset_guard = tree.frame_assets.as_ref().map(|assets| assets.enter());
    let mut registry_collector = match registry_mode {
        RefreshRegistryMode::Rebuild => Some(RegistryRefreshCollector::for_tree(tree)),
        RefreshRegistryMode::ReuseClean => None,
    };
    let Some(root_ix) = tree.root_ix() else {
        return RefreshBuildOutput {
            scene: RenderScene::default(),
            registry: registry_collector
                .map(|collector| RefreshRegistryOutput::Rebuilt(collector.finish(tree)))
                .unwrap_or(RefreshRegistryOutput::ReusedClean),
            text_input_focused: false,
            text_input_cursor_area: None,
        };
    };

    let mut text_input_focused = false;
    let mut text_input_cursor_area = None;
    let render_ctx = RenderBuildContext {
        scene_bounds: scene_bounds_for_root(tree, root_ix),
        ..RenderBuildContext::default()
    };
    let mut outputs = RenderOutputs {
        text_input_focused: &mut text_input_focused,
        text_input_cursor_area: &mut text_input_cursor_area,
    };
    let traversal = RenderTraversal {
        scene_ctx: SceneContext::default(),
        render_ctx: &render_ctx,
        disable_viewport_culling: false,
        inside_cacheable_semantic_layer: false,
        is_root: true,
        scroll_canon: None,
    };
    let subtree = match registry_collector.as_mut() {
        Some(collector) => build_element_subtree(
            tree,
            root_ix,
            &FontContext::default(),
            &mut outputs,
            traversal,
            BuildRegistryTraversal::root(collector),
        ),
        None => build_element_subtree(
            tree,
            root_ix,
            &FontContext::default(),
            &mut outputs,
            traversal,
            ReuseCleanRegistryTraversal,
        ),
    };

    let nodes = wrap_with_root_paint_layer(subtree.into_nodes(), tree.get_ix(root_ix));
    RefreshBuildOutput {
        scene: RenderScene {
            fonts: tree.frame_fonts.clone(),
            images: Some(tree.frame_assets.as_ref().map_or_else(
                || crate::renderer::ImageSnapshot::capture(&nodes),
                |assets| assets.images.referenced(&nodes),
            )),
            nodes,
        },
        registry: registry_collector
            .map(|collector| RefreshRegistryOutput::Rebuilt(collector.finish(tree)))
            .unwrap_or(RefreshRegistryOutput::ReusedClean),
        text_input_focused,
        text_input_cursor_area,
    }
}

/// Render the tree and collect rebuild metadata.
/// Reads from pre-scaled attrs (layout pass must run first).
#[cfg(test)]
pub(crate) fn render_tree(tree: &ElementTree) -> RenderOutput {
    let scene_output = render_tree_scene(tree);

    RenderOutput {
        scene: scene_output.scene,
        event_rebuild: registry_builder::build_registry_rebuild(tree),
        text_input_focused: scene_output.text_input_focused,
        text_input_cursor_area: scene_output.text_input_cursor_area,
    }
}

fn build_element_subtree<R: RegistryTraversalSink>(
    tree: &ElementTree,
    ix: NodeIx,
    inherited: &FontContext,
    outputs: &mut RenderOutputs<'_>,
    traversal: RenderTraversal<'_>,
    mut registry: R,
) -> RenderSubtree {
    let Some(element) = tree.get_ix(ix) else {
        return RenderSubtree::default();
    };
    let Some(frame) = element.layout.frame else {
        return RenderSubtree::default();
    };
    record_render_traversal_element_visit();

    let attrs = &element.layout.effective;
    let radius = attrs.border_radius.as_ref();
    let scene_state = resolve_node_state(element, traversal.scene_ctx.clone());
    let render_frame = scene_state
        .as_ref()
        .map(|state| state.adjusted_render_frame)
        .unwrap_or(frame);
    let transform = element_transform(render_frame, attrs);
    let alpha = attrs.alpha.unwrap_or(1.0) as f32;

    if !traversal.disable_viewport_culling
        && should_cull_render_subtree(
            tree,
            ix,
            attrs,
            render_frame,
            transform,
            &traversal.scene_ctx,
        )
    {
        registry.collect_registry_for_render_skipped_subtree(tree, ix);
        return RenderSubtree::default();
    }

    // One Raven patch: reuse this subtree's previous build when nothing in it
    // changed and it is built in the same context. Upstream rebuilds every
    // element on every animation frame, so one spinner costs as much as the
    // whole screen. A transform animation invalidates only the animated
    // element and its ancestors, so everything inside it is reused.
    let element_cache_key = (!traversal.is_root)
        .then(|| element_render_fragment_cache_key(tree, ix, element, &traversal, inherited))
        .flatten();
    if let Some(cache_key) = element_cache_key
        && !(element.refresh.render_dirty || element.refresh.render_descendant_dirty)
        && let Some(cached) = element
            .refresh
            .render_fragment_cache
            .borrow()
            .as_ref()
            .filter(|cache| cache.key == cache_key)
            .cloned()
    {
        registry.collect_registry_for_render_skipped_subtree(tree, ix);
        let subtree = RenderSubtree::from_fragment_cache(&cached);
        apply_subtree_outputs(outputs, &subtree);
        return subtree;
    }

    let host_registry = registry.visit_element(tree, element, scene_state.as_ref());
    let element_context = inherited.merge_with_attrs(attrs);
    let declared_animation = !traversal.is_root && element_has_declared_animation(element);
    let scroll_content = is_declared_scroll_container(element);
    let semantic_descendants =
        traversal.inside_cacheable_semantic_layer || declared_animation || scroll_content;

    let current_host_clip = HostClipDescriptor {
        clip: scene_state
            .as_ref()
            .map(|state| state.host_clip)
            .unwrap_or_else(|| host_clip_shape(render_frame, attrs)),
        scroll_x: effective_scrollbar_x(attrs),
        scroll_y: effective_scrollbar_y(attrs),
    };
    let current_self_shape = geometry_self_shape(render_frame, attrs);
    let child_render_ctx = traversal.render_ctx.with_host_clip(
        current_host_clip,
        ClipShape {
            rect: current_self_shape.rect,
            radii: current_self_shape.radii,
        },
        attrs.clip_nearby.unwrap_or(false),
    );
    let local_transform_render_ctx = (declared_animation || !transform.is_identity())
        .then(|| traversal.render_ctx.within_local_transform());
    let host_content_render_ctx = local_transform_render_ctx
        .as_ref()
        .unwrap_or(traversal.render_ctx);
    // One Raven patch: inside a scroll container's content, an element whose
    // own wrappers commute with canonicalization (no transform, layer or
    // media clip) builds its nodes canonical, from its children's canonical
    // nodes. Any other element builds raw and canonicalizes its own output.
    // A scroll container starts its own canonical context for its content.
    let outer_canon = traversal.scroll_canon.clone();
    let distributive = outer_canon.is_some()
        && !scroll_content
        && !declared_animation
        && transform.is_identity()
        && !matches!(element.spec.kind, ElementKind::Image | ElementKind::Video);
    let scroll_canon = scroll_content.then(|| {
        Rc::new(ScrollCanon::take(
            element,
            child_render_ctx.inherited_clip_shapes.clone(),
        ))
    });
    let canonicalize_after_build = canonicalize_scroll_content_after_build();
    let content_canon = if scroll_content {
        scroll_canon.clone().filter(|_| !canonicalize_after_build)
    } else if distributive {
        outer_canon.clone()
    } else {
        None
    };
    let mut host_content = build_host_content_subtree(
        tree,
        HostContentBuild {
            element,
            element_ix: ix,
            render_frame,
            element_context: &element_context,
            scene_state: scene_state.clone(),
        },
        &mut outputs.reborrow(),
        traversal
            .for_host_content(
                host_content_render_ctx,
                traversal.disable_viewport_culling,
                semantic_descendants,
            )
            .with_scroll_canon(content_canon),
        host_registry,
    );
    if let Some(canon) = scroll_canon {
        if canonicalize_after_build {
            host_content.local = canon.nodes(std::mem::take(&mut host_content.local));
        }
        canon.store(element);
    }
    let own_canon = outer_canon.as_ref().filter(|_| distributive);
    let canonical = |nodes: Vec<RenderNode>| match own_canon {
        Some(canon) => canon.nodes(nodes),
        None => nodes,
    };

    let outer_shadow_nodes = collect_box_shadow_nodes(render_frame, attrs, radius, false);
    let background_nodes = build_background_nodes(render_frame, attrs);
    let inset_shadow_nodes = collect_box_shadow_nodes(render_frame, attrs, radius, true);
    let border_nodes = collect_border_nodes(render_frame, attrs);
    let scrollbar_nodes = collect_scrollbar_nodes(scene_state.as_ref(), render_frame, attrs);
    let inherited_host_clips = traversal.render_ctx.full_clip_shapes();
    let inherited_self_clip = traversal.render_ctx.nearest_self_clip();

    // One Raven patch: an element the renderer animates keeps explicit
    // Transform/Alpha nodes even at identity, so the render thread can find
    // and update them on the retained scene.
    let renderer_animation = (declared_animation && !traversal.is_root)
        .then(|| element.layout.renderer_animation.clone())
        .flatten();
    let force_wrappers = renderer_animation.is_some();

    let mut local = if force_wrappers {
        wrap_with_shadow_pass(wrap_with_clips(
            wrap_with_transform_forced(outer_shadow_nodes, transform),
            &traversal.render_ctx.shadow_clip_shapes(),
        ))
    } else {
        canonical(wrap_outer_shadow_nodes(
            outer_shadow_nodes,
            transform,
            traversal.render_ctx,
        ))
    };
    let mut normal_nodes = Vec::new();
    normal_nodes.extend(canonical(background_nodes));
    normal_nodes.extend(canonical(inset_shadow_nodes));

    if scroll_content {
        normal_nodes.extend(wrap_semantic_scroll_content_layer(
            host_content.local,
            element,
            render_frame,
            current_host_clip,
            traversal.render_ctx.scene_bounds,
        ));
        normal_nodes.extend(wrap_with_host_clip(scrollbar_nodes, current_host_clip.clip));
    } else {
        normal_nodes.extend(host_content.local);
        normal_nodes.extend(canonical(scrollbar_nodes));
    }
    normal_nodes.extend(canonical(border_nodes));

    // An image or video without its own box is clipped to its parent's
    // shape. One Raven patch: that clip is applied inside the element's
    // transform, so it turns and scales with the image; outside it, a rotated
    // icon was cut to its parent's unrotated box.
    let image_self_clip = if matches!(element.spec.kind, ElementKind::Image | ElementKind::Video)
        && !image_video_needs_own_host_clip(attrs)
    {
        inherited_self_clip
    } else {
        None
    };
    let wrap_transform = if force_wrappers {
        wrap_with_transform_forced
    } else {
        wrap_with_transform
    };
    let normal_nodes = if let Some(canon) = own_canon {
        canon.wrap_with_clips(normal_nodes, inherited_host_clips)
    } else if let Some(clip) = image_self_clip {
        wrap_transform(wrap_with_relaxed_clips(normal_nodes, &[clip]), transform)
    } else if matches!(element.spec.kind, ElementKind::Image | ElementKind::Video) {
        wrap_with_relaxed_clips(wrap_transform(normal_nodes, transform), inherited_host_clips)
    } else {
        wrap_with_clips(
            wrap_transform(normal_nodes, transform),
            inherited_host_clips,
        )
    };
    local.extend(normal_nodes);

    if element.spec.kind == ElementKind::Video
        && (traversal.inside_cacheable_semantic_layer || declared_animation)
        && !local.is_empty()
    {
        local = wrap_with_paint_layer(
            local,
            element.id.to_wire_u64(),
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::DirectOnly,
            PaintLayerReason::DirectMedia,
            render_frame,
            Some(0),
        );
    }

    if declared_animation && !local.is_empty() {
        let wrapped = if force_wrappers {
            wrap_with_alpha_forced(local, alpha)
        } else {
            wrap_with_alpha(local, alpha)
        };
        local = wrap_with_compositor_animation_layer(wrapped, element, render_frame);
        if let Some(animation) = renderer_animation {
            attach_renderer_animation(&mut local, animation, attrs, render_frame, transform, alpha);
        }
    } else {
        local = wrap_with_alpha(local, alpha);
    }
    if let Some(canon) = outer_canon.as_ref().filter(|_| !distributive) {
        local = canon.nodes(local);
    }

    // The local transform context dropped the outer clip_nearby clips.
    let escape_clips: &[ClipShape] = if local_transform_render_ctx.is_some() {
        &traversal.render_ctx.nearby_clip_shapes
    } else {
        &[]
    };
    let escapes = wrap_with_clips(
        wrap_with_transform(host_content.escapes, transform),
        escape_clips,
    );

    let subtree = RenderSubtree {
        local,
        escapes: wrap_with_alpha(escapes, alpha),
        text_input_focused: false,
        text_input_cursor_area: None,
    };
    if let Some(cache_key) = element_cache_key
        && render_subtree_has_cacheable_fragment(&subtree)
    {
        element
            .refresh
            .render_fragment_cache
            .borrow_mut()
            .replace(subtree.to_fragment_cache(cache_key));
    }
    subtree
}

// One Raven patch: the key for reusing an element's subtree. Everything the
// build reads from outside the element: the nearby key's scroll offset, clips
// and topology, plus the scene bounds (culling), interaction transform,
// inherited font and traversal flags.
fn element_render_fragment_cache_key(
    tree: &ElementTree,
    ix: NodeIx,
    element: &Element,
    traversal: &RenderTraversal<'_>,
    inherited: &FontContext,
) -> Option<RenderFragmentCacheKey> {
    let mut key = nearby_render_fragment_cache_key(
        tree,
        ix,
        element,
        &traversal.scene_ctx,
        traversal.render_ctx,
    )?;
    let mut hasher = FastHasher::new();
    hasher.write_u64(key.context);
    hash_rect_bits(&mut hasher, traversal.render_ctx.scene_bounds);
    // Only the linear part: the render nodes are in layout coordinates, and
    // culling maps the element and its clip through the same inherited
    // transform, so moving an ancestor (a dragged sheet) leaves both alone.
    let to_screen = traversal.scene_ctx.interaction_transform;
    for part in [to_screen.xx, to_screen.yx, to_screen.xy, to_screen.yy] {
        hash_f32_bits(&mut hasher, part);
    }
    hasher.write_usize(traversal.scene_ctx.interaction_clips.len());
    hash_font_context(&mut hasher, inherited);
    hasher.write_u8(traversal.disable_viewport_culling as u8);
    hasher.write_u8(traversal.inside_cacheable_semantic_layer as u8);
    // One Raven patch: canonical scroll content nodes differ from raw ones.
    if let Some(canon) = &traversal.scroll_canon {
        hasher.write_u8(1);
        hasher.write_usize(canon.clips.len());
        hash_f32_bits(&mut hasher, canon.dx);
        hash_f32_bits(&mut hasher, canon.dy);
    }
    key.kind = RenderFragmentCacheKind::Element;
    key.context = hasher.finish();
    Some(key)
}

// One Raven patch: hash the inherited font field by field. The key used to
// hash `format!("{:?}", inherited)`, a string built for every element visited
// on every refresh (5% of a one-text patch on a 1,400-node screen).
fn hash_font_context(hasher: &mut FastHasher, font: &FontContext) {
    use std::hash::Hash;
    let FontContext {
        font_family,
        font_weight,
        font_italic,
        font_size,
        font_color,
        font_underline,
        font_strike,
        font_letter_spacing,
        font_word_spacing,
        text_align,
    } = font;
    font_family.hash(hasher);
    font_weight.hash(hasher);
    font_italic.hash(hasher);
    font_color.hash(hasher);
    font_underline.hash(hasher);
    font_strike.hash(hasher);
    for value in [font_size, font_letter_spacing, font_word_spacing] {
        value.map(f32::to_bits).hash(hasher);
    }
    text_align.map(|align| align as u8).hash(hasher);
}

// `canonical_nodes`: the content, built canonical (see `ScrollCanon`).
fn wrap_semantic_scroll_content_layer(
    canonical_nodes: Vec<RenderNode>,
    element: &Element,
    render_frame: Frame,
    host_clip: HostClipDescriptor,
    scene_bounds: Rect,
) -> Vec<RenderNode> {
    let scroll_x = element.layout.scroll_x;
    let scroll_y = element.layout.scroll_y;
    let nominal_bounds = Rect::from_frame(render_frame);
    let content_bounds = paint_layer_bounds_from_visual_bounds(
        paint_layer_own_content_visual_bounds(&canonical_nodes),
        nominal_bounds,
    );
    let canonical_host_clip = host_clip.clip.offset(-scroll_x, -scroll_y);
    let canonical_shadow_clip =
        shadow_boundary_clip(host_clip, scene_bounds).offset(-scroll_x, -scroll_y);
    let canonical_nodes =
        wrap_scroll_content_with_clips(canonical_nodes, canonical_host_clip, canonical_shadow_clip);
    let content = RenderPaintLayerContent::from_composition_nodes(canonical_nodes);
    let content_generation =
        paint_layer_content_generation(&content.own_payload_render_nodes(), content_bounds);
    let layer = RenderPaintLayer::from_prepared_children(
        RenderPaintLayerBuildParts {
            id: PaintLayerId::new(element.id.to_wire_u64(), PaintLayerReason::ScrollContent),
            bounds: content_bounds,
            placement: PaintLayerPlacement::ScrollMoving,
            policy: PaintLayerPolicy::Cacheable,
            content_generation,
            visual_bounds: Some(content_bounds),
        },
        content,
    );
    wrap_with_transform(
        vec![RenderNode::PaintLayer(layer)],
        Affine2::translation(-scroll_x, -scroll_y),
    )
}

// One Raven patch: clip canonical scroll content to its container. A node
// holding a shadow escapes the clip (`wrap_with_clips`), which is right for a
// bare `ShadowPass`: it carries the container's shadow boundary from where it
// was built. A node that mixes a shadow with ordinary content (the alpha,
// transform or animation layer of an element with a shadowed descendant)
// escaped whole, and canonicalizing had stripped the container's clip from
// the content inside it, so a faded, rotated or `animate_enter` wrapper
// around a shadowed card was drawn outside the scroll viewport (the Media
// hero art over the status bar). Such a node is now clipped to the
// container's shadow boundary: the viewport on the scrolling axis, as a bare
// shadow is. Upstream bug.
fn wrap_scroll_content_with_clips(
    nodes: Vec<RenderNode>,
    host_clip: ClipShape,
    shadow_clip: ClipShape,
) -> Vec<RenderNode> {
    if !nodes.iter().any(render_node_has_shadow_escape) {
        return wrap_with_host_clip(nodes, host_clip);
    }
    let mut out = Vec::new();
    let mut clipped = Vec::new();
    for node in nodes {
        if matches!(node, RenderNode::ShadowPass { .. }) {
            push_clipped_group(&mut out, &[host_clip], false, &mut clipped);
            out.push(node);
        } else if render_node_has_shadow_escape(&node) {
            push_clipped_group(&mut out, &[host_clip], false, &mut clipped);
            out.push(clip_node(vec![shadow_clip], false, vec![node]));
        } else {
            clipped.push(node);
        }
    }
    push_clipped_group(&mut out, &[host_clip], false, &mut clipped);
    out
}

// One Raven patch: nodes go straight into the output. Upstream returned a
// `Vec` per node (one allocation per primitive), and a scroll container
// strips every node of its content on each rebuild.
fn remove_composition_clips(
    nodes: Vec<RenderNode>,
    composition_clips: &[ClipShape],
    rewriter: &mut LayerRewriter,
) -> Vec<RenderNode> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        remove_composition_clip_node(node, composition_clips, rewriter, &mut out);
    }
    out
}

fn remove_composition_clip_node(
    node: RenderNode,
    composition_clips: &[ClipShape],
    rewriter: &mut LayerRewriter,
    out: &mut Vec<RenderNode>,
) {
    #[cfg(test)]
    CANONICALIZED_NODES.with(|count| count.set(count.get() + 1));
    match node {
        RenderNode::Clip { clips, children } => rebuild_composition_clip_node(
            clips,
            remove_composition_clips(children, composition_clips, rewriter),
            composition_clips,
            false,
            out,
        ),
        RenderNode::RelaxedClip { clips, children } => rebuild_composition_clip_node(
            clips,
            remove_composition_clips(children, composition_clips, rewriter),
            composition_clips,
            true,
            out,
        ),
        // ShadowPass clips are axis-specific shadow boundaries, not ordinary inherited content
        // clips. Keep them in the reusable scroll payload even when their rectangle happens to
        // equal an inherited viewport clip.
        RenderNode::ShadowPass { children } => out.push(RenderNode::ShadowPass { children }),
        RenderNode::Transform {
            transform,
            children,
        } => out.push(RenderNode::Transform {
            transform,
            children: remove_composition_clips(children, composition_clips, rewriter),
        }),
        RenderNode::Alpha { alpha, children } => out.push(RenderNode::Alpha {
            alpha,
            children: remove_composition_clips(children, composition_clips, rewriter),
        }),
        RenderNode::PaintLayer(layer) => {
            out.push(RenderNode::PaintLayer(strip_layer_clips(
                &layer,
                composition_clips,
                rewriter,
            )));
        }
        RenderNode::Primitive(_) => out.push(node),
    }
}

// One Raven patch: a scroll container's content is cached as a layer in
// canonical form: the clips it inherits (`clips`, its own included) stripped
// and its scroll offset added back, so scrolling only moves the layer.
// Upstream built the content raw and canonicalized all of it on every
// rebuild, so one changed row in a 1,400-node screen under the panel's
// scroll body copied and rewrote every visible node (about half of a
// visible one-switch patch). Canonicalizing is a per-node map, so it is
// built in place: an element whose own wrappers commute with it (clips and
// alpha only) wraps its children's canonical nodes and canonicalizes just
// what it adds, and a clean child hands back its cached canonical nodes.
// Anything else (a transform, a layer, a nearby branch) is built raw and
// canonicalized as a whole, as before.
struct ScrollCanon {
    clips: Arc<[ClipShape]>,
    dx: f32,
    dy: f32,
    rewriter: RefCell<LayerRewriter>,
}

impl ScrollCanon {
    fn take(element: &Element, clips: Arc<[ClipShape]>) -> Self {
        Self {
            clips,
            dx: element.layout.scroll_x,
            dy: element.layout.scroll_y,
            rewriter: RefCell::new(LayerRewriter::take(element)),
        }
    }

    fn store(&self, element: &Element) {
        let rewriter = std::mem::replace(
            &mut *self.rewriter.borrow_mut(),
            LayerRewriter {
                previous: Vec::new(),
                current: Vec::new(),
            },
        );
        rewriter.store(element);
    }

    /// Canonicalize raw nodes.
    fn nodes(&self, nodes: Vec<RenderNode>) -> Vec<RenderNode> {
        if nodes.is_empty() {
            return nodes;
        }
        let mut rewriter = self.rewriter.borrow_mut();
        let stripped = remove_composition_clips(nodes, &self.clips, &mut rewriter);
        offset_render_nodes(stripped, self.dx, self.dy, &mut rewriter)
    }

    /// `wrap_with_clips` over canonical nodes, canonical: the clips stripped
    /// and offset as `nodes` would do to the clip nodes the wrap makes.
    fn wrap_with_clips(&self, nodes: Vec<RenderNode>, clips: &[ClipShape]) -> Vec<RenderNode> {
        let kept: Vec<ClipShape> = clips
            .iter()
            .filter(|clip| !self.clips.contains(clip))
            .map(|clip| clip.offset(-self.dx, -self.dy))
            .collect();
        // With no clip left every group the wrap makes is spliced back in
        // order; otherwise each node keeps its escape grouping (a spliced
        // node never escapes a remaining clip it inherited).
        wrap_with_clips(nodes, &kept)
    }
}

// Test reference: build scroll content raw and canonicalize it at the
// container, as upstream does.
#[cfg(test)]
thread_local! {
    static CANONICALIZE_AFTER_BUILD: Cell<bool> = const { Cell::new(false) };
    static CANONICALIZED_NODES: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn canonicalize_scroll_content_after_build() -> bool {
    CANONICALIZE_AFTER_BUILD.with(Cell::get)
}

#[cfg(not(test))]
fn canonicalize_scroll_content_after_build() -> bool {
    false
}

#[cfg(test)]
pub(crate) fn set_canonicalize_scroll_content_after_build(enabled: bool) {
    CANONICALIZE_AFTER_BUILD.with(|flag| flag.set(enabled));
}

/// Render nodes a clip strip has visited since the last call.
#[cfg(test)]
pub(crate) fn take_canonicalized_nodes() -> usize {
    CANONICALIZED_NODES.with(|count| count.replace(0))
}

// One Raven patch: a scroll container's content is canonicalized on every
// rebuild (inherited clips stripped, scroll offset applied), and that copies
// and rebuilds every paint layer inside it, nested scroll layers included.
// So a one-text change in a screen under the panel's scroll body rebuilt the
// clean list's layer each time: 20% of the tree actor's patch time on a
// 1,400-node screen. A rewrite depends only on the input layer and the
// operation, and a clean subtree hands back the same layer (same `Arc`
// content) every frame, so the container keeps its last rewrites and
// reuses those that come round again.
struct LayerRewriter {
    previous: Vec<LayerRewrite>,
    current: Vec<LayerRewrite>,
}

impl LayerRewriter {
    fn take(element: &Element) -> Self {
        Self {
            previous: std::mem::take(&mut *element.refresh.layer_rewrites.borrow_mut()),
            current: Vec::new(),
        }
    }

    fn store(self, element: &Element) {
        *element.refresh.layer_rewrites.borrow_mut() = self.current;
    }

    fn rewrite(
        &mut self,
        layer: &RenderPaintLayer,
        op: LayerRewriteOp,
        compute: impl FnOnce(&mut Self) -> RenderPaintLayer,
    ) -> RenderPaintLayer {
        let matches = |entry: &LayerRewrite| entry.op == op && same_layer_input(&entry.input, layer);
        if let Some(entry) = self.current.iter().find(|entry| matches(entry)) {
            return entry.output.clone();
        }
        if let Some(index) = self.previous.iter().position(matches) {
            let entry = self.previous.swap_remove(index);
            let output = entry.output.clone();
            self.current.push(entry);
            return output;
        }
        #[cfg(test)]
        LAYER_REWRITES_COMPUTED.with(|count| count.set(count.get() + 1));
        let output = compute(self);
        self.current.push(LayerRewrite {
            input: layer.clone(),
            op,
            output: output.clone(),
        });
        output
    }
}

fn same_layer_input(a: &RenderPaintLayer, b: &RenderPaintLayer) -> bool {
    Arc::ptr_eq(&a.content, &b.content)
        && a.id == b.id
        && a.bounds == b.bounds
        && a.placement == b.placement
        && a.policy == b.policy
        && a.content_generation == b.content_generation
        && a.metrics == b.metrics
        && match (&a.renderer_animation, &b.renderer_animation) {
            (None, None) => true,
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
}

#[cfg(test)]
thread_local! {
    static LAYER_REWRITES_COMPUTED: Cell<usize> = const { Cell::new(0) };
}

/// Paint-layer rewrites computed (not reused) since the last call.
#[cfg(test)]
pub(crate) fn take_layer_rewrites_computed() -> usize {
    LAYER_REWRITES_COMPUTED.with(|count| count.replace(0))
}

fn strip_layer_clips(
    layer: &RenderPaintLayer,
    composition_clips: &[ClipShape],
    rewriter: &mut LayerRewriter,
) -> RenderPaintLayer {
    let op = LayerRewriteOp::StripClips(composition_clips.to_vec());
    rewriter.rewrite(layer, op, |rewriter| {
        if layer.id.role == PaintLayerReason::ScrollContent
            && strip_keeps_content(&layer.content.nodes, composition_clips)
        {
            return layer_with_stripped_child_layers(layer, composition_clips, rewriter);
        }
        let children = remove_composition_clips(layer.content_nodes(), composition_clips, rewriter);
        layer.with_children(children)
    })
}

// One Raven patch: a nested scroll container's layer has had its inherited
// clips stripped already, so an outer strip (a nearby boundary over the page)
// usually finds nothing to remove in it, yet copied its whole content out and
// rebuilt it whenever the content changed: a second full pass over the list.
// When no clip node in it would change, the result is the same layer with its
// child layers stripped (their own rewrites), so build exactly that. Only for
// scroll content: it is built in composition form, which the rebuild would
// reproduce, and its metrics' visual bounds already cover its own nodes.
fn strip_keeps_content(content: &[RenderPaintLayerContentNode], clips: &[ClipShape]) -> bool {
    content.iter().all(|node| match node {
        RenderPaintLayerContentNode::Own(run) => run
            .nodes
            .iter()
            .all(|node| matches!(node, RenderNode::Primitive(_))),
        RenderPaintLayerContentNode::Child(_) | RenderPaintLayerContentNode::ShadowPass { .. } => {
            true
        }
        RenderPaintLayerContentNode::Clip {
            clips: own,
            children,
        }
        | RenderPaintLayerContentNode::RelaxedClip {
            clips: own,
            children,
        } => {
            !children.is_empty()
                && !own.iter().any(|clip| clips.contains(clip))
                && strip_keeps_content(children, clips)
        }
        RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            strip_keeps_content(children, clips)
        }
    })
}

fn layer_with_stripped_child_layers(
    layer: &RenderPaintLayer,
    clips: &[ClipShape],
    rewriter: &mut LayerRewriter,
) -> RenderPaintLayer {
    fn map(
        content: &[RenderPaintLayerContentNode],
        clips: &[ClipShape],
        rewriter: &mut LayerRewriter,
    ) -> Vec<RenderPaintLayerContentNode> {
        content
            .iter()
            .map(|node| match node {
                RenderPaintLayerContentNode::Child(child) => {
                    RenderPaintLayerContentNode::Child(strip_layer_clips(child, clips, rewriter))
                }
                // The strip leaves shadow passes and runs alone.
                RenderPaintLayerContentNode::Own(_)
                | RenderPaintLayerContentNode::ShadowPass { .. } => node.clone(),
                RenderPaintLayerContentNode::Clip {
                    clips: own,
                    children,
                } => RenderPaintLayerContentNode::Clip {
                    clips: own.clone(),
                    children: map(children, clips, rewriter),
                },
                RenderPaintLayerContentNode::RelaxedClip {
                    clips: own,
                    children,
                } => RenderPaintLayerContentNode::RelaxedClip {
                    clips: own.clone(),
                    children: map(children, clips, rewriter),
                },
                RenderPaintLayerContentNode::Transform {
                    transform,
                    children,
                } => RenderPaintLayerContentNode::Transform {
                    transform: *transform,
                    children: map(children, clips, rewriter),
                },
                RenderPaintLayerContentNode::Alpha { alpha, children } => {
                    RenderPaintLayerContentNode::Alpha {
                        alpha: *alpha,
                        children: map(children, clips, rewriter),
                    }
                }
            })
            .collect()
    }
    RenderPaintLayer {
        content: Arc::new(RenderPaintLayerContent {
            nodes: map(&layer.content.nodes, clips, rewriter),
        }),
        ..layer.clone()
    }
}

fn rebuild_composition_clip_node(
    mut clips: Vec<ClipShape>,
    children: Vec<RenderNode>,
    composition_clips: &[ClipShape],
    relaxed: bool,
    out: &mut Vec<RenderNode>,
) {
    if children.is_empty() {
        return;
    }
    clips.retain(|clip| !composition_clips.contains(clip));
    if clips.is_empty() {
        out.extend(children);
    } else if relaxed {
        out.push(RenderNode::RelaxedClip { clips, children });
    } else {
        out.push(RenderNode::Clip { clips, children });
    }
}

fn offset_render_nodes(
    nodes: Vec<RenderNode>,
    dx: f32,
    dy: f32,
    rewriter: &mut LayerRewriter,
) -> Vec<RenderNode> {
    nodes
        .into_iter()
        .map(|node| offset_render_node(node, dx, dy, rewriter))
        .collect()
}

fn offset_render_node(
    node: RenderNode,
    dx: f32,
    dy: f32,
    rewriter: &mut LayerRewriter,
) -> RenderNode {
    match node {
        RenderNode::Clip { clips, children } => RenderNode::Clip {
            clips: clips
                .into_iter()
                .map(|clip| clip.offset(-dx, -dy))
                .collect(),
            children: offset_render_nodes(children, dx, dy, rewriter),
        },
        RenderNode::RelaxedClip { clips, children } => RenderNode::RelaxedClip {
            clips: clips
                .into_iter()
                .map(|clip| clip.offset(-dx, -dy))
                .collect(),
            children: offset_render_nodes(children, dx, dy, rewriter),
        },
        RenderNode::ShadowPass { children } => RenderNode::ShadowPass {
            children: offset_render_nodes(children, dx, dy, rewriter),
        },
        RenderNode::Transform {
            transform,
            children,
        } => RenderNode::Transform {
            transform: Affine2::translation(dx, dy).then(transform),
            children,
        },
        RenderNode::Alpha { alpha, children } => RenderNode::Alpha {
            alpha,
            children: offset_render_nodes(children, dx, dy, rewriter),
        },
        RenderNode::PaintLayer(layer) => {
            let op = LayerRewriteOp::Offset {
                dx: dx.to_bits(),
                dy: dy.to_bits(),
            };
            RenderNode::PaintLayer(rewriter.rewrite(&layer, op, |rewriter| {
                offset_layer(&layer, dx, dy, rewriter)
            }))
        }
        RenderNode::Primitive(primitive) => {
            RenderNode::Primitive(offset_draw_primitive(primitive, dx, dy))
        }
    }
}

fn offset_layer(
    layer: &RenderPaintLayer,
    dx: f32,
    dy: f32,
    rewriter: &mut LayerRewriter,
) -> RenderPaintLayer {
    let bounds = Rect {
        x: layer.bounds.x + dx,
        y: layer.bounds.y + dy,
        ..layer.bounds
    };
    let children = offset_render_nodes(layer.content_nodes(), dx, dy, rewriter);
    let mut layer = layer.with_bounds_and_children(bounds, children);
    // One Raven patch: the offset prepended a translation to this
    // layer's Transform nodes; the renderer animation must match.
    if let Some(animation) = layer.renderer_animation.as_mut() {
        let animation = Arc::make_mut(animation);
        animation.offset = (animation.offset.0 + dx, animation.offset.1 + dy);
        animation.current_transform =
            Affine2::translation(dx, dy).then(animation.current_transform);
        animation.bounds_transform =
            Affine2::translation(dx, dy).then(animation.bounds_transform);
    }
    if layer.policy.allows_payload_cache() {
        layer.content_generation = paint_layer_content_generation(
            &layer.content.own_payload_render_nodes(),
            bounds,
        );
    }
    layer
}

fn offset_draw_primitive(primitive: DrawPrimitive, dx: f32, dy: f32) -> DrawPrimitive {
    match primitive {
        DrawPrimitive::Rect(x, y, w, h, color) => {
            DrawPrimitive::Rect(x + dx, y + dy, w, h, color.translated(dx, dy))
        }
        DrawPrimitive::RoundedRect(x, y, w, h, radius, color) => {
            DrawPrimitive::RoundedRect(x + dx, y + dy, w, h, radius, color.translated(dx, dy))
        }
        DrawPrimitive::Border(x, y, w, h, radius, width, color, style) => DrawPrimitive::Border(
            x + dx,
            y + dy,
            w,
            h,
            radius,
            width,
            color.translated(dx, dy),
            style,
        ),
        DrawPrimitive::BorderCorners(x, y, w, h, tl, tr, br, bl, width, color, style) => {
            DrawPrimitive::BorderCorners(
                x + dx,
                y + dy,
                w,
                h,
                tl,
                tr,
                br,
                bl,
                width,
                color.translated(dx, dy),
                style,
            )
        }
        DrawPrimitive::BorderEdges(x, y, w, h, radius, top, right, bottom, left, color, style) => {
            DrawPrimitive::BorderEdges(
                x + dx,
                y + dy,
                w,
                h,
                radius,
                top,
                right,
                bottom,
                left,
                color.translated(dx, dy),
                style,
            )
        }
        DrawPrimitive::Shadow(x, y, w, h, ox, oy, blur, size, radius, color) => {
            DrawPrimitive::Shadow(
                x + dx,
                y + dy,
                w,
                h,
                ox,
                oy,
                blur,
                size,
                radius,
                color.translated(dx, dy),
            )
        }
        DrawPrimitive::InsetShadow(x, y, w, h, ox, oy, blur, size, radius, color) => {
            DrawPrimitive::InsetShadow(
                x + dx,
                y + dy,
                w,
                h,
                ox,
                oy,
                blur,
                size,
                radius,
                color.translated(dx, dy),
            )
        }
        DrawPrimitive::TextWithFont(x, y, text, size, fill, family, weight, italic) => {
            DrawPrimitive::TextWithFont(
                x + dx,
                y + dy,
                text,
                size,
                fill.translated(dx, dy),
                family,
                weight,
                italic,
            )
        }

        DrawPrimitive::Image(x, y, w, h, id, fit, tint) => DrawPrimitive::Image(
            x + dx,
            y + dy,
            w,
            h,
            id,
            fit,
            tint.map(|c| c.translated(dx, dy)),
        ),
        DrawPrimitive::Video(x, y, w, h, target, fit) => {
            DrawPrimitive::Video(x + dx, y + dy, w, h, target, fit)
        }
        DrawPrimitive::ImageLoading(x, y, w, h) => {
            DrawPrimitive::ImageLoading(x + dx, y + dy, w, h)
        }
        DrawPrimitive::ImageFailed(x, y, w, h) => DrawPrimitive::ImageFailed(x + dx, y + dy, w, h),
    }
}

fn is_declared_scroll_container(element: &Element) -> bool {
    element.layout.scroll_x != 0.0
        || element.layout.scroll_y != 0.0
        || element.layout.scroll_x_max > 0.0
        || element.layout.scroll_y_max > 0.0
        || effective_scrollbar_x(&element.layout.effective)
        || effective_scrollbar_y(&element.layout.effective)
}

fn element_has_declared_animation(element: &Element) -> bool {
    let specs = [
        element.spec.declared.animate.as_ref(),
        element.spec.declared.animate_enter.as_ref(),
        element.spec.declared.animate_exit.as_ref(),
        element.lifecycle.ghost_exit_animation.as_ref(),
    ];
    let mut declared = false;
    specs.into_iter().flatten().all(|spec| {
        declared = true;
        animation_spec_is_compositor_only(spec)
    }) && declared
}

fn should_cull_render_subtree(
    tree: &ElementTree,
    ix: NodeIx,
    attrs: &Attrs,
    render_frame: Frame,
    transform: Affine2,
    scene_ctx: &SceneContext,
) -> bool {
    let culled =
        should_skip_resolved_viewport_subtree(tree, ix, attrs, render_frame, transform, scene_ctx);
    if culled {
        record_render_traversal_culled_subtree();
    }
    culled
}

fn should_skip_render_child_subtree(
    tree: &ElementTree,
    child_ix: NodeIx,
    scene_ctx: &SceneContext,
) -> bool {
    let culled = should_skip_render_viewport_subtree(tree, child_ix, scene_ctx);
    if culled {
        record_render_traversal_culled_subtree();
    }
    culled
}

fn build_host_content_subtree<H: HostRegistryTraversalSink>(
    tree: &ElementTree,
    input: HostContentBuild<'_>,
    outputs: &mut RenderOutputs<'_>,
    traversal: RenderTraversal<'_>,
    mut registry: H,
) -> RenderSubtree {
    let HostContentBuild {
        element,
        element_ix,
        render_frame,
        element_context,
        scene_state,
    } = input;

    let attrs = &element.layout.effective;
    let current_host_clip = HostClipDescriptor {
        clip: scene_state
            .as_ref()
            .map(|state| state.host_clip)
            .unwrap_or_else(|| host_clip_shape(render_frame, attrs)),
        scroll_x: effective_scrollbar_x(attrs),
        scroll_y: effective_scrollbar_y(attrs),
    };
    let current_self_shape = geometry_self_shape(render_frame, attrs);
    let current_self_clip = ClipShape {
        rect: current_self_shape.rect,
        radii: current_self_shape.radii,
    };
    let clip_nearby = attrs.clip_nearby.unwrap_or(false);
    let child_render_ctx =
        traversal
            .render_ctx
            .with_host_clip(current_host_clip, current_self_clip, clip_nearby);
    // One Raven patch: a scroll container clips the nearby content of its
    // children (as `clip_nearby` does), since that content scrolls with them;
    // its own nearby (`child_render_ctx`) still overflows it. Matches
    // `ResolvedNodeState::clip_content_nearby`, which clips input and culling.
    let scrolls = current_host_clip.scroll_x || current_host_clip.scroll_y;
    let content_render_ctx_owned = (scrolls && !clip_nearby).then(|| {
        traversal
            .render_ctx
            .with_host_clip(current_host_clip, current_self_clip, true)
    });
    let content_render_ctx = content_render_ctx_owned
        .as_ref()
        .unwrap_or(&child_render_ctx);

    // One Raven patch: in canonical scroll content (see `ScrollCanon`), child
    // elements build canonical; every other piece of local content is built
    // raw and canonicalized here. Escapes stay raw.
    let canon = traversal.scroll_canon.clone();
    let canonical = |nodes: Vec<RenderNode>| match &canon {
        Some(canon) => canon.nodes(nodes),
        None => nodes,
    };
    let mut subtree = RenderSubtree::default();
    let slider_slots = (element.spec.kind == ElementKind::Slider)
        .then(|| tree.child_ixs(element_ix))
        .filter(|slots| slots.len() == 3);
    let mut slider_value_subtree = RenderSubtree::default();
    let mut slider_value_emitted = false;

    if element.spec.kind == ElementKind::Paragraph {
        for mount in tree.local_nearby_mounts_ix(element_ix) {
            let branch_registry = registry.local_nearby_traversal(scene_state.as_ref());
            let mut branch_subtree = build_nearby_mount_subtree(
                tree,
                mount.ix,
                mount.slot,
                element_context,
                &mut outputs.reborrow(),
                traversal.for_branch(&child_render_ctx).raw(),
                scene_state.clone(),
                branch_registry,
            );
            branch_subtree.local = canonical(std::mem::take(&mut branch_subtree.local));
            subtree.extend_local(branch_subtree);
        }
    } else {
        let child_scene_ctx = children_scene_context(scene_state.as_ref());
        element.for_each_retained_local_branch(tree, |branch| match branch {
            RetainedLocalBranchRef::Nearby(mount) => {
                let branch_registry = registry.local_nearby_traversal(scene_state.as_ref());
                let mut branch_subtree = build_nearby_mount_subtree(
                    tree,
                    mount.ix,
                    mount.slot,
                    element_context,
                    &mut outputs.reborrow(),
                    traversal.for_branch(&child_render_ctx).raw(),
                    scene_state.clone(),
                    branch_registry,
                );
                branch_subtree.local = canonical(std::mem::take(&mut branch_subtree.local));
                subtree.extend_local(branch_subtree);
            }
            RetainedLocalBranchRef::Child(child) => {
                let slider_value_slot = slider_slots
                    .as_ref()
                    .is_some_and(|slots| child.ix == slots[1] || child.ix == slots[2]);
                let slider_thumb_slot = slider_slots
                    .as_ref()
                    .is_some_and(|slots| child.ix == slots[2]);
                let child_registry_skipped =
                    registry.should_skip_child(tree, child.ix, &child_scene_ctx);
                if !traversal.disable_viewport_culling
                    && !slider_value_slot
                    && should_skip_render_child_subtree(tree, child.ix, &child_scene_ctx)
                {
                    if !child_registry_skipped {
                        let child_registry_ctx = registry.child_context(child_scene_ctx.clone());
                        registry.collect_child_subtree(tree, child.ix, child_registry_ctx);
                    }
                    return;
                }
                let child_registry_ctx = (!child_registry_skipped)
                    .then(|| registry.child_context(child_scene_ctx.clone()))
                    .flatten();
                let branch_registry =
                    registry.child_traversal(child_registry_skipped, child_registry_ctx);
                let child_traversal =
                    traversal.for_child(child_scene_ctx.clone(), content_render_ctx);
                let child_traversal = if slider_value_slot {
                    child_traversal.within_cacheable_semantic_layer().raw()
                } else {
                    child_traversal
                };
                let branch_subtree = build_element_subtree(
                    tree,
                    child.ix,
                    element_context,
                    &mut outputs.reborrow(),
                    child_traversal,
                    branch_registry,
                );
                if slider_value_slot {
                    slider_value_subtree.extend_local(branch_subtree);
                    if slider_thumb_slot {
                        subtree.merge_outputs(&slider_value_subtree);
                        subtree.local.extend(canonical(wrap_with_paint_layer(
                            std::mem::take(&mut slider_value_subtree.local),
                            element.id.to_wire_u64(),
                            PaintLayerPlacement::Fixed,
                            PaintLayerPolicy::Cacheable,
                            PaintLayerReason::SliderValue,
                            render_frame,
                            None,
                        )));
                        subtree.escapes.append(&mut slider_value_subtree.escapes);
                        slider_value_emitted = true;
                    }
                } else {
                    subtree.extend_local(branch_subtree);
                }
            }
        });
    }

    if slider_slots.is_some() && !slider_value_emitted {
        subtree.merge_outputs(&slider_value_subtree);
        subtree.local.extend(canonical(wrap_with_paint_layer(
            std::mem::take(&mut slider_value_subtree.local),
            element.id.to_wire_u64(),
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::Cacheable,
            PaintLayerReason::SliderValue,
            render_frame,
            None,
        )));
        subtree.escapes.append(&mut slider_value_subtree.escapes);
    }

    let mut own_text_input_focused = false;
    let mut own_text_input_cursor_area = None;
    let own_content_nodes = build_own_content_nodes(
        element,
        render_frame,
        attrs,
        element_context,
        &mut own_text_input_focused,
        &mut own_text_input_cursor_area,
    );
    if own_text_input_focused {
        *outputs.text_input_focused = true;
    }
    if outputs.text_input_cursor_area.is_none() {
        *outputs.text_input_cursor_area = own_text_input_cursor_area;
    }
    subtree.text_input_focused |= own_text_input_focused;
    if subtree.text_input_cursor_area.is_none() {
        subtree.text_input_cursor_area = own_text_input_cursor_area;
    }

    subtree.local.extend(canonical(wrap_own_content_nodes(
        own_content_nodes,
        attrs,
        element.spec.kind,
        current_host_clip.clip,
    )));

    if element.spec.kind == ElementKind::Paragraph {
        let mut paragraph_subtree = build_paragraph_subtree(
            tree,
            element,
            element_context,
            &mut outputs.reborrow(),
            traversal.for_branch(content_render_ctx).raw(),
            scene_state.clone(),
            current_host_clip.clip,
            &mut registry,
        );
        paragraph_subtree.local = canonical(std::mem::take(&mut paragraph_subtree.local));
        subtree.extend_local(paragraph_subtree);
    }

    for mount in tree.escape_nearby_mounts_ix(element_ix) {
        registry.defer_escape_nearby_subtree(tree, mount.ix, mount.slot, scene_state.as_ref());
        let escape_render_ctx = child_render_ctx.without_host_clips();
        subtree.extend_escape(build_nearby_mount_subtree(
            tree,
            mount.ix,
            mount.slot,
            element_context,
            &mut outputs.reborrow(),
            traversal.for_branch(&escape_render_ctx).raw(),
            scene_state.clone(),
            ReuseCleanRegistryTraversal,
        ));
    }

    subtree
}

#[allow(clippy::too_many_arguments)]
fn build_nearby_mount_subtree<R: RegistryTraversalSink>(
    tree: &ElementTree,
    nearby_ix: NodeIx,
    slot: NearbySlot,
    element_context: &FontContext,
    outputs: &mut RenderOutputs<'_>,
    traversal: RenderTraversal<'_>,
    scene_state: Option<ResolvedNodeState>,
    mut registry: R,
) -> RenderSubtree {
    let nearby_scene_ctx = nearby_scene_context(scene_state.as_ref(), slot);

    let cache_key = tree.get_ix(nearby_ix).and_then(|element| {
        nearby_render_fragment_cache_key(
            tree,
            nearby_ix,
            element,
            &nearby_scene_ctx,
            traversal.render_ctx,
        )
    });
    if let Some(cache_key) = cache_key
        && let Some(cached) = tree
            .get_ix(nearby_ix)
            .filter(|element| {
                !(element.refresh.render_dirty || element.refresh.render_descendant_dirty)
            })
            .and_then(|element| {
                element
                    .refresh
                    .render_fragment_cache
                    .borrow()
                    .as_ref()
                    .filter(|cache| cache.key == cache_key)
                    .cloned()
            })
    {
        registry.collect_registry_for_render_skipped_subtree(tree, nearby_ix);
        let subtree = RenderSubtree::from_fragment_cache(&cached);
        apply_subtree_outputs(outputs, &subtree);
        return subtree;
    }

    let subtree = build_element_subtree(
        tree,
        nearby_ix,
        element_context,
        &mut outputs.reborrow(),
        traversal
            .for_scene_context(nearby_scene_ctx.clone())
            .within_cacheable_semantic_layer(),
        registry,
    );
    let subtree = wrap_nearby_subtree_with_nearby_layer_boundary(
        tree.get_ix(nearby_ix),
        &nearby_scene_ctx,
        traversal.render_ctx,
        subtree,
    );
    if let Some(cache_key) = cache_key
        && render_subtree_has_cacheable_fragment(&subtree)
        && let Some(element) = tree.get_ix(nearby_ix)
    {
        element
            .refresh
            .render_fragment_cache
            .borrow_mut()
            .replace(subtree.to_fragment_cache(cache_key));
    }
    subtree
}

fn render_subtree_has_cacheable_fragment(subtree: &RenderSubtree) -> bool {
    !subtree.local.is_empty()
        || !subtree.escapes.is_empty()
        || subtree.text_input_focused
        || subtree.text_input_cursor_area.is_some()
}

fn nearby_render_fragment_cache_key(
    tree: &ElementTree,
    ix: NodeIx,
    element: &Element,
    scene_ctx: &SceneContext,
    render_ctx: &RenderBuildContext,
) -> Option<RenderFragmentCacheKey> {
    let frame = element.layout.frame?;
    let raw_render_frame = element.layout.render_frame.unwrap_or(frame);
    let render_frame = Frame {
        x: raw_render_frame.x - scene_ctx.scroll_dx,
        y: raw_render_frame.y - scene_ctx.scroll_dy,
        ..raw_render_frame
    };

    Some(RenderFragmentCacheKey {
        kind: RenderFragmentCacheKind::Nearby,
        paint_generation: element.refresh.paint_generation,
        topology: tree.topology_dependency_key_ix(ix),
        bounds: Rect::from_frame(render_frame),
        context: nearby_render_fragment_context_key(scene_ctx, render_ctx),
    })
}

fn nearby_render_fragment_context_key(
    scene_ctx: &SceneContext,
    render_ctx: &RenderBuildContext,
) -> u64 {
    let mut hasher = FastHasher::new();
    hash_f32_bits(&mut hasher, scene_ctx.scroll_dx);
    hash_f32_bits(&mut hasher, scene_ctx.scroll_dy);
    hasher.write_u8(scene_ctx.front_nearby_subtree as u8);
    hasher.write_u8(scene_ctx.front_nearby_root as u8);
    scene_ctx
        .visible_clip
        .iter()
        .for_each(|clip| hash_clip_shape(&mut hasher, clip));
    scene_ctx
        .nearby_visible_clip
        .iter()
        .for_each(|clip| hash_clip_shape(&mut hasher, clip));
    render_ctx
        .full_clip_shapes()
        .iter()
        .for_each(|clip| hash_clip_shape(&mut hasher, clip));
    hasher.finish()
}

fn hash_clip_shape(hasher: &mut FastHasher, clip: &ClipShape) {
    hash_rect_bits(hasher, clip.rect);
    match clip.radii {
        Some(radii) => {
            hasher.write_u8(1);
            hash_f32_bits(hasher, radii.tl);
            hash_f32_bits(hasher, radii.tr);
            hash_f32_bits(hasher, radii.br);
            hash_f32_bits(hasher, radii.bl);
        }
        None => hasher.write_u8(0),
    }
}

fn hash_rect_bits(hasher: &mut FastHasher, rect: Rect) {
    hash_f32_bits(hasher, rect.x);
    hash_f32_bits(hasher, rect.y);
    hash_f32_bits(hasher, rect.width);
    hash_f32_bits(hasher, rect.height);
}

fn hash_f32_bits(hasher: &mut FastHasher, value: f32) {
    hasher.write_u32(if value == 0.0 { 0.0f32 } else { value }.to_bits());
}

#[allow(clippy::too_many_arguments)]
fn build_paragraph_subtree<H: HostRegistryTraversalSink>(
    tree: &ElementTree,
    element: &Element,
    element_context: &FontContext,
    outputs: &mut RenderOutputs<'_>,
    traversal: RenderTraversal<'_>,
    scene_state: Option<ResolvedNodeState>,
    current_host_clip: ClipShape,
    registry: &mut H,
) -> RenderSubtree {
    let child_scene_ctx = children_scene_context(scene_state.as_ref());
    let fragment_offset = scene_state
        .as_ref()
        .map(|state| {
            (
                state.adjusted_frame.x - state.frame.x,
                state.adjusted_frame.y - state.frame.y,
            )
        })
        .unwrap_or_default();

    let mut subtree = RenderSubtree::default();
    element.for_each_retained_child(tree, |child| match child.mode {
        RetainedChildMode::Scope => {
            let child_registry_skipped =
                registry.should_skip_child(tree, child.ix, &child_scene_ctx);
            if !traversal.disable_viewport_culling
                && should_skip_render_child_subtree(tree, child.ix, &child_scene_ctx)
            {
                if !child_registry_skipped {
                    let child_registry_ctx = registry.child_context(child_scene_ctx.clone());
                    registry.collect_child_subtree(tree, child.ix, child_registry_ctx);
                }
                return;
            }
            let child_registry_ctx = (!child_registry_skipped)
                .then(|| registry.child_context(child_scene_ctx.clone()))
                .flatten();
            let branch_registry =
                registry.child_traversal(child_registry_skipped, child_registry_ctx);
            let child_subtree = build_element_subtree(
                tree,
                child.ix,
                element_context,
                &mut outputs.reborrow(),
                traversal.for_scene_context(child_scene_ctx.clone()),
                branch_registry,
            );
            subtree.extend_local(child_subtree);
        }
        RetainedChildMode::InlineEventOnly => {
            let child_registry_skipped =
                registry.should_skip_child(tree, child.ix, &child_scene_ctx);
            if !child_registry_skipped {
                let child_registry_ctx = registry.child_context(child_scene_ctx.clone());
                registry.collect_child_subtree(tree, child.ix, child_registry_ctx);
            }
        }
    });

    if let Some(fragments) = &element.layout.paragraph_fragments {
        let Some(frame) = scene_state
            .as_ref()
            .map(|s| s.adjusted_frame)
            .or(element.layout.frame)
        else {
            return subtree;
        };
        let text_bounds = box_model::content_rect(frame, &element.layout.effective);
        let boxes = &element.layout.paragraph_boxes;
        let tail = boxes.last().map_or(0, |b| b.text_range.end);
        let mut cursor = 0;
        let groups = boxes
            .iter()
            .flat_map(|b| {
                let before = cursor..b.text_range.start;
                cursor = b.text_range.end;
                [(before, None), (b.text_range.clone(), Some(b))]
            })
            .chain(std::iter::once((tail..fragments.len(), None)));
        subtree.local.extend(groups.flat_map(|(range, inline)| {
            let text = paragraph_text_nodes(&fragments[range], text_bounds, fragment_offset);
            let Some((inline, owner)) =
                inline.and_then(|b| tree.get(&b.owner).map(|owner| (b, owner)))
            else {
                return wrap_with_host_clip(text, current_host_clip);
            };
            let attrs = &owner.layout.effective;
            let frame = Frame {
                x: inline.frame.x + fragment_offset.0,
                y: inline.frame.y + fragment_offset.1,
                ..inline.frame
            };
            let outer = wrap_outer_shadow_nodes(
                collect_box_shadow_nodes(frame, attrs, attrs.border_radius.as_ref(), false),
                crate::tree::transform::Affine2::identity(),
                traversal.render_ctx,
            );
            // No implicit background for plain or shadow-only wrappers. Resolve
            // only an explicitly supplied color (including active styles/animation).
            // Inline image backgrounds are not part of the text-flow contract.
            let background = match attrs.background {
                Some(super::attrs::Background::Color(_)) => build_background_nodes(frame, attrs),
                _ => Vec::new(),
            };
            let normal = background
                .into_iter()
                .chain(collect_box_shadow_nodes(
                    frame,
                    attrs,
                    attrs.border_radius.as_ref(),
                    true,
                ))
                .chain(text)
                .chain(collect_border_nodes(frame, attrs))
                .collect();
            wrap_with_alpha(
                outer
                    .into_iter()
                    .chain(wrap_with_host_clip(normal, current_host_clip))
                    .collect(),
                attrs.alpha.unwrap_or(1.0) as f32,
            )
        }));
    }

    subtree
}

fn paragraph_text_nodes(
    fragments: &[super::attrs::TextFragment],
    bounds: (f32, f32, f32, f32),
    fragment_offset: (f32, f32),
) -> Vec<RenderNode> {
    let (x, y, width, height) = bounds;
    fragments
        .iter()
        .filter(|frag| !frag.text.is_empty())
        .flat_map(|frag| {
            let color = frag.color.with_bounds(crate::tree::geometry::Rect {
                x,
                y,
                width,
                height,
            });
            let x = frag.x + fragment_offset.0;
            let baseline_y = frag.y + fragment_offset.1 + frag.ascent;
            let text = RenderNode::Primitive(DrawPrimitive::TextWithFont(
                x,
                baseline_y,
                frag.text.clone(),
                frag.font_size,
                color.clone(),
                frag.family.clone(),
                frag.weight,
                frag.italic,
            ));

            let decorations = if frag.underline || frag.strike {
                let font =
                    make_font_with_style(&frag.family, frag.weight, frag.italic, frag.font_size);
                let word_width =
                    measure_text_visual_metrics_with_font(&font, &frag.text).visual_width;
                text_decoration_items(TextDecorationSpec {
                    x,
                    baseline_y,
                    width: word_width,
                    font_size: frag.font_size,
                    color: &color,
                    underline: frag.underline,
                    strike: frag.strike,
                })
            } else {
                Vec::new()
            };
            std::iter::once(text).chain(decorations)
        })
        .collect()
}

fn build_own_content_nodes(
    element: &Element,
    frame: Frame,
    attrs: &Attrs,
    inherited: &FontContext,
    text_input_focused: &mut bool,
    text_input_cursor_area: &mut Option<(f32, f32, f32, f32)>,
) -> Vec<RenderNode> {
    let mut nodes = Vec::new();

    match element.spec.kind {
        ElementKind::Text => nodes.extend(render_text_items(frame, attrs, inherited)),
        ElementKind::TextInput => {
            if element.runtime.text_input_focused {
                *text_input_focused = true;
            }

            if text_input_cursor_area.is_none() {
                *text_input_cursor_area =
                    render_text_input_items(&mut nodes, frame, attrs, &element.runtime, inherited);
            } else {
                let _ =
                    render_text_input_items(&mut nodes, frame, attrs, &element.runtime, inherited);
            }
        }
        ElementKind::Multiline => {
            if element.runtime.text_input_focused {
                *text_input_focused = true;
            }

            if text_input_cursor_area.is_none() {
                *text_input_cursor_area = render_multiline_text_input_items(
                    &mut nodes,
                    frame,
                    attrs,
                    &element.runtime,
                    inherited,
                );
            } else {
                let _ = render_multiline_text_input_items(
                    &mut nodes,
                    frame,
                    attrs,
                    &element.runtime,
                    inherited,
                );
            }
        }
        ElementKind::Image => nodes.extend(render_image_nodes(frame, attrs)),
        ElementKind::Video => nodes.extend(render_video_nodes(frame, attrs)),
        _ => {}
    }

    nodes
}

fn wrap_with_clips(nodes: Vec<RenderNode>, clips: &[ClipShape]) -> Vec<RenderNode> {
    if nodes.is_empty() {
        return nodes;
    }

    if clips.is_empty() {
        return nodes;
    }

    wrap_with_clip_kind(nodes, clips, false)
}

fn wrap_with_relaxed_clips(nodes: Vec<RenderNode>, clips: &[ClipShape]) -> Vec<RenderNode> {
    if nodes.is_empty() {
        return nodes;
    }

    if clips.is_empty() {
        return nodes;
    }

    wrap_with_clip_kind(nodes, clips, true)
}

fn wrap_with_clip_kind(
    nodes: Vec<RenderNode>,
    clips: &[ClipShape],
    relaxed: bool,
) -> Vec<RenderNode> {
    if !nodes.iter().any(render_node_has_shadow_escape) {
        return vec![clip_node(clips.to_vec(), relaxed, nodes)];
    }

    let mut out = Vec::new();
    let mut clipped = Vec::new();

    for node in nodes {
        if render_node_has_shadow_escape(&node) {
            for (escapes, part) in split_shadow_escape(node) {
                if escapes {
                    push_clipped_group(&mut out, clips, relaxed, &mut clipped);
                    out.push(part);
                } else {
                    clipped.push(part);
                }
            }
        } else {
            clipped.push(node);
        }
    }

    push_clipped_group(&mut out, clips, relaxed, &mut clipped);
    out
}

// One Raven patch: a shadow escapes its ancestors' clips, but the node holding
// it escaped whole, so a moved element (`move_x`, `rotate`, a dragged
// carousel strip) with a shadowed descendant drew all its content past its
// host: the host clips its children everywhere else, and the same element
// unmoved was clipped. A transform or clip node is now split, in paint order,
// into its parts that hold a shadow and those that don't (each re-wrapped in
// the same transform or clip, which paints the same), so only the shadows
// escape. An alpha or layer node can't be split without changing how it
// composites, so it still escapes whole. Upstream bug.
fn split_shadow_escape(node: RenderNode) -> Vec<(bool, RenderNode)> {
    fn regroup(
        children: Vec<RenderNode>,
        wrap: impl Fn(Vec<RenderNode>) -> RenderNode,
    ) -> Vec<(bool, RenderNode)> {
        let mut parts: Vec<(bool, Vec<RenderNode>)> = Vec::new();
        for (escapes, part) in children.into_iter().flat_map(split_shadow_escape) {
            match parts.last_mut() {
                Some((last, group)) if *last == escapes => group.push(part),
                _ => parts.push((escapes, vec![part])),
            }
        }
        parts
            .into_iter()
            .map(|(escapes, group)| (escapes, wrap(group)))
            .collect()
    }

    if !render_node_has_shadow_escape(&node) {
        return vec![(false, node)];
    }
    match node {
        RenderNode::Transform {
            transform,
            children,
        } => regroup(children, |children| RenderNode::Transform {
            transform,
            children,
        }),
        RenderNode::Clip { clips, children } => regroup(children, |children| RenderNode::Clip {
            clips: clips.clone(),
            children,
        }),
        RenderNode::RelaxedClip { clips, children } => {
            regroup(children, |children| RenderNode::RelaxedClip {
                clips: clips.clone(),
                children,
            })
        }
        node => vec![(true, node)],
    }
}

fn render_node_has_shadow_escape(node: &RenderNode) -> bool {
    match node {
        RenderNode::ShadowPass { .. } => true,
        RenderNode::PaintLayer(layer) => layer_content_contains_shadow_pass(&layer.content.nodes),
        RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => {
            children.iter().any(render_node_contains_shadow_pass)
        }
        RenderNode::Primitive(_) => false,
    }
}

fn render_node_contains_shadow_pass(node: &RenderNode) -> bool {
    match node {
        RenderNode::ShadowPass { .. } => true,
        RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => {
            children.iter().any(render_node_contains_shadow_pass)
        }
        RenderNode::PaintLayer(layer) => layer_content_contains_shadow_pass(&layer.content.nodes),
        RenderNode::Primitive(_) => false,
    }
}

// One Raven patch: search a paint layer's content in place. Upstream called
// `content_nodes()`, which copies the layer's whole content into new render
// nodes, and every clip wrap on the path to a change asks this of everything
// below it, so a one-text patch above a scroll list copied the list's layers
// once per ancestor (29% of the tree actor's time on the hub's Stress screen).
fn layer_content_contains_shadow_pass(content: &[RenderPaintLayerContentNode]) -> bool {
    content.iter().any(|node| match node {
        RenderPaintLayerContentNode::Own(run) => {
            run.nodes.iter().any(render_node_contains_shadow_pass)
        }
        RenderPaintLayerContentNode::Child(layer) => {
            layer_content_contains_shadow_pass(&layer.content.nodes)
        }
        RenderPaintLayerContentNode::ShadowPass { .. } => true,
        RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            layer_content_contains_shadow_pass(children)
        }
    })
}

fn push_clipped_group(
    out: &mut Vec<RenderNode>,
    clips: &[ClipShape],
    relaxed: bool,
    clipped: &mut Vec<RenderNode>,
) {
    if clipped.is_empty() {
        return;
    }

    out.push(clip_node(clips.to_vec(), relaxed, std::mem::take(clipped)));
}

fn clip_node(clips: Vec<ClipShape>, relaxed: bool, children: Vec<RenderNode>) -> RenderNode {
    if relaxed {
        RenderNode::RelaxedClip { clips, children }
    } else {
        RenderNode::Clip { clips, children }
    }
}

fn wrap_with_shadow_pass(nodes: Vec<RenderNode>) -> Vec<RenderNode> {
    if nodes.is_empty() {
        return nodes;
    }

    vec![RenderNode::ShadowPass { children: nodes }]
}

fn wrap_outer_shadow_nodes(
    nodes: Vec<RenderNode>,
    transform: crate::tree::transform::Affine2,
    render_ctx: &RenderBuildContext,
) -> Vec<RenderNode> {
    wrap_with_shadow_pass(wrap_with_clips(
        wrap_with_transform(nodes, transform),
        &render_ctx.shadow_clip_shapes(),
    ))
}

fn wrap_with_host_clip(nodes: Vec<RenderNode>, host_clip: ClipShape) -> Vec<RenderNode> {
    wrap_with_clips(nodes, &[host_clip])
}

fn wrap_nearby_subtree_with_nearby_layer_boundary(
    element: Option<&Element>,
    scene_ctx: &SceneContext,
    render_ctx: &RenderBuildContext,
    mut subtree: RenderSubtree,
) -> RenderSubtree {
    let Some(element) = element else {
        return subtree;
    };
    let Some(frame) = element.layout.frame else {
        return subtree;
    };
    let raw_render_frame = element.layout.render_frame.unwrap_or(frame);
    let render_frame = Frame {
        x: raw_render_frame.x - scene_ctx.scroll_dx,
        y: raw_render_frame.y - scene_ctx.scroll_dy,
        ..raw_render_frame
    };
    let mut rewriter = LayerRewriter::take(element);
    let local = remove_composition_clips(
        std::mem::take(&mut subtree.local),
        render_ctx.full_clip_shapes(),
        &mut rewriter,
    );
    rewriter.store(element);
    // One Raven patch: a nearby moved only by a translation (a dragged sheet's
    // move_y) is cached without it and placed by a Transform outside the
    // layer, as scroll content is. With the translation inside, the payload
    // key changed every frame and the overlay was re-rasterized each time.
    // The outside offset is snapped to whole pixels, as a browser compositor
    // places layers, so text keeps one subpixel phase and one payload.
    let (local, translation) = match common_pure_translation(&local) {
        Some((dx, dy)) => (
            strip_translation(local, dx, dy),
            Affine2::translation(dx.round(), dy.round()),
        ),
        None => (local, Affine2::identity()),
    };
    subtree.local = wrap_with_clips(
        wrap_with_transform(
            wrap_with_paint_layer(
                local,
                element.id.to_wire_u64(),
                PaintLayerPlacement::Fixed,
                PaintLayerPolicy::Cacheable,
                PaintLayerReason::Nearby,
                render_frame,
                None,
            ),
            translation,
        ),
        render_ctx.full_clip_shapes(),
    );
    subtree
}

// One Raven patch: the translation every top-level branch reaches first, if
// they all reach the same pure-translation Transform through clips, alphas
// and shadow passes only.
fn common_pure_translation(nodes: &[RenderNode]) -> Option<(f32, f32)> {
    fn visit(nodes: &[RenderNode], found: &mut Option<(f32, f32)>) -> bool {
        !nodes.is_empty()
            && nodes.iter().all(|node| match node {
                RenderNode::Transform { transform, .. } => {
                    let t = *transform;
                    let pure = t.xx == 1.0
                        && t.yx == 0.0
                        && t.xy == 0.0
                        && t.yy == 1.0
                        && t.tx.is_finite()
                        && t.ty.is_finite();
                    pure && *found.get_or_insert((t.tx, t.ty)) == (t.tx, t.ty)
                }
                RenderNode::Clip { children, .. }
                | RenderNode::RelaxedClip { children, .. }
                | RenderNode::ShadowPass { children }
                | RenderNode::Alpha { children, .. } => visit(children, found),
                RenderNode::PaintLayer(_) | RenderNode::Primitive(_) => false,
            })
    }
    let mut found = None;
    visit(nodes, &mut found).then_some(found).flatten()
}

// One Raven patch: remove the translation `common_pure_translation` found;
// clips above it move into the untranslated space.
fn strip_translation(nodes: Vec<RenderNode>, dx: f32, dy: f32) -> Vec<RenderNode> {
    nodes
        .into_iter()
        .flat_map(|node| match node {
            RenderNode::Transform { children, .. } => children,
            RenderNode::Clip { clips, children } => vec![RenderNode::Clip {
                clips: clips.into_iter().map(|clip| clip.offset(dx, dy)).collect(),
                children: strip_translation(children, dx, dy),
            }],
            RenderNode::RelaxedClip { clips, children } => vec![RenderNode::RelaxedClip {
                clips: clips.into_iter().map(|clip| clip.offset(dx, dy)).collect(),
                children: strip_translation(children, dx, dy),
            }],
            RenderNode::ShadowPass { children } => vec![RenderNode::ShadowPass {
                children: strip_translation(children, dx, dy),
            }],
            RenderNode::Alpha { alpha, children } => vec![RenderNode::Alpha {
                alpha,
                children: strip_translation(children, dx, dy),
            }],
            node => vec![node],
        })
        .collect()
}

fn wrap_with_root_paint_layer(nodes: Vec<RenderNode>, root: Option<&Element>) -> Vec<RenderNode> {
    let Some(root) = root else {
        return nodes;
    };
    let Some(frame) = root.layout.frame else {
        return nodes;
    };

    wrap_with_paint_layer(
        nodes,
        root.id.to_wire_u64(),
        PaintLayerPlacement::Fixed,
        PaintLayerPolicy::DirectOnly,
        PaintLayerReason::Root,
        frame,
        Some(0),
    )
}

fn wrap_with_compositor_animation_layer(
    nodes: Vec<RenderNode>,
    element: &Element,
    render_frame: Frame,
) -> Vec<RenderNode> {
    let nominal_bounds = Rect::from_frame(render_frame);
    let visual_bounds = paint_layer_own_content_visual_bounds(&nodes);
    let bounds = paint_layer_bounds_from_visual_bounds(visual_bounds, nominal_bounds);
    let content = RenderPaintLayerContent::from_composition_nodes(nodes);
    let content_generation =
        paint_layer_content_generation(&content.own_payload_render_nodes(), nominal_bounds);
    vec![RenderNode::PaintLayer(
        RenderPaintLayer::from_prepared_children(
            RenderPaintLayerBuildParts {
                id: PaintLayerId::new(element.id.to_wire_u64(), PaintLayerReason::Animation),
                bounds,
                placement: PaintLayerPlacement::Fixed,
                policy: PaintLayerPolicy::Cacheable,
                content_generation,
                visual_bounds,
            },
            content,
        ),
    )]
}

fn paint_layer_content_generation(own_nodes: &[RenderNode], payload_bounds: Rect) -> u64 {
    let mut hasher = FastHasher::new();
    hash_paint_layer_render_nodes(
        &mut hasher,
        own_nodes,
        PaintLayerHashFloat::Exact,
        Some(payload_bounds),
    );
    hasher.finish()
}

fn wrap_with_paint_layer(
    nodes: Vec<RenderNode>,
    stable_id: u64,
    placement: PaintLayerPlacement,
    policy: PaintLayerPolicy,
    reason: PaintLayerReason,
    render_frame: Frame,
    content_generation: Option<u64>,
) -> Vec<RenderNode> {
    let nominal_bounds = Rect {
        x: render_frame.x,
        y: render_frame.y,
        width: render_frame.width,
        height: render_frame.height,
    };
    let visual_bounds = paint_layer_own_content_visual_bounds(&nodes);
    let content = RenderPaintLayerContent::from_nodes(nodes);
    let own_nodes = content.own_render_nodes();
    let bounds = paint_layer_bounds_from_visual_bounds(visual_bounds, nominal_bounds);
    let content_generation = if let Some(content_generation) = content_generation {
        content_generation
    } else if policy.allows_payload_cache() {
        paint_layer_content_generation(&own_nodes, bounds)
    } else {
        0
    };

    vec![RenderNode::PaintLayer(
        RenderPaintLayer::from_prepared_children(
            RenderPaintLayerBuildParts {
                id: PaintLayerId::new(stable_id, reason),
                bounds,
                placement,
                policy,
                content_generation,
                visual_bounds,
            },
            content,
        ),
    )]
}

fn wrap_own_content_nodes(
    nodes: Vec<RenderNode>,
    attrs: &Attrs,
    kind: ElementKind,
    host_clip: ClipShape,
) -> Vec<RenderNode> {
    if nodes.is_empty() {
        return nodes;
    }

    if matches!(kind, ElementKind::Image | ElementKind::Video) {
        if !image_video_needs_own_host_clip(attrs) {
            return nodes;
        }

        return vec![RenderNode::RelaxedClip {
            clips: vec![host_clip],
            children: nodes,
        }];
    }

    wrap_with_host_clip(nodes, host_clip)
}

fn image_video_needs_own_host_clip(attrs: &Attrs) -> bool {
    attrs.padding.is_some() || attrs.border_width.is_some() || attrs.border_radius.is_some()
}

fn wrap_with_transform(
    nodes: Vec<RenderNode>,
    transform: crate::tree::transform::Affine2,
) -> Vec<RenderNode> {
    if nodes.is_empty() {
        return nodes;
    }

    if transform.is_identity() {
        return nodes;
    }

    vec![RenderNode::Transform {
        transform,
        children: nodes,
    }]
}

// One Raven patch: wrappers that always emit their node (see build_element_subtree).
fn wrap_with_transform_forced(
    nodes: Vec<RenderNode>,
    transform: crate::tree::transform::Affine2,
) -> Vec<RenderNode> {
    if nodes.is_empty() {
        return nodes;
    }
    vec![RenderNode::Transform {
        transform,
        children: nodes,
    }]
}

fn wrap_with_alpha_forced(nodes: Vec<RenderNode>, alpha: f32) -> Vec<RenderNode> {
    if nodes.is_empty() {
        return nodes;
    }
    vec![RenderNode::Alpha {
        alpha,
        children: nodes,
    }]
}

// One Raven patch: hand the render thread what it needs to re-evaluate this
// element's transform and alpha at any time: the animation, the frame it
// rotates and scales about, the element's non-animated values, and the values
// the scene was built with (which identify the nodes to update).
fn attach_renderer_animation(
    nodes: &mut [RenderNode],
    animation: super::animation::RendererDrivenAnimation,
    attrs: &Attrs,
    render_frame: Frame,
    built_transform: crate::tree::transform::Affine2,
    built_alpha: f32,
) {
    if let Some(RenderNode::PaintLayer(layer)) = nodes.first_mut() {
        layer.renderer_animation = Some(Arc::new(crate::render_scene::LayerRendererAnimation {
            spec: animation.spec,
            started_at: animation.started_at,
            revision: animation.revision,
            frame: render_frame,
            base: attrs.clone(),
            current_transform: built_transform,
            current_alpha: built_alpha,
            offset: (0.0, 0.0),
            bounds_transform: built_transform,
        }));
    }
}

fn wrap_with_alpha(nodes: Vec<RenderNode>, alpha: f32) -> Vec<RenderNode> {
    if nodes.is_empty() {
        return nodes;
    }

    if alpha >= 1.0 {
        return nodes;
    }

    vec![RenderNode::Alpha {
        alpha,
        children: nodes,
    }]
}

fn scene_bounds_for_root(tree: &ElementTree, root: NodeIx) -> Rect {
    tree.get_ix(root)
        .and_then(|element| element.layout.frame)
        .map(Rect::from_frame)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
