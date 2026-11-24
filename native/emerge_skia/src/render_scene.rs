use crate::render_color::RenderColor;
use std::collections::HashSet;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::renderer::measure_text_visual_metrics;
use crate::tree::attrs::{BorderStyle, ImageFit};
use crate::tree::geometry::{ClipShape, Rect};
use crate::tree::transform::Affine2;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderScene {
    /// Frozen native font facts used when this retained scene is painted later.
    #[doc(hidden)]
    pub fonts: Option<crate::renderer::FontSnapshot>,
    /// Immutable image bindings for native tree publication; None keeps manual
    /// low-level scenes' live-ID behavior.
    #[doc(hidden)]
    pub images: Option<crate::renderer::ImageSnapshot>,
    pub nodes: Vec<RenderNode>,
    /// One Raven patch: where a fling the render thread carries on stands in
    /// this scene (crate::scroll_fling).
    #[doc(hidden)]
    pub scroll_fling: Option<crate::scroll_fling::ScrollFlingAnchor>,
}

impl RenderScene {
    pub fn summary(&self) -> RenderSceneSummary {
        let mut summary = RenderSceneSummary::default();
        summary.record_nodes(&self.nodes);
        summary
    }

    pub fn has_payload_cache_candidate_layers(&self) -> bool {
        nodes_have_payload_cache_candidate_layers(&self.nodes)
    }

    /// Compatibility name retained for downstream renderer integrations.
    /// Reports whether the semantic layer tree contains any cacheable payload.
    pub fn has_cacheable_paint_layers(&self) -> bool {
        self.has_payload_cache_candidate_layers()
    }

    pub fn has_scroll_moving_paint_layers(&self) -> bool {
        nodes_have_scroll_moving_paint_layers(&self.nodes)
    }

    pub fn video_target_ids(&self) -> HashSet<String> {
        let mut targets = HashSet::new();
        collect_video_target_ids(&self.nodes, &mut targets);
        targets
    }
}

fn collect_video_target_ids(nodes: &[RenderNode], targets: &mut HashSet<String>) {
    nodes.iter().for_each(|node| match node {
        RenderNode::ShadowPass { children }
        | RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => collect_video_target_ids(children, targets),
        RenderNode::PaintLayer(layer) => collect_layer_video_target_ids(layer, targets),
        RenderNode::Primitive(DrawPrimitive::Video(_, _, _, _, target, _)) => {
            targets.insert(target.clone());
        }
        RenderNode::Primitive(_) => {}
    });
}

fn collect_video_target_ids_from_layer_content(
    content: &[RenderPaintLayerContentNode],
    targets: &mut HashSet<String>,
) {
    content.iter().for_each(|node| match node {
        RenderPaintLayerContentNode::Own(run) => collect_video_target_ids(&run.nodes, targets),
        RenderPaintLayerContentNode::Child(layer) => collect_layer_video_target_ids(layer, targets),
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            collect_video_target_ids_from_layer_content(children, targets)
        }
    });
}

// One Raven patch: a compositor scroll frame publishes the same scroll
// content layer at a new offset; its video targets are kept by its content
// `Arc` (held, so the pointer is not reused) instead of walking all of it
// again on the render thread for every scene.
thread_local! {
    static LAYER_VIDEO_TARGETS: std::cell::RefCell<
        Vec<(Arc<RenderPaintLayerContent>, Arc<[String]>)>,
    > = const { std::cell::RefCell::new(Vec::new()) };
}

fn collect_layer_video_target_ids(layer: &RenderPaintLayer, targets: &mut HashSet<String>) {
    if layer.id.role != PaintLayerReason::ScrollContent {
        collect_video_target_ids_from_layer_content(&layer.content.nodes, targets);
        return;
    }
    let memo = LAYER_VIDEO_TARGETS.with(|memo| {
        memo.borrow()
            .iter()
            .find(|(content, _)| Arc::ptr_eq(content, &layer.content))
            .map(|(_, ids)| Arc::clone(ids))
    });
    let ids = memo.unwrap_or_else(|| {
        let mut found = HashSet::new();
        collect_video_target_ids_from_layer_content(&layer.content.nodes, &mut found);
        let ids: Arc<[String]> = found.into_iter().collect::<Vec<_>>().into();
        LAYER_VIDEO_TARGETS.with(|memo| {
            let mut memo = memo.borrow_mut();
            if memo.len() >= 4 {
                memo.remove(0);
            }
            memo.push((Arc::clone(&layer.content), Arc::clone(&ids)));
        });
        ids
    });
    targets.extend(ids.iter().cloned());
}

fn nodes_have_payload_cache_candidate_layers(nodes: &[RenderNode]) -> bool {
    nodes.iter().any(|node| match node {
        RenderNode::ShadowPass { children }
        | RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => nodes_have_payload_cache_candidate_layers(children),
        RenderNode::PaintLayer(layer) => {
            layer.policy.allows_payload_cache()
                || layer_content_has_payload_cache_candidate(&layer.content.nodes)
        }
        RenderNode::Primitive(_) => false,
    })
}

fn layer_content_has_payload_cache_candidate(content: &[RenderPaintLayerContentNode]) -> bool {
    content.iter().any(|node| match node {
        RenderPaintLayerContentNode::Own(_) => false,
        RenderPaintLayerContentNode::Child(layer) => {
            layer.policy.allows_payload_cache()
                || layer_content_has_payload_cache_candidate(&layer.content.nodes)
        }
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            layer_content_has_payload_cache_candidate(children)
        }
    })
}

fn nodes_have_scroll_moving_paint_layers(nodes: &[RenderNode]) -> bool {
    nodes.iter().any(|node| match node {
        RenderNode::ShadowPass { children }
        | RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => nodes_have_scroll_moving_paint_layers(children),
        RenderNode::PaintLayer(layer) => {
            layer.placement == PaintLayerPlacement::ScrollMoving
                || layer_content_has_scroll_moving_paint_layer(&layer.content.nodes)
        }
        RenderNode::Primitive(_) => false,
    })
}

fn layer_content_has_scroll_moving_paint_layer(content: &[RenderPaintLayerContentNode]) -> bool {
    content.iter().any(|node| match node {
        RenderPaintLayerContentNode::Own(_) => false,
        RenderPaintLayerContentNode::Child(layer) => {
            layer.placement == PaintLayerPlacement::ScrollMoving
                || layer_content_has_scroll_moving_paint_layer(&layer.content.nodes)
        }
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            layer_content_has_scroll_moving_paint_layer(children)
        }
    })
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RenderSceneSummary {
    pub nodes: usize,
    pub shadow_passes: usize,
    pub clips: usize,
    pub relaxed_clips: usize,
    pub clip_shapes: usize,
    pub transforms: usize,
    pub alphas: usize,
    pub primitives: usize,
    pub rects: usize,
    pub rounded_rects: usize,
    pub borders: usize,
    pub border_corners: usize,
    pub border_edges: usize,
    pub shadows: usize,
    pub inset_shadows: usize,
    pub texts: usize,
    pub text_bytes: usize,
    pub gradients: usize,
    pub images: usize,
    pub videos: usize,
    pub image_loading: usize,
    pub image_failed: usize,
    pub paint_layers: usize,
    pub cacheable_layers: usize,
    pub moving_layers: usize,
    pub direct_only_layers: usize,
}

impl RenderSceneSummary {
    pub(crate) fn from_nodes(nodes: &[RenderNode]) -> Self {
        let mut summary = Self::default();
        summary.record_nodes(nodes);
        summary
    }

    fn record_nodes(&mut self, nodes: &[RenderNode]) {
        for node in nodes {
            self.nodes += 1;

            match node {
                RenderNode::ShadowPass { children } => {
                    self.shadow_passes += 1;
                    self.record_nodes(children);
                }
                RenderNode::Clip { clips, children } => {
                    self.clips += 1;
                    self.clip_shapes += clips.len();
                    self.record_nodes(children);
                }
                RenderNode::RelaxedClip { clips, children } => {
                    self.relaxed_clips += 1;
                    self.clip_shapes += clips.len();
                    self.record_nodes(children);
                }
                RenderNode::Transform { children, .. } => {
                    self.transforms += 1;
                    self.record_nodes(children);
                }
                RenderNode::Alpha { children, .. } => {
                    self.alphas += 1;
                    self.record_nodes(children);
                }
                RenderNode::PaintLayer(layer) => self.record_paint_layer(layer),
                RenderNode::Primitive(primitive) => self.record_primitive(primitive),
            }
        }
    }

    fn record_paint_layer(&mut self, layer: &RenderPaintLayer) {
        self.paint_layers += 1;
        match layer.policy {
            PaintLayerPolicy::Cacheable => self.cacheable_layers += 1,
            PaintLayerPolicy::DirectOnly => self.direct_only_layers += 1,
        }
        if layer.placement == PaintLayerPlacement::ScrollMoving {
            self.moving_layers += 1;
        }
        self.record_layer_content(&layer.content.nodes);
    }

    fn record_layer_content(&mut self, content: &[RenderPaintLayerContentNode]) {
        content.iter().for_each(|node| match node {
            RenderPaintLayerContentNode::Own(run) => self.record_nodes(&run.nodes),
            RenderPaintLayerContentNode::Child(layer) => self.record_paint_layer(layer),
            RenderPaintLayerContentNode::ShadowPass { children } => {
                self.nodes += 1;
                self.shadow_passes += 1;
                self.record_layer_content(children);
            }
            RenderPaintLayerContentNode::Clip { clips, children } => {
                self.nodes += 1;
                self.clips += 1;
                self.clip_shapes += clips.len();
                self.record_layer_content(children);
            }
            RenderPaintLayerContentNode::RelaxedClip { clips, children } => {
                self.nodes += 1;
                self.relaxed_clips += 1;
                self.clip_shapes += clips.len();
                self.record_layer_content(children);
            }
            RenderPaintLayerContentNode::Transform { children, .. } => {
                self.nodes += 1;
                self.transforms += 1;
                self.record_layer_content(children);
            }
            RenderPaintLayerContentNode::Alpha { children, .. } => {
                self.nodes += 1;
                self.alphas += 1;
                self.record_layer_content(children);
            }
        });
    }

    fn record_primitive(&mut self, primitive: &DrawPrimitive) {
        self.primitives += 1;
        self.gradients += usize::from(primitive.has_gradient());

        match primitive {
            DrawPrimitive::Rect(..) => self.rects += 1,
            DrawPrimitive::RoundedRect(..) => self.rounded_rects += 1,
            DrawPrimitive::Border(..) => self.borders += 1,
            DrawPrimitive::BorderCorners(..) => self.border_corners += 1,
            DrawPrimitive::BorderEdges(..) => self.border_edges += 1,
            DrawPrimitive::Shadow(..) => self.shadows += 1,
            DrawPrimitive::InsetShadow(..) => self.inset_shadows += 1,
            DrawPrimitive::TextWithFont(_, _, text, ..) => {
                self.texts += 1;
                self.text_bytes += text.len();
            }

            DrawPrimitive::Image(..) => self.images += 1,
            DrawPrimitive::Video(..) => self.videos += 1,
            DrawPrimitive::ImageLoading(..) => self.image_loading += 1,
            DrawPrimitive::ImageFailed(..) => self.image_failed += 1,
        }
    }
}

impl fmt::Display for RenderSceneSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            concat!(
                "nodes={} primitives={} scopes={{shadow_passes={} clips={} relaxed_clips={} ",
                "clip_shapes={} transforms={} alphas={}}} draws={{rects={} rounded_rects={} ",
                "borders={} border_corners={} border_edges={} shadows={} inset_shadows={} ",
                "texts={} text_bytes={} gradients={} images={} videos={} image_loading={} ",
                "image_failed={}}} paint_layers={{total={} cacheable={} moving={} direct_only={}}}"
            ),
            self.nodes,
            self.primitives,
            self.shadow_passes,
            self.clips,
            self.relaxed_clips,
            self.clip_shapes,
            self.transforms,
            self.alphas,
            self.rects,
            self.rounded_rects,
            self.borders,
            self.border_corners,
            self.border_edges,
            self.shadows,
            self.inset_shadows,
            self.texts,
            self.text_bytes,
            self.gradients,
            self.images,
            self.videos,
            self.image_loading,
            self.image_failed,
            self.paint_layers,
            self.cacheable_layers,
            self.moving_layers,
            self.direct_only_layers
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RenderNode {
    ShadowPass {
        children: Vec<RenderNode>,
    },
    Clip {
        clips: Vec<ClipShape>,
        children: Vec<RenderNode>,
    },
    RelaxedClip {
        clips: Vec<ClipShape>,
        children: Vec<RenderNode>,
    },
    Transform {
        transform: Affine2,
        children: Vec<RenderNode>,
    },
    Alpha {
        alpha: f32,
        children: Vec<RenderNode>,
    },
    PaintLayer(RenderPaintLayer),
    Primitive(DrawPrimitive),
}

/// One Raven patch: whether two scenes' nodes differ, stopping at the first
/// difference. Layers and runs sharing content compare by pointer. A scroll
/// step's scene differs near its root, so the presenter learns it must draw
/// without hashing every visible node (`visible_frame_fingerprint`).
pub(crate) fn render_nodes_differ(a: &[RenderNode], b: &[RenderNode]) -> bool {
    a.len() != b.len() || a.iter().zip(b).any(|(a, b)| render_node_differs(a, b))
}

fn render_node_differs(a: &RenderNode, b: &RenderNode) -> bool {
    use RenderNode as N;
    match (a, b) {
        (N::PaintLayer(a), N::PaintLayer(b)) => paint_layer_differs(a, b),
        (N::ShadowPass { children: a }, N::ShadowPass { children: b }) => {
            render_nodes_differ(a, b)
        }
        (
            N::Clip {
                clips: ca,
                children: a,
            },
            N::Clip {
                clips: cb,
                children: b,
            },
        )
        | (
            N::RelaxedClip {
                clips: ca,
                children: a,
            },
            N::RelaxedClip {
                clips: cb,
                children: b,
            },
        ) => ca != cb || render_nodes_differ(a, b),
        (
            N::Transform {
                transform: ta,
                children: a,
            },
            N::Transform {
                transform: tb,
                children: b,
            },
        ) => ta != tb || render_nodes_differ(a, b),
        (
            N::Alpha {
                alpha: aa,
                children: a,
            },
            N::Alpha {
                alpha: ab,
                children: b,
            },
        ) => aa != ab || render_nodes_differ(a, b),
        (N::Primitive(a), N::Primitive(b)) => a != b,
        _ => true,
    }
}

fn paint_layer_differs(a: &RenderPaintLayer, b: &RenderPaintLayer) -> bool {
    a.id != b.id
        || a.bounds != b.bounds
        || a.placement != b.placement
        || a.policy != b.policy
        || a.content_generation != b.content_generation
        || a.metrics != b.metrics
        || a.renderer_animation != b.renderer_animation
        || (!Arc::ptr_eq(&a.content, &b.content)
            && layer_content_differs(&a.content.nodes, &b.content.nodes))
}

fn layer_content_differs(a: &[RenderPaintLayerContentNode], b: &[RenderPaintLayerContentNode]) -> bool {
    use RenderPaintLayerContentNode as C;
    a.len() != b.len()
        || a.iter().zip(b).any(|(a, b)| match (a, b) {
            (C::Own(a), C::Own(b)) => {
                a.slot != b.slot
                    || a.bounds != b.bounds
                    || a.metrics != b.metrics
                    || (!Arc::ptr_eq(&a.nodes, &b.nodes) && render_nodes_differ(&a.nodes, &b.nodes))
            }
            (C::Child(a), C::Child(b)) => paint_layer_differs(a, b),
            (C::ShadowPass { children: a }, C::ShadowPass { children: b }) => {
                layer_content_differs(a, b)
            }
            (
                C::Clip {
                    clips: ca,
                    children: a,
                },
                C::Clip {
                    clips: cb,
                    children: b,
                },
            )
            | (
                C::RelaxedClip {
                    clips: ca,
                    children: a,
                },
                C::RelaxedClip {
                    clips: cb,
                    children: b,
                },
            ) => ca != cb || layer_content_differs(a, b),
            (
                C::Transform {
                    transform: ta,
                    children: a,
                },
                C::Transform {
                    transform: tb,
                    children: b,
                },
            ) => ta != tb || layer_content_differs(a, b),
            (
                C::Alpha {
                    alpha: aa,
                    children: a,
                },
                C::Alpha {
                    alpha: ab,
                    children: b,
                },
            ) => aa != ab || layer_content_differs(a, b),
            _ => true,
        })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PaintLayerId {
    pub node_id: u64,
    pub role: PaintLayerReason,
}

impl PaintLayerId {
    pub const fn new(node_id: u64, role: PaintLayerReason) -> Self {
        Self { node_id, role }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderPaintLayer {
    pub id: PaintLayerId,
    pub bounds: Rect,
    pub placement: PaintLayerPlacement,
    pub policy: PaintLayerPolicy,
    pub content_generation: u64,
    pub content: Arc<RenderPaintLayerContent>,
    pub metrics: RenderPaintLayerMetrics,
    /// One Raven patch: set when the render thread animates this layer.
    pub renderer_animation: Option<Arc<LayerRendererAnimation>>,
}

/// One Raven patch: a looping transform/alpha animation the render thread
/// samples itself. The current_* fields are the values the layer's Transform/Alpha
/// nodes hold now; each advance finds those nodes by value and replaces them.
#[derive(Clone, Debug)]
pub struct LayerRendererAnimation {
    pub spec: Arc<crate::tree::animation::AnimationSpec>,
    pub started_at: std::time::Instant,
    pub revision: u64,
    pub frame: crate::tree::element::Frame,
    pub base: AnimatedBase,
    pub current_transform: Affine2,
    pub current_alpha: f32,
    /// Translation scroll layers prepend to the element's transform.
    pub offset: (f32, f32),
    /// The transform the layer's bounds were measured under.
    pub bounds_transform: Affine2,
}

/// One Raven patch: the element's own (non-animated) values its transform
/// and alpha are computed from. The render thread samples every animated
/// layer every frame; it held the element's whole `Attrs` (~2.5 KB, with its
/// strings and lists), cloned it for each sample and again whenever the layer
/// was copied on write.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AnimatedBase {
    pub move_x: Option<f64>,
    pub move_y: Option<f64>,
    pub layout_rotate: Option<f64>,
    pub rotate: Option<f64>,
    pub scale: Option<f64>,
    pub alpha: Option<f64>,
}

impl AnimatedBase {
    pub fn of(attrs: &crate::tree::attrs::Attrs) -> Self {
        Self {
            move_x: attrs.move_x,
            move_y: attrs.move_y,
            layout_rotate: attrs.layout_rotate,
            rotate: attrs.rotate,
            scale: attrs.scale,
            alpha: attrs.alpha,
        }
    }

    /// These values with a sample laid over them, as
    /// `tree::animation::apply_sample_attrs` lays it over the attrs.
    fn with_sample(self, sample: &crate::tree::attrs::Attrs) -> Self {
        Self {
            move_x: sample.move_x.or(self.move_x),
            move_y: sample.move_y.or(self.move_y),
            layout_rotate: sample.layout_rotate.or(self.layout_rotate),
            rotate: sample.rotate.or(self.rotate),
            scale: sample.scale.or(self.scale),
            alpha: sample.alpha.or(self.alpha),
        }
    }
}

/// One Raven patch: result of advancing renderer-driven animations.
#[derive(Clone, Copy, Debug, Default)]
pub struct RendererAnimationAdvance {
    pub found: bool,
    /// Scene area whose pixels changed since the previous advance; None with
    /// `changed` when some change can't be bounded (redraw everything).
    pub damage: Option<Rect>,
    /// Whether any animated layer moved. A sample that rounds to what is
    /// already drawn (a hold in the keyframes, or motion far below a pixel)
    /// is not applied, so a frame with nothing changed needn't be drawn.
    pub changed: bool,
    unbounded: bool,
}

impl RendererAnimationAdvance {
    fn add_damage(&mut self, rect: Option<Rect>) {
        self.changed = true;
        match rect {
            Some(rect) if !self.unbounded => {
                self.damage = union_damage_rect(self.damage, rect);
            }
            _ => {
                self.unbounded = true;
                self.damage = None;
            }
        }
    }
}

fn union_damage_rect(a: Option<Rect>, b: Rect) -> Option<Rect> {
    Some(match a {
        None => b,
        Some(a) => {
            let x = a.x.min(b.x);
            let y = a.y.min(b.y);
            Rect {
                x,
                y,
                width: (a.x + a.width).max(b.x + b.width) - x,
                height: (a.y + a.height).max(b.y + b.height) - y,
            }
        }
    })
}

pub fn union_damage(a: Option<Rect>, b: Rect) -> Option<Rect> {
    union_damage_rect(a, b)
}

impl PartialEq for LayerRendererAnimation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.spec, &other.spec)
            && self.started_at == other.started_at
            && self.revision == other.revision
    }
}

impl LayerRendererAnimation {
    /// The element's transform and alpha at : its own values with the
    /// animation's sample laid over them, as the tree would compute them.
    fn sample(&self, at: std::time::Instant) -> (Affine2, f32) {
        let entry = crate::tree::animation::AnimationRuntimeEntry {
            spec_hash: self.revision,
            started_at: self.started_at,
        };
        let sample =
            crate::tree::animation::sample_animation_spec(&self.spec, Some(&entry), Some(at));
        let values = self.base.with_sample(&sample.attrs);
        let transform = crate::tree::transform::element_transform_of(
            self.frame,
            values.move_x,
            values.move_y,
            values.layout_rotate.or(values.rotate),
            values.scale,
        );
        let transform = if self.offset == (0.0, 0.0) {
            transform
        } else {
            Affine2::translation(self.offset.0, self.offset.1).then(transform)
        };
        (transform, values.alpha.unwrap_or(1.0) as f32)
    }
}

impl RenderScene {
    /// One Raven patch: move every renderer-driven animation in this scene to
    /// time at, in place. Returns whether the scene has any, i.e. whether it
    /// needs drawing again next vblank.
    pub fn advance_renderer_animations(
        &mut self,
        at: std::time::Instant,
    ) -> RendererAnimationAdvance {
        let mut advance = RendererAnimationAdvance::default();
        advance_nodes(&mut self.nodes, at, &mut advance);
        advance
    }
}

fn advance_nodes(
    nodes: &mut [RenderNode],
    at: std::time::Instant,
    found: &mut RendererAnimationAdvance,
) {
    for node in nodes.iter_mut() {
        match node {
            RenderNode::ShadowPass { children }
            | RenderNode::Clip { children, .. }
            | RenderNode::RelaxedClip { children, .. }
            | RenderNode::Transform { children, .. }
            | RenderNode::Alpha { children, .. } => advance_nodes(children, at, found),
            RenderNode::PaintLayer(layer) => advance_layer(layer, at, found),
            RenderNode::Primitive(_) => {}
        }
    }
}

fn layer_has_renderer_animation(layer: &RenderPaintLayer) -> bool {
    layer.renderer_animation.is_some() || content_has_renderer_animation(&layer.content.nodes)
}

#[cfg(test)]
thread_local! {
    /// Content nodes searched for renderer-driven animations (tests count
    /// how much of a scene each advance visits).
    pub(crate) static ANIMATION_SEARCH_NODES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

pub(crate) fn content_has_renderer_animation(nodes: &[RenderPaintLayerContentNode]) -> bool {
    #[cfg(test)]
    ANIMATION_SEARCH_NODES.with(|count| count.set(count.get() + nodes.len() as u64));
    nodes.iter().any(|node| match node {
        RenderPaintLayerContentNode::Own(_) => false,
        RenderPaintLayerContentNode::Child(layer) => layer_has_renderer_animation(layer),
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            content_has_renderer_animation(children)
        }
    })
}

fn advance_layer(
    layer: &mut RenderPaintLayer,
    at: std::time::Instant,
    found: &mut RendererAnimationAdvance,
) {
    // Copy-on-write only along paths that hold an animation; the rest of the
    // scene stays shared with the tree.
    if !layer_has_renderer_animation(layer) {
        return;
    }
    found.found = true;
    advance_own(layer, at, found);
    let content = Arc::make_mut(&mut layer.content);
    advance_content_children(&mut content.nodes, at, found);
}

/// One Raven patch: how far below a device pixel (and an 8-bit alpha step) a
/// new sample may differ from the drawn one and still count as unchanged.
/// Skipped samples aren't applied, so differences can't accumulate.
const UNCHANGED_CORNER_PX: f32 = 1.0 / 64.0;

fn layer_corners(layer: &RenderPaintLayer, screen: Affine2) -> [crate::tree::transform::Point; 4] {
    use crate::tree::transform::Point;
    let r = layer.bounds;
    [
        (r.x, r.y),
        (r.x + r.width, r.y),
        (r.x, r.y + r.height),
        (r.x + r.width, r.y + r.height),
    ]
    .map(|(x, y)| screen.map_point(Point { x, y }))
}

fn max_corner_distance(a: &[crate::tree::transform::Point; 4], b: &[crate::tree::transform::Point; 4]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(a, b)| ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt())
        .fold(0.0, f32::max)
}

fn alpha_byte(alpha: f32) -> u8 {
    (alpha.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Samples this layer's own animation at `at` and patches its own
/// Transform/Alpha nodes (not those of child layers), adding the old and new
/// screen area to the damage. Leaves the layer as it is when the sample
/// rounds to what is drawn.
fn advance_own(layer: &mut RenderPaintLayer, at: std::time::Instant, found: &mut RendererAnimationAdvance) {
    let Some(animation) = layer.renderer_animation.as_ref() else {
        return;
    };
    let (transform, alpha) = animation.sample(at);
    let (old_transform, old_alpha) = (animation.current_transform, animation.current_alpha);
    // Old and new screen area of this layer: its bounds were measured under
    // bounds_transform; map them to each transform.
    let inverse = animation.bounds_transform.inverse();
    let unchanged = inverse.is_some_and(|inverse| {
        alpha_byte(alpha) == alpha_byte(old_alpha)
            && max_corner_distance(
                &layer_corners(layer, old_transform.then(inverse)),
                &layer_corners(layer, transform.then(inverse)),
            ) < UNCHANGED_CORNER_PX
    });
    if unchanged {
        return;
    }
    let damage = inverse.map(|inverse| {
        let before = old_transform.then(inverse).map_rect_aabb(layer.bounds);
        let after = transform.then(inverse).map_rect_aabb(layer.bounds);
        let pad = |r: Rect| Rect {
            x: r.x - 4.0,
            y: r.y - 4.0,
            width: r.width + 8.0,
            height: r.height + 8.0,
        };
        union_damage_rect(Some(pad(before)), pad(after)).expect("union of two rects")
    });
    found.add_damage(damage);
    let content = Arc::make_mut(&mut layer.content);
    patch_content(&mut content.nodes, old_transform, transform, old_alpha, alpha);
    if let Some(animation) = layer.renderer_animation.as_mut() {
        let animation = Arc::make_mut(animation);
        animation.current_transform = transform;
        animation.current_alpha = alpha;
    }
}

/// One Raven patch: where the renderer-driven layers of one scene are, so
/// each frame visits only them (and their ancestors) instead of searching
/// the whole scene. A path is the index taken at each level of nesting:
/// into a node list, then into a layer's content, and so on, ending at an
/// animated layer. Paths are in tree order (a parent before its children).
#[derive(Debug, Default)]
pub struct RendererAnimationIndex {
    paths: Option<Vec<Vec<u32>>>,
    /// The node list the paths were taken in (a scene replaced without
    /// `invalidate` is caught here).
    scene_nodes: usize,
    motions: std::collections::HashMap<PaintLayerId, MotionEntry>,
}

#[derive(Debug)]
struct MotionEntry {
    spec: Arc<crate::tree::animation::AnimationSpec>,
    revision: u64,
    frame: crate::tree::element::Frame,
    base: AnimatedBase,
    bounds: Rect,
    bounds_transform: Affine2,
    motion: AnimationMotion,
}

/// One Raven patch: the fastest a renderer-driven layer changes, over one
/// period of its loop: corner movement in device pixels per second, and
/// alpha per second.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct AnimationMotion {
    pub px_per_second: f32,
    pub alpha_per_second: f32,
}

impl RendererAnimationIndex {
    /// Forget the paths (a new scene). Measured motions are kept by layer.
    pub fn invalidate(&mut self) {
        self.paths = None;
    }

    fn paths(&mut self, scene: &RenderScene) -> &[Vec<u32>] {
        let scene_nodes = scene.nodes.as_ptr() as usize;
        if self.paths.is_none() || self.scene_nodes != scene_nodes {
            let mut paths = Vec::new();
            collect_animation_paths_in_nodes(&scene.nodes, &mut Vec::new(), &mut paths);
            self.paths = Some(paths);
            self.scene_nodes = scene_nodes;
        }
        self.paths.as_deref().unwrap_or_default()
    }

    /// Same as `RenderScene::advance_renderer_animations`, visiting only the
    /// animated layers.
    pub fn advance(
        &mut self,
        scene: &mut RenderScene,
        at: std::time::Instant,
    ) -> RendererAnimationAdvance {
        let valid = |paths: &[Vec<u32>], scene: &RenderScene| {
            paths.iter().all(|path| {
                layer_at_path(&scene.nodes, path)
                    .is_some_and(|layer| layer.renderer_animation.is_some())
            })
        };
        if !valid(self.paths(scene), scene) {
            // The scene changed shape under the index without `invalidate`.
            self.paths = None;
        }
        // One Raven patch: borrow the paths (they were copied every frame).
        self.paths(scene);
        let paths = self.paths.as_deref().unwrap_or_default();
        let mut advance = RendererAnimationAdvance {
            found: !paths.is_empty(),
            ..RendererAnimationAdvance::default()
        };
        for path in paths {
            advance_at_path(&mut scene.nodes, path, at, &mut advance);
        }
        advance
    }

    /// The motion of every renderer-driven layer in the scene, measured once
    /// per animation and layout.
    pub fn motions(&mut self, scene: &RenderScene) -> Vec<AnimationMotion> {
        self.paths(scene);
        let paths = self.paths.as_deref().unwrap_or_default();
        let mut seen = HashSet::new();
        let motions = paths
            .iter()
            .filter_map(|path| layer_at_path(&scene.nodes, path))
            .filter_map(|layer| {
                let animation = layer.renderer_animation.as_deref()?;
                seen.insert(layer.id);
                let fresh = self.motions.get(&layer.id).is_some_and(|entry| {
                    Arc::ptr_eq(&entry.spec, &animation.spec)
                        && entry.revision == animation.revision
                        && entry.frame == animation.frame
                        && entry.bounds == layer.bounds
                        && entry.bounds_transform == animation.bounds_transform
                        && entry.base == animation.base
                });
                if !fresh {
                    self.motions.insert(
                        layer.id,
                        MotionEntry {
                            spec: animation.spec.clone(),
                            revision: animation.revision,
                            frame: animation.frame,
                            base: animation.base,
                            bounds: layer.bounds,
                            bounds_transform: animation.bounds_transform,
                            motion: measure_animation_motion(layer, animation),
                        },
                    );
                }
                self.motions.get(&layer.id).map(|entry| entry.motion)
            })
            .collect();
        self.motions.retain(|id, _| seen.contains(id));
        motions
    }
}

/// One Raven patch: samples one period of the loop (`MOTION_SAMPLES` times,
/// so a long loop costs no more to measure than a short one: this runs on the
/// render thread whenever the tree rebuilds the layer) and takes the largest
/// step between neighbouring samples, with a margin for the peak of an eased
/// curve falling between two samples. Corners are
/// enough: under an affine map a point's movement is affine in the point, so
/// its maximum over the layer's box is at a corner. The wrap from the last
/// keyframe back to the first is not motion and is left out.
pub(crate) fn measure_animation_motion(
    layer: &RenderPaintLayer,
    animation: &LayerRendererAnimation,
) -> AnimationMotion {
    let period = animation.spec.duration_ms.max(0.0) / 1000.0;
    let Some(inverse) = animation.bounds_transform.inverse() else {
        return AnimationMotion {
            px_per_second: f32::INFINITY,
            alpha_per_second: f32::INFINITY,
        };
    };
    if !(period > 0.0) || !period.is_finite() {
        return AnimationMotion::default();
    }
    let steps = MOTION_SAMPLES;
    let dt = period / steps as f64;
    let at = |k: usize| {
        // The last sample stays just inside the period.
        let t = if k == steps { period - 1e-6 } else { k as f64 * dt };
        animation.started_at + std::time::Duration::from_secs_f64(t.max(0.0))
    };
    let mut motion = AnimationMotion::default();
    let (transform, mut alpha) = animation.sample(at(0));
    let mut corners = layer_corners(layer, transform.then(inverse));
    for k in 1..=steps {
        let (transform, next_alpha) = animation.sample(at(k));
        let next_corners = layer_corners(layer, transform.then(inverse));
        let seconds = if k == steps { dt - 1e-6 } else { dt } as f32;
        if seconds > 0.0 {
            motion.px_per_second = motion
                .px_per_second
                .max(max_corner_distance(&corners, &next_corners) / seconds);
            motion.alpha_per_second = motion
                .alpha_per_second
                .max((next_alpha - alpha).abs() / seconds);
        }
        corners = next_corners;
        alpha = next_alpha;
    }
    AnimationMotion {
        px_per_second: motion.px_per_second * MOTION_MARGIN,
        alpha_per_second: motion.alpha_per_second * MOTION_MARGIN,
    }
}

const MOTION_SAMPLES: usize = 96;
const MOTION_MARGIN: f32 = 1.1;

fn collect_animation_paths_in_nodes(
    nodes: &[RenderNode],
    path: &mut Vec<u32>,
    paths: &mut Vec<Vec<u32>>,
) {
    for (index, node) in nodes.iter().enumerate() {
        path.push(index as u32);
        match node {
            RenderNode::ShadowPass { children }
            | RenderNode::Clip { children, .. }
            | RenderNode::RelaxedClip { children, .. }
            | RenderNode::Transform { children, .. }
            | RenderNode::Alpha { children, .. } => {
                collect_animation_paths_in_nodes(children, path, paths)
            }
            RenderNode::PaintLayer(layer) => collect_animation_paths_in_layer(layer, path, paths),
            RenderNode::Primitive(_) => {}
        }
        path.pop();
    }
}

fn collect_animation_paths_in_layer(
    layer: &RenderPaintLayer,
    path: &mut Vec<u32>,
    paths: &mut Vec<Vec<u32>>,
) {
    if layer.renderer_animation.is_some() {
        paths.push(path.clone());
    }
    collect_animation_paths_in_content(&layer.content.nodes, path, paths);
}

fn collect_animation_paths_in_content(
    nodes: &[RenderPaintLayerContentNode],
    path: &mut Vec<u32>,
    paths: &mut Vec<Vec<u32>>,
) {
    for (index, node) in nodes.iter().enumerate() {
        path.push(index as u32);
        match node {
            RenderPaintLayerContentNode::Own(_) => {}
            RenderPaintLayerContentNode::Child(layer) => {
                collect_animation_paths_in_layer(layer, path, paths)
            }
            RenderPaintLayerContentNode::ShadowPass { children }
            | RenderPaintLayerContentNode::Clip { children, .. }
            | RenderPaintLayerContentNode::RelaxedClip { children, .. }
            | RenderPaintLayerContentNode::Transform { children, .. }
            | RenderPaintLayerContentNode::Alpha { children, .. } => {
                collect_animation_paths_in_content(children, path, paths)
            }
        }
        path.pop();
    }
}

fn layer_at_path<'a>(nodes: &'a [RenderNode], path: &[u32]) -> Option<&'a RenderPaintLayer> {
    let (&index, rest) = path.split_first()?;
    match nodes.get(index as usize)? {
        RenderNode::ShadowPass { children }
        | RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => layer_at_path(children, rest),
        RenderNode::PaintLayer(layer) => layer_at_content_path(layer, rest),
        RenderNode::Primitive(_) => None,
    }
}

fn layer_at_content_path<'a>(layer: &'a RenderPaintLayer, path: &[u32]) -> Option<&'a RenderPaintLayer> {
    let Some((&index, rest)) = path.split_first() else {
        return Some(layer);
    };
    match layer.content.nodes.get(index as usize)? {
        RenderPaintLayerContentNode::Child(child) => layer_at_content_path(child, rest),
        node => layer_at_content_children_path(node, rest),
    }
}

fn layer_at_content_children_path<'a>(
    node: &'a RenderPaintLayerContentNode,
    path: &[u32],
) -> Option<&'a RenderPaintLayer> {
    match node {
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            let (&index, rest) = path.split_first()?;
            match children.get(index as usize)? {
                RenderPaintLayerContentNode::Child(child) => layer_at_content_path(child, rest),
                node => layer_at_content_children_path(node, rest),
            }
        }
        RenderPaintLayerContentNode::Own(_) | RenderPaintLayerContentNode::Child(_) => None,
    }
}

/// Advances the animated layer at `path`; false when the path doesn't lead
/// to one. Layers on the way are copied on write, as in a full advance.
fn advance_at_path(
    nodes: &mut [RenderNode],
    path: &[u32],
    at: std::time::Instant,
    advance: &mut RendererAnimationAdvance,
) -> bool {
    let Some((&index, rest)) = path.split_first() else {
        return false;
    };
    match nodes.get_mut(index as usize) {
        Some(
            RenderNode::ShadowPass { children }
            | RenderNode::Clip { children, .. }
            | RenderNode::RelaxedClip { children, .. }
            | RenderNode::Transform { children, .. }
            | RenderNode::Alpha { children, .. },
        ) => advance_at_path(children, rest, at, advance),
        Some(RenderNode::PaintLayer(layer)) => advance_at_layer_path(layer, rest, at, advance),
        _ => false,
    }
}

fn advance_at_layer_path(
    layer: &mut RenderPaintLayer,
    path: &[u32],
    at: std::time::Instant,
    advance: &mut RendererAnimationAdvance,
) -> bool {
    let Some((&index, rest)) = path.split_first() else {
        if layer.renderer_animation.is_none() {
            return false;
        }
        advance_own(layer, at, advance);
        return true;
    };
    if layer
        .content
        .nodes
        .get(index as usize)
        .is_none_or(|node| matches!(node, RenderPaintLayerContentNode::Own(_)))
    {
        return false;
    }
    let content = Arc::make_mut(&mut layer.content);
    advance_at_content_path(&mut content.nodes[index as usize], rest, at, advance)
}

fn advance_at_content_path(
    node: &mut RenderPaintLayerContentNode,
    path: &[u32],
    at: std::time::Instant,
    advance: &mut RendererAnimationAdvance,
) -> bool {
    match node {
        RenderPaintLayerContentNode::Child(child) => advance_at_layer_path(child, path, at, advance),
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            let Some((&index, rest)) = path.split_first() else {
                return false;
            };
            match children.get_mut(index as usize) {
                Some(node) => advance_at_content_path(node, rest, at, advance),
                None => false,
            }
        }
        RenderPaintLayerContentNode::Own(_) => false,
    }
}

// Replaces this layer's own Transform/Alpha nodes (not those of child layers,
// which carry their own animations).
pub(crate) fn patch_content(
    nodes: &mut [RenderPaintLayerContentNode],
    old_transform: Affine2,
    transform: Affine2,
    old_alpha: f32,
    alpha: f32,
) {
    for node in nodes.iter_mut() {
        match node {
            RenderPaintLayerContentNode::Transform {
                transform: t,
                children,
            } => {
                if *t == old_transform {
                    *t = transform;
                }
                patch_content(children, old_transform, transform, old_alpha, alpha);
            }
            RenderPaintLayerContentNode::Alpha { alpha: a, children } => {
                if *a == old_alpha {
                    *a = alpha;
                }
                patch_content(children, old_transform, transform, old_alpha, alpha);
            }
            RenderPaintLayerContentNode::ShadowPass { children }
            | RenderPaintLayerContentNode::Clip { children, .. }
            | RenderPaintLayerContentNode::RelaxedClip { children, .. } => {
                patch_content(children, old_transform, transform, old_alpha, alpha)
            }
            RenderPaintLayerContentNode::Own(_) | RenderPaintLayerContentNode::Child(_) => {}
        }
    }
}

/// One Raven patch: a renderer-driven layer the renderer may draw as one
/// cached texture of its whole subtree (child layers included): its
/// animation is transform-only, and nothing inside it moves on its own or is
/// live media.
pub(crate) fn renderer_animation_snapshot_eligible(layer: &RenderPaintLayer) -> bool {
    let Some(animation) = layer.renderer_animation.as_deref() else {
        return false;
    };
    animation
        .spec
        .keyframes
        .iter()
        .all(|attrs| attrs.alpha.is_none())
        && !content_has_renderer_animation(&layer.content.nodes)
        && !content_has_media(&layer.content.nodes)
}

fn content_has_media(nodes: &[RenderPaintLayerContentNode]) -> bool {
    nodes.iter().any(|node| match node {
        RenderPaintLayerContentNode::Own(_) => false,
        RenderPaintLayerContentNode::Child(layer) => {
            layer.id.role == PaintLayerReason::DirectMedia
                || content_has_media(&layer.content.nodes)
        }
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => content_has_media(children),
    })
}

/// One Raven patch: identity of a renderer-driven layer's subtree content,
/// apart from its own animated transform. Paint runs are compared by their
/// shared payload (the tree replaces it whenever the content changes), and
/// every structural value and child layer is included.
pub(crate) fn renderer_animation_snapshot_key(layer: &RenderPaintLayer) -> u64 {
    let own = layer
        .renderer_animation
        .as_deref()
        .map(|animation| animation.current_transform);
    let mut hasher = crate::fast_hash::FastHasher::new();
    hash_rect(&mut hasher, layer.bounds);
    hash_snapshot_content(&mut hasher, &layer.content.nodes, own);
    hasher.finish()
}

fn hash_rect(hasher: &mut impl Hasher, rect: Rect) {
    for value in [rect.x, rect.y, rect.width, rect.height] {
        value.to_bits().hash(hasher);
    }
}

fn hash_snapshot_content(
    hasher: &mut impl Hasher,
    nodes: &[RenderPaintLayerContentNode],
    own_transform: Option<Affine2>,
) {
    for node in nodes {
        match node {
            RenderPaintLayerContentNode::Own(run) => {
                0u8.hash(hasher);
                (Arc::as_ptr(&run.nodes) as usize).hash(hasher);
                hash_rect(hasher, run.bounds);
            }
            RenderPaintLayerContentNode::Child(child) => {
                1u8.hash(hasher);
                child.id.hash(hasher);
                hash_rect(hasher, child.bounds);
                hash_snapshot_content(hasher, &child.content.nodes, None);
            }
            RenderPaintLayerContentNode::ShadowPass { children } => {
                2u8.hash(hasher);
                hash_snapshot_content(hasher, children, own_transform);
            }
            RenderPaintLayerContentNode::Clip { clips, children }
            | RenderPaintLayerContentNode::RelaxedClip { clips, children } => {
                3u8.hash(hasher);
                for clip in clips {
                    hash_rect(hasher, clip.rect);
                    format!("{:?}", clip.radii).hash(hasher);
                }
                hash_snapshot_content(hasher, children, own_transform);
            }
            RenderPaintLayerContentNode::Transform {
                transform,
                children,
            } => {
                4u8.hash(hasher);
                if Some(*transform) != own_transform {
                    for value in [
                        transform.xx,
                        transform.yx,
                        transform.xy,
                        transform.yy,
                        transform.tx,
                        transform.ty,
                    ] {
                        value.to_bits().hash(hasher);
                    }
                }
                hash_snapshot_content(hasher, children, own_transform);
            }
            RenderPaintLayerContentNode::Alpha { alpha, children } => {
                5u8.hash(hasher);
                alpha.to_bits().hash(hasher);
                hash_snapshot_content(hasher, children, own_transform);
            }
        }
    }
}

fn advance_content_children(
    nodes: &mut [RenderPaintLayerContentNode],
    at: std::time::Instant,
    found: &mut RendererAnimationAdvance,
) {
    for node in nodes.iter_mut() {
        match node {
            RenderPaintLayerContentNode::Child(layer) => advance_layer(layer, at, found),
            RenderPaintLayerContentNode::ShadowPass { children }
            | RenderPaintLayerContentNode::Clip { children, .. }
            | RenderPaintLayerContentNode::RelaxedClip { children, .. }
            | RenderPaintLayerContentNode::Transform { children, .. }
            | RenderPaintLayerContentNode::Alpha { children, .. } => {
                advance_content_children(children, at, found)
            }
            RenderPaintLayerContentNode::Own(_) => {}
        }
    }
}

impl RenderPaintLayer {
    pub fn from_children(
        stable_id: u64,
        bounds: Rect,
        placement: PaintLayerPlacement,
        policy: PaintLayerPolicy,
        reason: PaintLayerReason,
        content_generation: u64,
        children: Vec<RenderNode>,
    ) -> Self {
        Self::from_prepared_children(
            RenderPaintLayerBuildParts {
                id: PaintLayerId::new(stable_id, reason),
                bounds,
                placement,
                policy,
                content_generation,
                visual_bounds: None,
            },
            RenderPaintLayerContent::from_nodes(children),
        )
    }

    pub(crate) fn from_prepared_children(
        parts: RenderPaintLayerBuildParts,
        mut content: RenderPaintLayerContent,
    ) -> Self {
        content.assign_run_metadata(parts.bounds);
        let metrics = RenderPaintLayerMetrics::from_content(
            &content.nodes,
            parts.bounds,
            parts.visual_bounds,
        );

        Self {
            id: parts.id,
            bounds: parts.bounds,
            placement: parts.placement,
            policy: parts.policy,
            content_generation: parts.content_generation,
            content: Arc::new(content),
            metrics,
            renderer_animation: None,
        }
    }

    pub(crate) fn content_nodes(&self) -> Vec<RenderNode> {
        self.content.to_render_nodes()
    }

    pub(crate) fn with_children(&self, children: Vec<RenderNode>) -> Self {
        self.with_bounds_and_children(self.bounds, children)
    }

    pub(crate) fn with_bounds_and_children(&self, bounds: Rect, children: Vec<RenderNode>) -> Self {
        let mut layer = Self::from_prepared_children(
            RenderPaintLayerBuildParts {
                id: self.id,
                bounds,
                placement: self.placement,
                policy: self.policy,
                content_generation: self.content_generation,
                visual_bounds: None,
            },
            RenderPaintLayerContent::from_composition_nodes(children),
        );
        // One Raven patch: a rebuilt layer keeps its renderer-driven animation.
        layer.renderer_animation = self.renderer_animation.clone();
        layer
    }

    pub(crate) fn child_layer_count(&self) -> usize {
        self.content.child_layer_count()
    }

    #[cfg(test)]
    pub(crate) fn own_render_nodes(&self) -> Vec<RenderNode> {
        self.content.own_render_nodes()
    }

    #[cfg(test)]
    pub(crate) fn own_runs(&self) -> Vec<&RenderPaintRun> {
        let mut runs = Vec::new();
        collect_layer_own_runs(&self.content.nodes, &mut runs);
        runs
    }

    #[cfg(test)]
    pub(crate) fn descendant_layers(&self) -> Vec<&RenderPaintLayer> {
        let mut layers = Vec::new();
        collect_layer_descendants(&self.content.nodes, &mut layers);
        layers
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RenderPaintLayerBuildParts {
    pub id: PaintLayerId,
    pub bounds: Rect,
    pub placement: PaintLayerPlacement,
    pub policy: PaintLayerPolicy,
    pub content_generation: u64,
    pub visual_bounds: Option<Rect>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RenderPaintLayerMetrics {
    pub own_node_count: u32,
    pub own_primitive_count: u32,
    pub own_primitive_cost: u64,
    pub payload_pixels: u64,
    pub visual_bounds: Rect,
    pub opaque: bool,
}

impl RenderPaintLayerMetrics {
    fn from_content(
        content: &[RenderPaintLayerContentNode],
        bounds: Rect,
        visual_bounds: Option<Rect>,
    ) -> Self {
        let own_nodes = layer_content_own_render_nodes(content);
        let visual_bounds = visual_bounds
            .or_else(|| render_nodes_visual_bounds(&own_nodes, Affine2::identity()))
            .map(|content_bounds| union_rect(bounds, content_bounds))
            .unwrap_or(bounds);
        let node_metrics = render_nodes_metrics(&own_nodes);

        Self {
            own_node_count: node_metrics.node_count.min(u32::MAX as usize) as u32,
            own_primitive_count: node_metrics.primitive_count.min(u32::MAX as usize) as u32,
            own_primitive_cost: node_metrics.primitive_cost,
            payload_pixels: paint_layer_payload_pixels(bounds),
            visual_bounds,
            opaque: false,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RenderPaintLayerContent {
    pub nodes: Vec<RenderPaintLayerContentNode>,
    /// One Raven patch: whether a shadow pass is anywhere in this content,
    /// nested layers included, worked out once when the content is made.
    /// Every clip wrap on the way up from a change asked this of everything
    /// below it, walking whole scroll contents once per ancestor (22% of the
    /// tree actor's time in a hard fling on the hub's Dashboard). The render
    /// thread rewrites transforms and alphas in place, never shadow passes.
    pub shadow_pass: bool,
    /// One Raven patch: which build this content came from. Copies keep it,
    /// so content the render thread copied on write to advance a
    /// renderer-driven animation in it is still known as the same build
    /// (the scroll tiles keep their plan).
    pub source: ContentSource,
}

/// One Raven patch: a build's identity (`RenderPaintLayerContent::source`).
/// It never makes two contents unequal.
#[derive(Clone, Copy, Debug, Default)]
pub struct ContentSource(u64);

impl ContentSource {
    fn next() -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self(NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
    }

    pub fn same_build(self, other: Self) -> bool {
        self.0 != 0 && self.0 == other.0
    }
}

impl PartialEq for ContentSource {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl RenderPaintLayerContent {
    pub(crate) fn new(nodes: Vec<RenderPaintLayerContentNode>) -> Self {
        let shadow_pass = layer_content_has_shadow_pass(&nodes);
        Self {
            nodes,
            shadow_pass,
            source: ContentSource::next(),
        }
    }

    pub(crate) fn from_nodes(nodes: Vec<RenderNode>) -> Self {
        Self::new(build_ordered_layer_content(nodes))
    }

    pub(crate) fn from_composition_nodes(nodes: Vec<RenderNode>) -> Self {
        Self::new(build_composition_layer_content(nodes))
    }

    pub(crate) fn own_render_nodes(&self) -> Vec<RenderNode> {
        layer_content_own_render_nodes(&self.nodes)
    }

    pub(crate) fn own_payload_render_nodes(&self) -> Vec<RenderNode> {
        let mut nodes = Vec::new();
        collect_layer_own_payload_nodes(&self.nodes, &mut nodes);
        nodes
    }

    pub(crate) fn to_render_nodes(&self) -> Vec<RenderNode> {
        layer_content_to_render_nodes(&self.nodes)
    }

    pub(crate) fn child_layer_count(&self) -> usize {
        layer_content_child_layer_count(&self.nodes)
    }

    fn assign_run_metadata(&mut self, bounds: Rect) {
        let mut next_slot = 0;
        assign_layer_run_metadata(&mut self.nodes, bounds, &mut next_slot);
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum RenderPaintLayerContentNode {
    Own(RenderPaintRun),
    Child(RenderPaintLayer),
    ShadowPass {
        children: Vec<RenderPaintLayerContentNode>,
    },
    Clip {
        clips: Vec<ClipShape>,
        children: Vec<RenderPaintLayerContentNode>,
    },
    RelaxedClip {
        clips: Vec<ClipShape>,
        children: Vec<RenderPaintLayerContentNode>,
    },
    Transform {
        transform: Affine2,
        children: Vec<RenderPaintLayerContentNode>,
    },
    Alpha {
        alpha: f32,
        children: Vec<RenderPaintLayerContentNode>,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct RenderPaintRun {
    pub slot: u32,
    pub bounds: Rect,
    pub nodes: Arc<Vec<RenderNode>>,
    pub metrics: RenderPaintLayerMetrics,
}

/// One Raven patch: a new scene takes the render thread's copies of the
/// scroll contents it shares with the scene before it. Advancing a
/// renderer-driven animation in scroll content copies that content on write,
/// and a scene the tree sends shares it with the tree's cache, so every scene
/// of a scroll or fling copied the whole content again on its first frame
/// (18% of the render thread in a hard fling on the Dashboard). A copy of the
/// same build (`ContentSource`) differs only in the animated values, which
/// the next advance sets anyway, and nothing else holds it, so later
/// advances change it in place.
pub(crate) fn adopt_advanced_scroll_content(new: &mut [RenderNode], old: &[RenderNode]) {
    let mut copies = Vec::new();
    collect_scroll_contents(old, &mut copies);
    if copies.is_empty() {
        return;
    }
    adopt_in_nodes(new, &copies);
}

type ScrollContentCopy = (PaintLayerId, Arc<RenderPaintLayerContent>);

fn collect_scroll_contents(nodes: &[RenderNode], out: &mut Vec<ScrollContentCopy>) {
    for node in nodes {
        match node {
            RenderNode::ShadowPass { children }
            | RenderNode::Clip { children, .. }
            | RenderNode::RelaxedClip { children, .. }
            | RenderNode::Transform { children, .. }
            | RenderNode::Alpha { children, .. } => collect_scroll_contents(children, out),
            RenderNode::PaintLayer(layer) => collect_scroll_layer(layer, out),
            RenderNode::Primitive(_) => {}
        }
    }
}

fn collect_scroll_layer(layer: &RenderPaintLayer, out: &mut Vec<ScrollContentCopy>) {
    if layer.id.role == PaintLayerReason::ScrollContent {
        out.push((layer.id, Arc::clone(&layer.content)));
        return;
    }
    collect_scroll_content_nodes(&layer.content.nodes, out);
}

fn collect_scroll_content_nodes(nodes: &[RenderPaintLayerContentNode], out: &mut Vec<ScrollContentCopy>) {
    for node in nodes {
        match node {
            RenderPaintLayerContentNode::Own(run) => collect_scroll_contents(&run.nodes, out),
            RenderPaintLayerContentNode::Child(layer) => collect_scroll_layer(layer, out),
            RenderPaintLayerContentNode::ShadowPass { children }
            | RenderPaintLayerContentNode::Clip { children, .. }
            | RenderPaintLayerContentNode::RelaxedClip { children, .. }
            | RenderPaintLayerContentNode::Transform { children, .. }
            | RenderPaintLayerContentNode::Alpha { children, .. } => {
                collect_scroll_content_nodes(children, out)
            }
        }
    }
}

fn adoptable<'a>(layer: &RenderPaintLayer, copies: &'a [ScrollContentCopy]) -> Option<&'a Arc<RenderPaintLayerContent>> {
    copies
        .iter()
        .find(|(id, copy)| {
            *id == layer.id
                && !Arc::ptr_eq(copy, &layer.content)
                && copy.source.same_build(layer.content.source)
        })
        .map(|(_, copy)| copy)
}

fn layer_adopts(layer: &RenderPaintLayer, copies: &[ScrollContentCopy]) -> bool {
    if layer.id.role == PaintLayerReason::ScrollContent {
        return adoptable(layer, copies).is_some();
    }
    content_adopts(&layer.content.nodes, copies)
}

fn nodes_adopt(nodes: &[RenderNode], copies: &[ScrollContentCopy]) -> bool {
    nodes.iter().any(|node| match node {
        RenderNode::ShadowPass { children }
        | RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => nodes_adopt(children, copies),
        RenderNode::PaintLayer(layer) => layer_adopts(layer, copies),
        RenderNode::Primitive(_) => false,
    })
}

fn content_adopts(nodes: &[RenderPaintLayerContentNode], copies: &[ScrollContentCopy]) -> bool {
    nodes.iter().any(|node| match node {
        RenderPaintLayerContentNode::Own(run) => nodes_adopt(&run.nodes, copies),
        RenderPaintLayerContentNode::Child(layer) => layer_adopts(layer, copies),
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => content_adopts(children, copies),
    })
}

fn adopt_in_nodes(nodes: &mut [RenderNode], copies: &[ScrollContentCopy]) {
    for node in nodes {
        match node {
            RenderNode::ShadowPass { children }
            | RenderNode::Clip { children, .. }
            | RenderNode::RelaxedClip { children, .. }
            | RenderNode::Transform { children, .. }
            | RenderNode::Alpha { children, .. } => adopt_in_nodes(children, copies),
            RenderNode::PaintLayer(layer) => adopt_in_layer(layer, copies),
            RenderNode::Primitive(_) => {}
        }
    }
}

fn adopt_in_layer(layer: &mut RenderPaintLayer, copies: &[ScrollContentCopy]) {
    if layer.id.role == PaintLayerReason::ScrollContent {
        if let Some(copy) = adoptable(layer, copies) {
            layer.content = Arc::clone(copy);
        }
        return;
    }
    if content_adopts(&layer.content.nodes, copies) {
        adopt_in_content(&mut Arc::make_mut(&mut layer.content).nodes, copies);
    }
}

fn adopt_in_content(nodes: &mut [RenderPaintLayerContentNode], copies: &[ScrollContentCopy]) {
    for node in nodes {
        match node {
            RenderPaintLayerContentNode::Own(run) => {
                if nodes_adopt(&run.nodes, copies) {
                    adopt_in_nodes(Arc::make_mut(&mut run.nodes).as_mut_slice(), copies);
                }
            }
            RenderPaintLayerContentNode::Child(layer) => adopt_in_layer(layer, copies),
            RenderPaintLayerContentNode::ShadowPass { children }
            | RenderPaintLayerContentNode::Clip { children, .. }
            | RenderPaintLayerContentNode::RelaxedClip { children, .. }
            | RenderPaintLayerContentNode::Transform { children, .. }
            | RenderPaintLayerContentNode::Alpha { children, .. } => adopt_in_content(children, copies),
        }
    }
}

/// One Raven patch: whether `nodes` hold a shadow pass, taking each nested
/// layer's answer from its content.
pub(crate) fn layer_content_has_shadow_pass(nodes: &[RenderPaintLayerContentNode]) -> bool {
    nodes.iter().any(|node| match node {
        RenderPaintLayerContentNode::Own(run) => run.nodes.iter().any(render_node_has_shadow_pass),
        RenderPaintLayerContentNode::Child(layer) => layer.content.shadow_pass,
        RenderPaintLayerContentNode::ShadowPass { .. } => true,
        RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            layer_content_has_shadow_pass(children)
        }
    })
}

/// One Raven patch: whether `node` holds a shadow pass (see
/// `RenderPaintLayerContent::shadow_pass`).
pub(crate) fn render_node_has_shadow_pass(node: &RenderNode) -> bool {
    match node {
        RenderNode::ShadowPass { .. } => true,
        RenderNode::PaintLayer(layer) => layer.content.shadow_pass,
        RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => children.iter().any(render_node_has_shadow_pass),
        RenderNode::Primitive(_) => false,
    }
}

impl RenderPaintRun {
    fn from_node(node: RenderNode) -> Self {
        Self {
            slot: 0,
            bounds: Rect::default(),
            nodes: Arc::new(vec![node]),
            metrics: RenderPaintLayerMetrics::default(),
        }
    }
}

fn build_ordered_layer_content(nodes: Vec<RenderNode>) -> Vec<RenderPaintLayerContentNode> {
    nodes.into_iter().fold(Vec::new(), |mut content, node| {
        append_ordered_layer_node(&mut content, node);
        content
    })
}

fn append_ordered_layer_node(content: &mut Vec<RenderPaintLayerContentNode>, node: RenderNode) {
    match node {
        RenderNode::PaintLayer(layer) => content.push(RenderPaintLayerContentNode::Child(layer)),
        RenderNode::ShadowPass { children } => {
            content.push(RenderPaintLayerContentNode::ShadowPass {
                children: build_ordered_layer_content(children),
            });
        }
        RenderNode::Clip { clips, children } if render_nodes_contain_paint_layer(&children) => {
            content.push(RenderPaintLayerContentNode::Clip {
                clips,
                children: build_ordered_layer_content(children),
            });
        }
        RenderNode::RelaxedClip { clips, children }
            if render_nodes_contain_paint_layer(&children) =>
        {
            content.push(RenderPaintLayerContentNode::RelaxedClip {
                clips,
                children: build_ordered_layer_content(children),
            });
        }
        RenderNode::Transform {
            transform,
            children,
        } if render_nodes_contain_paint_layer(&children) => {
            content.push(RenderPaintLayerContentNode::Transform {
                transform,
                children: build_ordered_layer_content(children),
            });
        }
        RenderNode::Alpha { alpha, children } if render_nodes_contain_paint_layer(&children) => {
            content.push(RenderPaintLayerContentNode::Alpha {
                alpha,
                children: build_ordered_layer_content(children),
            });
        }
        own => append_own_render_node(content, own),
    }
}

fn build_composition_layer_content(nodes: Vec<RenderNode>) -> Vec<RenderPaintLayerContentNode> {
    nodes.into_iter().fold(Vec::new(), |mut content, node| {
        append_composition_layer_node(&mut content, node);
        content
    })
}

fn append_composition_layer_node(content: &mut Vec<RenderPaintLayerContentNode>, node: RenderNode) {
    match node {
        RenderNode::PaintLayer(layer) => content.push(RenderPaintLayerContentNode::Child(layer)),
        RenderNode::ShadowPass { children } => {
            content.push(RenderPaintLayerContentNode::ShadowPass {
                children: build_composition_layer_content(children),
            })
        }
        RenderNode::Clip { clips, children } => content.push(RenderPaintLayerContentNode::Clip {
            clips,
            children: build_composition_layer_content(children),
        }),
        RenderNode::RelaxedClip { clips, children } => {
            content.push(RenderPaintLayerContentNode::RelaxedClip {
                clips,
                children: build_composition_layer_content(children),
            });
        }
        RenderNode::Transform {
            transform,
            children,
        } => {
            content.push(RenderPaintLayerContentNode::Transform {
                transform,
                children: build_composition_layer_content(children),
            });
        }
        RenderNode::Alpha { alpha, children } => content.push(RenderPaintLayerContentNode::Alpha {
            alpha,
            children: build_composition_layer_content(children),
        }),
        RenderNode::Primitive(_) => append_own_render_node(content, node),
    }
}

fn append_own_render_node(content: &mut Vec<RenderPaintLayerContentNode>, node: RenderNode) {
    if let Some(RenderPaintLayerContentNode::Own(run)) = content.last_mut() {
        Arc::make_mut(&mut run.nodes).push(node);
    } else {
        content.push(RenderPaintLayerContentNode::Own(RenderPaintRun::from_node(
            node,
        )));
    }
}

fn render_nodes_contain_paint_layer(nodes: &[RenderNode]) -> bool {
    nodes.iter().any(|node| match node {
        RenderNode::PaintLayer(_) => true,
        RenderNode::ShadowPass { children }
        | RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => render_nodes_contain_paint_layer(children),
        RenderNode::Primitive(_) => false,
    })
}

fn assign_layer_run_metadata(
    content: &mut [RenderPaintLayerContentNode],
    bounds: Rect,
    next_slot: &mut u32,
) {
    content.iter_mut().for_each(|node| match node {
        RenderPaintLayerContentNode::Own(run) => {
            run.slot = *next_slot;
            *next_slot = (*next_slot).saturating_add(1);
            let node_metrics = render_nodes_metrics(&run.nodes);
            let visual_bounds =
                render_nodes_visual_bounds(&run.nodes, Affine2::identity()).unwrap_or(bounds);
            run.bounds = visual_bounds;
            run.metrics = RenderPaintLayerMetrics {
                own_node_count: node_metrics.node_count.min(u32::MAX as usize) as u32,
                own_primitive_count: node_metrics.primitive_count.min(u32::MAX as usize) as u32,
                own_primitive_cost: node_metrics.primitive_cost,
                payload_pixels: paint_layer_payload_pixels(run.bounds),
                visual_bounds,
                opaque: false,
            };
        }
        RenderPaintLayerContentNode::Child(_) => {}
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            assign_layer_run_metadata(children, bounds, next_slot)
        }
    });
}

fn collect_layer_own_payload_nodes(
    content: &[RenderPaintLayerContentNode],
    nodes: &mut Vec<RenderNode>,
) {
    content.iter().for_each(|node| match node {
        RenderPaintLayerContentNode::Own(run) => nodes.extend(run.nodes.iter().cloned()),
        RenderPaintLayerContentNode::Child(_) => {}
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            collect_layer_own_payload_nodes(children, nodes)
        }
    });
}

#[cfg(test)]
fn collect_layer_own_runs<'a>(
    content: &'a [RenderPaintLayerContentNode],
    runs: &mut Vec<&'a RenderPaintRun>,
) {
    content.iter().for_each(|node| match node {
        RenderPaintLayerContentNode::Own(run) => runs.push(run),
        RenderPaintLayerContentNode::Child(_) => {}
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            collect_layer_own_runs(children, runs)
        }
    });
}

#[cfg(test)]
fn collect_layer_descendants<'a>(
    content: &'a [RenderPaintLayerContentNode],
    layers: &mut Vec<&'a RenderPaintLayer>,
) {
    content.iter().for_each(|node| match node {
        RenderPaintLayerContentNode::Own(_) => {}
        RenderPaintLayerContentNode::Child(layer) => {
            layers.push(layer);
            collect_layer_descendants(&layer.content.nodes, layers);
        }
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            collect_layer_descendants(children, layers)
        }
    });
}

fn layer_content_child_layer_count(content: &[RenderPaintLayerContentNode]) -> usize {
    content
        .iter()
        .map(|node| match node {
            RenderPaintLayerContentNode::Own(_) => 0,
            RenderPaintLayerContentNode::Child(layer) => {
                1 + layer_content_child_layer_count(&layer.content.nodes)
            }
            RenderPaintLayerContentNode::ShadowPass { children }
            | RenderPaintLayerContentNode::Clip { children, .. }
            | RenderPaintLayerContentNode::RelaxedClip { children, .. }
            | RenderPaintLayerContentNode::Transform { children, .. }
            | RenderPaintLayerContentNode::Alpha { children, .. } => {
                layer_content_child_layer_count(children)
            }
        })
        .sum()
}

fn layer_content_own_render_nodes(content: &[RenderPaintLayerContentNode]) -> Vec<RenderNode> {
    content
        .iter()
        .filter_map(|node| match node {
            RenderPaintLayerContentNode::Own(run) => Some(run.nodes.iter().cloned().collect()),
            RenderPaintLayerContentNode::Child(_) => None,
            RenderPaintLayerContentNode::ShadowPass { children } => {
                nonempty_scoped_own_nodes(children, |children| RenderNode::ShadowPass { children })
            }
            RenderPaintLayerContentNode::Clip { clips, children } => {
                nonempty_scoped_own_nodes(children, |children| RenderNode::Clip {
                    clips: clips.clone(),
                    children,
                })
            }
            RenderPaintLayerContentNode::RelaxedClip { clips, children } => {
                nonempty_scoped_own_nodes(children, |children| RenderNode::RelaxedClip {
                    clips: clips.clone(),
                    children,
                })
            }
            RenderPaintLayerContentNode::Transform {
                transform,
                children,
            } => nonempty_scoped_own_nodes(children, |children| RenderNode::Transform {
                transform: *transform,
                children,
            }),
            RenderPaintLayerContentNode::Alpha { alpha, children } => {
                nonempty_scoped_own_nodes(children, |children| RenderNode::Alpha {
                    alpha: *alpha,
                    children,
                })
            }
        })
        .flatten()
        .collect()
}

fn nonempty_scoped_own_nodes(
    children: &[RenderPaintLayerContentNode],
    wrap: impl FnOnce(Vec<RenderNode>) -> RenderNode,
) -> Option<Vec<RenderNode>> {
    let children = layer_content_own_render_nodes(children);
    (!children.is_empty()).then(|| vec![wrap(children)])
}

fn layer_content_to_render_nodes(content: &[RenderPaintLayerContentNode]) -> Vec<RenderNode> {
    content
        .iter()
        .flat_map(|node| match node {
            RenderPaintLayerContentNode::Own(run) => run.nodes.iter().cloned().collect(),
            RenderPaintLayerContentNode::Child(layer) => {
                vec![RenderNode::PaintLayer(layer.clone())]
            }
            RenderPaintLayerContentNode::ShadowPass { children } => {
                vec![RenderNode::ShadowPass {
                    children: layer_content_to_render_nodes(children),
                }]
            }
            RenderPaintLayerContentNode::Clip { clips, children } => vec![RenderNode::Clip {
                clips: clips.clone(),
                children: layer_content_to_render_nodes(children),
            }],
            RenderPaintLayerContentNode::RelaxedClip { clips, children } => {
                vec![RenderNode::RelaxedClip {
                    clips: clips.clone(),
                    children: layer_content_to_render_nodes(children),
                }]
            }
            RenderPaintLayerContentNode::Transform {
                transform,
                children,
            } => vec![RenderNode::Transform {
                transform: *transform,
                children: layer_content_to_render_nodes(children),
            }],
            RenderPaintLayerContentNode::Alpha { alpha, children } => vec![RenderNode::Alpha {
                alpha: *alpha,
                children: layer_content_to_render_nodes(children),
            }],
        })
        .collect()
}

pub(crate) fn paint_layer_own_content_visual_bounds(nodes: &[RenderNode]) -> Option<Rect> {
    render_nodes_visual_bounds(nodes, Affine2::identity())
}

pub(crate) fn paint_layer_bounds_from_visual_bounds(
    visual_bounds: Option<Rect>,
    fallback: Rect,
) -> Rect {
    visual_bounds
        .map(|bounds| union_rect(fallback, bounds))
        .unwrap_or(fallback)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct RenderNodeMetrics {
    node_count: usize,
    primitive_count: usize,
    primitive_cost: u64,
}

fn render_nodes_metrics(nodes: &[RenderNode]) -> RenderNodeMetrics {
    nodes.iter().map(render_node_metrics).fold(
        RenderNodeMetrics::default(),
        |mut total, metrics| {
            total.node_count = total.node_count.saturating_add(metrics.node_count);
            total.primitive_count = total
                .primitive_count
                .saturating_add(metrics.primitive_count);
            total.primitive_cost = total.primitive_cost.saturating_add(metrics.primitive_cost);
            total
        },
    )
}

fn render_node_metrics(node: &RenderNode) -> RenderNodeMetrics {
    match node {
        RenderNode::ShadowPass { children } => {
            let mut metrics = render_nodes_metrics(children);
            metrics.node_count = metrics.node_count.saturating_add(1);
            metrics.primitive_cost = metrics.primitive_cost.saturating_add(12);
            metrics
        }
        RenderNode::Clip { children, .. } | RenderNode::RelaxedClip { children, .. } => {
            let mut metrics = render_nodes_metrics(children);
            metrics.node_count = metrics.node_count.saturating_add(1);
            metrics.primitive_cost = metrics.primitive_cost.saturating_add(3);
            metrics
        }
        RenderNode::Transform { children, .. } | RenderNode::Alpha { children, .. } => {
            let mut metrics = render_nodes_metrics(children);
            metrics.node_count = metrics.node_count.saturating_add(1);
            metrics.primitive_cost = metrics.primitive_cost.saturating_add(2);
            metrics
        }
        RenderNode::PaintLayer(_) => RenderNodeMetrics {
            node_count: 1,
            primitive_count: 0,
            primitive_cost: 0,
        },
        RenderNode::Primitive(primitive) => RenderNodeMetrics {
            node_count: 1,
            primitive_count: 1,
            primitive_cost: render_primitive_cost(primitive),
        },
    }
}

fn render_primitive_cost(primitive: &DrawPrimitive) -> u64 {
    let cost = match primitive {
        DrawPrimitive::Rect(..) => 1,
        DrawPrimitive::RoundedRect(..) => 2,
        DrawPrimitive::Border(.., style)
        | DrawPrimitive::BorderCorners(.., style)
        | DrawPrimitive::BorderEdges(.., style) => match style {
            BorderStyle::Solid => 4,
            BorderStyle::Dashed | BorderStyle::Dotted => 16,
        },
        DrawPrimitive::Shadow(..) | DrawPrimitive::InsetShadow(..) => 80,
        DrawPrimitive::TextWithFont(_, _, text, ..) => 32u64.saturating_add(text.len() as u64 / 2),

        DrawPrimitive::Image(..) => 20,
        DrawPrimitive::Video(..) => 24,
        DrawPrimitive::ImageLoading(..) | DrawPrimitive::ImageFailed(..) => 4,
    };
    cost + if primitive.has_gradient() { 11 } else { 0 }
}

fn paint_layer_payload_pixels(bounds: Rect) -> u64 {
    if bounds.width <= 0.0 || bounds.height <= 0.0 {
        return 0;
    }

    let width = bounds.width.ceil() as u64;
    let height = bounds.height.ceil() as u64;
    width.saturating_mul(height)
}

fn render_nodes_visual_bounds(nodes: &[RenderNode], transform: Affine2) -> Option<Rect> {
    nodes
        .iter()
        .filter_map(|node| render_node_visual_bounds(node, transform))
        .reduce(union_rect)
}

pub(crate) fn render_node_visual_bounds(node: &RenderNode, transform: Affine2) -> Option<Rect> {
    match node {
        RenderNode::ShadowPass { children } | RenderNode::Alpha { children, .. } => {
            render_nodes_visual_bounds(children, transform)
        }
        RenderNode::Clip { clips, children } | RenderNode::RelaxedClip { clips, children } => {
            render_clipped_nodes_visual_bounds(clips, children, transform)
        }
        RenderNode::Transform {
            transform: child_transform,
            children,
        } => render_nodes_visual_bounds(children, transform.then(*child_transform)),
        RenderNode::PaintLayer(_) => None,
        RenderNode::Primitive(primitive) => {
            Some(transform.map_rect_aabb(draw_primitive_visual_bounds(primitive)))
        }
    }
}

fn render_clipped_nodes_visual_bounds(
    clips: &[ClipShape],
    children: &[RenderNode],
    transform: Affine2,
) -> Option<Rect> {
    if clips.is_empty() {
        return render_nodes_visual_bounds(children, transform);
    }

    let clip_bounds = clips
        .iter()
        .map(|clip| transform.map_rect_aabb(clip.rect))
        .reduce(union_rect)?;

    children
        .iter()
        .filter_map(|child| match child {
            RenderNode::ShadowPass { children } => render_nodes_visual_bounds(children, transform),
            _ => render_node_visual_bounds(child, transform)
                .and_then(|bounds| bounds.intersect(clip_bounds)),
        })
        .reduce(union_rect)
}

pub(crate) fn draw_primitive_visual_bounds(primitive: &DrawPrimitive) -> Rect {
    match primitive {
        DrawPrimitive::Rect(x, y, w, h, _)
        | DrawPrimitive::RoundedRect(x, y, w, h, _, _)
        | DrawPrimitive::Image(x, y, w, h, _, _, _)
        | DrawPrimitive::Video(x, y, w, h, _, _)
        | DrawPrimitive::ImageLoading(x, y, w, h)
        | DrawPrimitive::ImageFailed(x, y, w, h) => Rect {
            x: *x,
            y: *y,
            width: *w,
            height: *h,
        },
        DrawPrimitive::Border(x, y, w, h, _, width, _, _) => {
            outset_rect(rect_from_xywh(*x, *y, *w, *h), *width / 2.0)
        }
        DrawPrimitive::BorderCorners(x, y, w, h, _, _, _, _, width, _, _) => {
            outset_rect(rect_from_xywh(*x, *y, *w, *h), *width / 2.0)
        }
        DrawPrimitive::BorderEdges(x, y, w, h, _, top, right, bottom, left, _, _) => {
            let outset = top.max(*right).max(*bottom).max(*left) / 2.0;
            outset_rect(rect_from_xywh(*x, *y, *w, *h), outset)
        }
        DrawPrimitive::Shadow(x, y, w, h, offset_x, offset_y, blur, size, _, _) => {
            let pad = blur.abs() * 2.0 + size.abs();
            Rect {
                x: *x + *offset_x - pad,
                y: *y + *offset_y - pad,
                width: *w + pad * 2.0,
                height: *h + pad * 2.0,
            }
        }
        DrawPrimitive::InsetShadow(x, y, w, h, _, _, _, _, _, _) => rect_from_xywh(*x, *y, *w, *h),
        DrawPrimitive::TextWithFont(x, y, text, font_size, _, family, weight, italic) => {
            const TEXT_VISUAL_BOUNDS_OUTSET: f32 = 1.0;
            let metrics =
                measure_text_visual_metrics(family, *weight, *italic, *font_size, text.as_str());
            Rect {
                x: *x - metrics.left_overhang - TEXT_VISUAL_BOUNDS_OUTSET,
                y: *y + metrics.visual_top - TEXT_VISUAL_BOUNDS_OUTSET,
                width: metrics.visual_width + TEXT_VISUAL_BOUNDS_OUTSET * 2.0,
                height: (metrics.visual_bottom - metrics.visual_top).max(0.0)
                    + TEXT_VISUAL_BOUNDS_OUTSET * 2.0,
            }
        }
    }
}

fn rect_from_xywh(x: f32, y: f32, width: f32, height: f32) -> Rect {
    Rect {
        x,
        y,
        width,
        height,
    }
}

fn outset_rect(rect: Rect, outset: f32) -> Rect {
    let outset = outset.max(0.0);
    Rect {
        x: rect.x - outset,
        y: rect.y - outset,
        width: rect.width + outset * 2.0,
        height: rect.height + outset * 2.0,
    }
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let min_x = a.x.min(b.x);
    let min_y = a.y.min(b.y);
    let max_x = (a.x + a.width).max(b.x + b.width);
    let max_y = (a.y + a.height).max(b.y + b.height);
    Rect {
        x: min_x,
        y: min_y,
        width: max_x - min_x,
        height: max_y - min_y,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaintLayerPlacement {
    Fixed,
    ScrollMoving,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PaintLayerPolicy {
    Cacheable,
    DirectOnly,
}

impl PaintLayerPolicy {
    pub(crate) fn allows_payload_cache(self) -> bool {
        self == Self::Cacheable
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PaintLayerReason {
    Root,
    Nearby,
    ScrollContent,
    Animation,
    SliderValue,
    DirectMedia,
}

#[derive(Clone, Debug, PartialEq)]
pub enum DrawPrimitive {
    Rect(f32, f32, f32, f32, RenderColor),
    RoundedRect(f32, f32, f32, f32, f32, RenderColor),
    Border(f32, f32, f32, f32, f32, f32, RenderColor, BorderStyle),
    BorderCorners(
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
        RenderColor,
        BorderStyle,
    ),
    BorderEdges(
        f32,
        f32,
        f32,
        f32,
        [f32; 4],
        f32,
        f32,
        f32,
        f32,
        RenderColor,
        BorderStyle,
    ),
    Shadow(
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
        [f32; 4],
        RenderColor,
    ),
    InsetShadow(
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
        f32,
        [f32; 4],
        RenderColor,
    ),
    TextWithFont(f32, f32, String, f32, RenderColor, String, u16, bool),
    Image(f32, f32, f32, f32, String, ImageFit, Option<RenderColor>),
    Video(f32, f32, f32, f32, String, ImageFit),
    ImageLoading(f32, f32, f32, f32),
    ImageFailed(f32, f32, f32, f32),
}

impl DrawPrimitive {
    pub fn color(&self) -> Option<&RenderColor> {
        match self {
            Self::Rect(_, _, _, _, c)
            | Self::RoundedRect(_, _, _, _, _, c)
            | Self::Border(_, _, _, _, _, _, c, _)
            | Self::BorderCorners(_, _, _, _, _, _, _, _, _, c, _)
            | Self::BorderEdges(_, _, _, _, _, _, _, _, _, c, _)
            | Self::Shadow(_, _, _, _, _, _, _, _, _, c)
            | Self::InsetShadow(_, _, _, _, _, _, _, _, _, c)
            | Self::TextWithFont(_, _, _, _, c, _, _, _) => Some(c),
            Self::Image(_, _, _, _, _, _, c) => c.as_ref(),
            _ => None,
        }
    }

    pub fn has_gradient(&self) -> bool {
        matches!(self.color(), Some(RenderColor::Linear { .. }))
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PaintLayerHashFloat {
    Exact,
}

impl PaintLayerHashFloat {
    pub(crate) fn hash_f32<H: Hasher>(self, hasher: &mut H, value: f32) {
        match self {
            Self::Exact => value.to_bits().hash(hasher),
        }
    }
}

pub(crate) fn hash_paint_layer_render_nodes<H: Hasher>(
    hasher: &mut H,
    nodes: &[RenderNode],
    float: PaintLayerHashFloat,
    payload_bounds: Option<Rect>,
) {
    let mut hashed_nodes = 0usize;
    for node in nodes {
        if hash_paint_layer_render_node(hasher, node, float, payload_bounds) {
            hashed_nodes += 1;
        }
    }
    hashed_nodes.hash(hasher);
}

pub(crate) fn hash_paint_layer_render_node<H: Hasher>(
    hasher: &mut H,
    node: &RenderNode,
    float: PaintLayerHashFloat,
    payload_bounds: Option<Rect>,
) -> bool {
    if payload_bounds
        .and_then(|bounds| {
            render_node_visual_bounds(node, Affine2::identity())
                .and_then(|node_bounds| node_bounds.intersect(bounds))
        })
        .is_none()
        && payload_bounds.is_some()
    {
        return false;
    }

    match node {
        RenderNode::ShadowPass { children } => {
            if !paint_layer_render_nodes_intersect_payload(children, payload_bounds) {
                return false;
            }
            0u8.hash(hasher);
            hash_paint_layer_render_nodes(hasher, children, float, payload_bounds);
            true
        }
        RenderNode::Clip { clips, children } => {
            let (clips, child_payload_bounds) =
                paint_layer_hash_clip_scope(clips, children, payload_bounds);
            if payload_bounds.is_some() && child_payload_bounds.is_none() {
                return false;
            }
            if !paint_layer_render_nodes_intersect_payload(children, child_payload_bounds) {
                return false;
            }
            1u8.hash(hasher);
            hash_paint_layer_clip_shapes(hasher, &clips, float);
            hash_paint_layer_render_nodes(hasher, children, float, child_payload_bounds);
            true
        }
        RenderNode::RelaxedClip { clips, children } => {
            let (clips, child_payload_bounds) =
                paint_layer_hash_clip_scope(clips, children, payload_bounds);
            if payload_bounds.is_some() && child_payload_bounds.is_none() {
                return false;
            }
            if !paint_layer_render_nodes_intersect_payload(children, child_payload_bounds) {
                return false;
            }
            2u8.hash(hasher);
            hash_paint_layer_clip_shapes(hasher, &clips, float);
            hash_paint_layer_render_nodes(hasher, children, float, child_payload_bounds);
            true
        }
        RenderNode::Transform {
            transform,
            children,
        } => {
            let child_payload_bounds = payload_bounds.and_then(|bounds| {
                transform
                    .inverse()
                    .map(|inverse| inverse.map_rect_aabb(bounds))
            });
            if !paint_layer_render_nodes_intersect_payload(children, child_payload_bounds) {
                return false;
            }
            3u8.hash(hasher);
            hash_paint_layer_affine2(hasher, *transform, float);
            hash_paint_layer_render_nodes(hasher, children, float, child_payload_bounds);
            true
        }
        RenderNode::Alpha { alpha, children } => {
            if !paint_layer_render_nodes_intersect_payload(children, payload_bounds) {
                return false;
            }
            4u8.hash(hasher);
            float.hash_f32(hasher, *alpha);
            hash_paint_layer_render_nodes(hasher, children, float, payload_bounds);
            true
        }
        RenderNode::PaintLayer(layer) => {
            if payload_bounds
                .and_then(|bounds| bounds.intersect(layer.bounds))
                .is_none()
                && payload_bounds.is_some()
            {
                return false;
            }
            5u8.hash(hasher);
            hash_paint_layer_metadata(hasher, layer);
            hash_paint_layer_rect(hasher, layer.bounds, float);
            true
        }
        RenderNode::Primitive(primitive) => {
            if payload_bounds
                .and_then(|bounds| bounds.intersect(draw_primitive_visual_bounds(primitive)))
                .is_none()
                && payload_bounds.is_some()
            {
                return false;
            }
            6u8.hash(hasher);
            hash_paint_layer_draw_primitive(hasher, primitive, float);
            true
        }
    }
}

fn paint_layer_render_nodes_intersect_payload(
    nodes: &[RenderNode],
    payload_bounds: Option<Rect>,
) -> bool {
    let Some(payload_bounds) = payload_bounds else {
        return true;
    };
    render_nodes_visual_bounds(nodes, Affine2::identity())
        .and_then(|bounds| bounds.intersect(payload_bounds))
        .is_some()
}

fn paint_layer_hash_clip_scope(
    clips: &[ClipShape],
    children: &[RenderNode],
    payload_bounds: Option<Rect>,
) -> (Vec<ClipShape>, Option<Rect>) {
    let Some(payload_bounds) = payload_bounds else {
        return (clips.to_vec(), None);
    };

    let has_shadow_escape = children
        .iter()
        .any(|child| matches!(child, RenderNode::ShadowPass { .. }));
    let child_payload_bounds = if has_shadow_escape {
        Some(payload_bounds)
    } else {
        clips
            .iter()
            .try_fold(payload_bounds, |bounds, clip| bounds.intersect(clip.rect))
    };
    let clips = clips
        .iter()
        .filter_map(|clip| {
            clip.rect.intersect(payload_bounds).map(|rect| ClipShape {
                rect,
                radii: clip.radii,
            })
        })
        .collect();

    (clips, child_payload_bounds)
}

pub(crate) fn hash_paint_layer_metadata<H: Hasher>(hasher: &mut H, layer: &RenderPaintLayer) {
    layer.id.hash(hasher);
    layer.placement.hash(hasher);
    layer.policy.hash(hasher);
    layer.content_generation.hash(hasher);
}

pub(crate) fn hash_paint_layer_clip_shapes<H: Hasher>(
    hasher: &mut H,
    clips: &[ClipShape],
    float: PaintLayerHashFloat,
) {
    clips.len().hash(hasher);
    for clip in clips {
        hash_paint_layer_rect(hasher, clip.rect, float);
        match clip.radii {
            Some(radii) => {
                true.hash(hasher);
                hash_paint_layer_f32s(hasher, &[radii.tl, radii.tr, radii.br, radii.bl], float);
            }
            None => false.hash(hasher),
        }
    }
}

pub(crate) fn hash_paint_layer_rect<H: Hasher>(
    hasher: &mut H,
    rect: Rect,
    float: PaintLayerHashFloat,
) {
    hash_paint_layer_f32s(hasher, &[rect.x, rect.y, rect.width, rect.height], float);
}

pub(crate) fn hash_paint_layer_affine2<H: Hasher>(
    hasher: &mut H,
    transform: Affine2,
    float: PaintLayerHashFloat,
) {
    hash_paint_layer_f32s(
        hasher,
        &[
            transform.xx,
            transform.yx,
            transform.xy,
            transform.yy,
            transform.tx,
            transform.ty,
        ],
        float,
    );
}

pub(crate) fn hash_paint_layer_draw_primitive<H: Hasher>(
    hasher: &mut H,
    primitive: &DrawPrimitive,
    float: PaintLayerHashFloat,
) {
    match primitive {
        DrawPrimitive::Rect(x, y, w, h, color) => {
            0u8.hash(hasher);
            hash_paint_layer_f32s(hasher, &[*x, *y, *w, *h], float);
            color.hash(hasher);
        }
        DrawPrimitive::RoundedRect(x, y, w, h, radius, color) => {
            1u8.hash(hasher);
            hash_paint_layer_f32s(hasher, &[*x, *y, *w, *h, *radius], float);
            color.hash(hasher);
        }
        DrawPrimitive::Border(x, y, w, h, radius, width, color, style) => {
            2u8.hash(hasher);
            hash_paint_layer_f32s(hasher, &[*x, *y, *w, *h, *radius, *width], float);
            color.hash(hasher);
            hash_paint_layer_border_style(hasher, *style);
        }
        DrawPrimitive::BorderCorners(x, y, w, h, tl, tr, br, bl, width, color, style) => {
            3u8.hash(hasher);
            hash_paint_layer_f32s(hasher, &[*x, *y, *w, *h, *tl, *tr, *br, *bl, *width], float);
            color.hash(hasher);
            hash_paint_layer_border_style(hasher, *style);
        }
        DrawPrimitive::BorderEdges(x, y, w, h, radius, top, right, bottom, left, color, style) => {
            4u8.hash(hasher);
            hash_paint_layer_f32s(
                hasher,
                &[*x, *y, *w, *h, *top, *right, *bottom, *left],
                float,
            );
            hash_paint_layer_f32s(hasher, radius, float);
            color.hash(hasher);
            hash_paint_layer_border_style(hasher, *style);
        }
        DrawPrimitive::Shadow(x, y, w, h, offset_x, offset_y, blur, size, radius, color) => {
            5u8.hash(hasher);
            hash_paint_layer_f32s(
                hasher,
                &[*x, *y, *w, *h, *offset_x, *offset_y, *blur, *size],
                float,
            );
            hash_paint_layer_f32s(hasher, radius, float);
            color.hash(hasher);
        }
        DrawPrimitive::InsetShadow(x, y, w, h, offset_x, offset_y, blur, size, radius, color) => {
            6u8.hash(hasher);
            hash_paint_layer_f32s(
                hasher,
                &[*x, *y, *w, *h, *offset_x, *offset_y, *blur, *size],
                float,
            );
            hash_paint_layer_f32s(hasher, radius, float);
            color.hash(hasher);
        }
        DrawPrimitive::TextWithFont(x, y, text, font_size, fill, family, weight, italic) => {
            7u8.hash(hasher);
            hash_paint_layer_f32s(hasher, &[*x, *y, *font_size], float);
            text.hash(hasher);
            fill.hash(hasher);
            family.hash(hasher);
            weight.hash(hasher);
            italic.hash(hasher);
        }

        DrawPrimitive::Image(x, y, w, h, image_id, fit, tint) => {
            9u8.hash(hasher);
            hash_paint_layer_f32s(hasher, &[*x, *y, *w, *h], float);
            image_id.hash(hasher);
            hash_paint_layer_image_fit(hasher, *fit);
            tint.hash(hasher);
        }
        DrawPrimitive::Video(x, y, w, h, target, fit) => {
            10u8.hash(hasher);
            hash_paint_layer_f32s(hasher, &[*x, *y, *w, *h], float);
            target.hash(hasher);
            hash_paint_layer_image_fit(hasher, *fit);
        }
        DrawPrimitive::ImageLoading(x, y, w, h) => {
            11u8.hash(hasher);
            hash_paint_layer_f32s(hasher, &[*x, *y, *w, *h], float);
        }
        DrawPrimitive::ImageFailed(x, y, w, h) => {
            12u8.hash(hasher);
            hash_paint_layer_f32s(hasher, &[*x, *y, *w, *h], float);
        }
    }
}

pub(crate) fn hash_paint_layer_f32s<H: Hasher>(
    hasher: &mut H,
    values: &[f32],
    float: PaintLayerHashFloat,
) {
    for value in values {
        float.hash_f32(hasher, *value);
    }
}

fn hash_paint_layer_border_style<H: Hasher>(hasher: &mut H, style: BorderStyle) {
    match style {
        BorderStyle::Solid => 0u8,
        BorderStyle::Dashed => 1u8,
        BorderStyle::Dotted => 2u8,
    }
    .hash(hasher);
}

fn hash_paint_layer_image_fit<H: Hasher>(hasher: &mut H, fit: ImageFit) {
    match fit {
        ImageFit::Contain => 0u8,
        ImageFit::Cover => 1u8,
        ImageFit::Repeat => 2u8,
        ImageFit::RepeatX => 3u8,
        ImageFit::RepeatY => 4u8,
    }
    .hash(hasher);
}

#[cfg(test)]
mod tests {
    use super::{
        DrawPrimitive, PaintLayerPlacement, PaintLayerPolicy, PaintLayerReason, RenderNode,
        RenderPaintLayer, RenderScene,
    };
    use crate::tree::attrs::ImageFit;
    use crate::tree::geometry::Rect;
    use std::collections::HashSet;

    // One Raven patch: scene comparison for the presenter.
    #[test]
    fn render_nodes_differ_finds_a_moved_layer_and_ignores_a_rebuilt_copy() {
        use super::render_nodes_differ;
        use crate::render_color::RenderColor;
        use crate::tree::transform::Affine2;
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: 64.0,
            height: 32.0,
        };
        let rect = |w| RenderNode::Primitive(DrawPrimitive::Rect(0.0, 0.0, w, 8.0, RenderColor::from(0xff0000ff)));
        let content = RenderPaintLayer::from_children(
            2,
            bounds,
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::Cacheable,
            PaintLayerReason::ScrollContent,
            7,
            vec![rect(10.0), rect(20.0)],
        );
        let scene = |dy: f32, layer: &RenderPaintLayer| {
            vec![rect(64.0), RenderNode::Transform {
                transform: Affine2::translation(0.0, dy),
                children: vec![RenderNode::PaintLayer(layer.clone())],
            }]
        };
        let rebuilt = RenderPaintLayer::from_children(
            2,
            bounds,
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::Cacheable,
            PaintLayerReason::ScrollContent,
            7,
            vec![rect(10.0), rect(20.0)],
        );
        assert!(!render_nodes_differ(&scene(-5.0, &content), &scene(-5.0, &content)));
        assert!(!render_nodes_differ(&scene(-5.0, &content), &scene(-5.0, &rebuilt)));
        assert!(render_nodes_differ(&scene(-5.0, &content), &scene(-6.0, &content)));
        let changed = RenderPaintLayer::from_children(
            2,
            bounds,
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::Cacheable,
            PaintLayerReason::ScrollContent,
            7,
            vec![rect(10.0), rect(21.0)],
        );
        assert!(render_nodes_differ(&scene(-5.0, &content), &scene(-5.0, &changed)));
    }

    #[test]
    fn summary_and_video_targets_follow_ordered_layer_content() {
        let bounds = Rect {
            x: 0.0,
            y: 0.0,
            width: 64.0,
            height: 32.0,
        };
        let child = RenderPaintLayer::from_children(
            2,
            bounds,
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::DirectOnly,
            PaintLayerReason::Nearby,
            0,
            vec![RenderNode::Primitive(DrawPrimitive::Video(
                0.0,
                0.0,
                64.0,
                32.0,
                "secondary".to_string(),
                ImageFit::Contain,
            ))],
        );
        let parent = RenderPaintLayer::from_children(
            1,
            bounds,
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::DirectOnly,
            PaintLayerReason::Root,
            0,
            vec![RenderNode::Alpha {
                alpha: 1.0,
                children: vec![
                    RenderNode::Primitive(DrawPrimitive::Video(
                        0.0,
                        0.0,
                        64.0,
                        32.0,
                        "preview".to_string(),
                        ImageFit::Contain,
                    )),
                    RenderNode::PaintLayer(child),
                ],
            }],
        );
        let scene = RenderScene {
            scroll_fling: None,
            fonts: None,
            images: None,
            nodes: vec![RenderNode::PaintLayer(parent)],
        };

        assert_eq!(
            scene.video_target_ids(),
            HashSet::from(["preview".to_string(), "secondary".to_string()])
        );
        let summary = scene.summary();
        assert_eq!(summary.paint_layers, 2);
        assert_eq!(summary.alphas, 1);
        assert_eq!(summary.videos, 2);
        assert!(RenderScene::default().video_target_ids().is_empty());
    }

    #[test]
    fn gradient_hash_includes_middle_stops_order_count_and_angle() {
        use std::hash::Hasher;
        let fingerprint = |colors, angle| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            super::hash_paint_layer_draw_primitive(
                &mut hasher,
                &crate::render_scene::DrawPrimitive::Rect(
                    0.0,
                    0.0,
                    100.0,
                    50.0,
                    crate::render_color::RenderColor::linear(
                        colors,
                        angle,
                        crate::tree::geometry::Rect {
                            x: 0.0,
                            y: 0.0,
                            width: 100.0,
                            height: 50.0,
                        },
                    ),
                ),
                super::PaintLayerHashFloat::Exact,
            );
            hasher.finish()
        };
        let original = fingerprint(vec![1, 2, 3], 0.0);
        for (colors, angle) in [
            (vec![1, 4, 3], 0.0),
            (vec![3, 2, 1], 0.0),
            (vec![1, 2, 2, 3], 0.0),
            (vec![1, 2, 3], 90.0),
        ] {
            assert_ne!(original, fingerprint(colors, angle));
        }
    }
}

#[cfg(test)]
mod renderer_animation_tests {
    use super::*;
    use crate::tree::animation::{AnimationCurve, AnimationRepeat, AnimationSpec};
    use crate::tree::attrs::Attrs;
    use crate::tree::element::Frame;
    use std::time::{Duration, Instant};

    const BOUNDS: Rect = Rect {
        x: 100.0,
        y: 200.0,
        width: 80.0,
        height: 40.0,
    };

    fn frame() -> Frame {
        Frame {
            x: BOUNDS.x,
            y: BOUNDS.y,
            width: BOUNDS.width,
            height: BOUNDS.height,
            ..Frame::default()
        }
    }

    fn spin_spec() -> Arc<AnimationSpec> {
        let at = |deg| Attrs {
            rotate: Some(deg),
            ..Attrs::default()
        };
        Arc::new(AnimationSpec {
            keyframes: vec![at(0.0), at(360.0)],
            duration_ms: 1000.0,
            curve: AnimationCurve::Linear,
            repeat: AnimationRepeat::Loop,
        })
    }

    fn rect_node() -> RenderNode {
        RenderNode::Primitive(DrawPrimitive::Rect(
            BOUNDS.x,
            BOUNDS.y,
            BOUNDS.width,
            BOUNDS.height,
            RenderColor::from(0xff0000ff),
        ))
    }

    /// An animated layer built the way tree/render.rs builds one: its own
    /// Transform node holding the built transform, plus a child layer.
    fn animated_layer(started_at: Instant, child: Option<RenderPaintLayer>) -> RenderPaintLayer {
        let built = Affine2::identity();
        let mut children = vec![rect_node()];
        if let Some(child) = child {
            children.push(RenderNode::PaintLayer(child));
        }
        let mut layer = RenderPaintLayer::from_prepared_children(
            RenderPaintLayerBuildParts {
                id: PaintLayerId::new(7, PaintLayerReason::Animation),
                bounds: BOUNDS,
                placement: PaintLayerPlacement::Fixed,
                policy: PaintLayerPolicy::Cacheable,
                content_generation: 1,
                visual_bounds: None,
            },
            RenderPaintLayerContent::from_composition_nodes(vec![RenderNode::Transform {
                transform: built,
                children,
            }]),
        );
        layer.renderer_animation = Some(Arc::new(LayerRendererAnimation {
            spec: spin_spec(),
            started_at,
            revision: 0,
            frame: frame(),
            base: AnimatedBase::default(),
            current_transform: built,
            current_alpha: 1.0,
            offset: (0.0, 0.0),
            bounds_transform: built,
        }));
        layer
    }

    // One Raven patch: the render thread samples from the six values the
    // element's transform and alpha read, not a clone of its whole attrs.
    #[test]
    fn sampling_from_the_animated_base_matches_sampling_from_the_attrs() {
        use crate::tree::attrs::{Background, Color, Length};
        let attrs = Attrs {
            width: Some(Length::Px(80.0)),
            content: Some("label".into()),
            background: Some(Background::Color(Color::Named("red".into()))),
            move_x: Some(5.0),
            move_y: Some(-3.0),
            layout_rotate: Some(10.0),
            scale: Some(1.25),
            alpha: Some(0.5),
            ..Attrs::default()
        };
        let keyframe = |t: f64| Attrs {
            move_y: Some(20.0 * t),
            rotate: Some(90.0 * t),
            alpha: Some(1.0 - t / 2.0),
            ..Attrs::default()
        };
        let started_at = Instant::now();
        let animation = LayerRendererAnimation {
            spec: Arc::new(AnimationSpec {
                keyframes: vec![keyframe(0.0), keyframe(1.0)],
                duration_ms: 1000.0,
                curve: AnimationCurve::EaseInOut,
                repeat: AnimationRepeat::Loop,
            }),
            started_at,
            revision: 0,
            frame: frame(),
            base: AnimatedBase::of(&attrs),
            current_transform: Affine2::identity(),
            current_alpha: 1.0,
            offset: (3.0, 4.0),
            bounds_transform: Affine2::identity(),
        };
        for ms in [0, 130, 500, 999, 1700] {
            let at = started_at + std::time::Duration::from_millis(ms);
            // What the sample did before: the whole attrs, sample laid over.
            let entry = crate::tree::animation::AnimationRuntimeEntry {
                spec_hash: 0,
                started_at,
            };
            let sample = crate::tree::animation::sample_animation_spec(
                &animation.spec,
                Some(&entry),
                Some(at),
            );
            let mut expected = attrs.clone();
            crate::tree::animation::apply_sample_attrs(&mut expected, &sample.attrs);
            let transform = Affine2::translation(3.0, 4.0).then(
                crate::tree::transform::element_transform(frame(), &expected),
            );
            assert_eq!(
                animation.sample(at),
                (transform, expected.alpha.unwrap() as f32),
                "{ms} ms"
            );
        }
    }

    /// `cargo test --release --lib renderer_animation_sample_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn renderer_animation_sample_timing() {
        use crate::tree::attrs::{Background, BoxShadow, Color, Font, Length};
        // A card's attrs as the tree holds them: text, font, colours, shadow.
        let attrs = Attrs {
            width: Some(Length::Px(320.0)),
            height: Some(Length::Px(180.0)),
            content: Some("Living room lamp".into()),
            font: Some(Font::String("Inter".into())),
            font_size: Some(18.0),
            background: Some(Background::Color(Color::Rgba {
                r: 30,
                g: 30,
                b: 40,
                a: 255,
            })),
            box_shadows: Some(vec![BoxShadow {
                offset_x: 0.0,
                offset_y: 4.0,
                blur: 12.0,
                size: 0.0,
                color: Color::Named("black".into()),
                inset: false,
            }]),
            on_click: Some(true),
            ..Attrs::default()
        };
        let started_at = Instant::now();
        let animation = LayerRendererAnimation {
            spec: spin_spec(),
            started_at,
            revision: 0,
            frame: frame(),
            base: AnimatedBase::of(&attrs),
            current_transform: Affine2::identity(),
            current_alpha: 1.0,
            offset: (0.0, 0.0),
            bounds_transform: Affine2::identity(),
        };
        let runs = 50_000;
        let time = |f: &dyn Fn(Instant) -> (Affine2, f32)| {
            let begun = Instant::now();
            for k in 0..runs {
                std::hint::black_box(f(started_at + std::time::Duration::from_micros(k * 16)));
            }
            begun.elapsed().as_nanos() as f64 / runs as f64
        };
        // Before: sample, then lay it over a clone of the whole attrs.
        let cloned = time(&|at| {
            let entry = crate::tree::animation::AnimationRuntimeEntry {
                spec_hash: 0,
                started_at,
            };
            let sample = crate::tree::animation::sample_animation_spec(
                &animation.spec,
                Some(&entry),
                Some(at),
            );
            let mut attrs = std::hint::black_box(&attrs).clone();
            crate::tree::animation::apply_sample_attrs(&mut attrs, &sample.attrs);
            (
                crate::tree::transform::element_transform(animation.frame, &attrs),
                attrs.alpha.unwrap_or(1.0) as f32,
            )
        });
        let base = time(&|at| animation.sample(at));
        // Copy-on-write of the layer's animation (Arc::make_mut while shared).
        let copies = 50_000;
        let begun = Instant::now();
        for _ in 0..copies {
            std::hint::black_box((std::hint::black_box(&attrs).clone(), animation.clone()));
        }
        let copy_before = begun.elapsed().as_nanos() as f64 / copies as f64;
        let begun = Instant::now();
        for _ in 0..copies {
            std::hint::black_box(std::hint::black_box(&animation).clone());
        }
        let copy_after = begun.elapsed().as_nanos() as f64 / copies as f64;
        eprintln!(
            "renderer animation sample: {cloned:.0} ns with the attrs cloned, {base:.0} ns from the base; \
             copy of the animation: {copy_before:.0} ns with attrs, {copy_after:.0} ns with the base"
        );
    }

    fn child_layer(generation: u64) -> RenderPaintLayer {
        RenderPaintLayer::from_children(
            9,
            BOUNDS,
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::Cacheable,
            PaintLayerReason::Nearby,
            generation,
            vec![rect_node()],
        )
    }

    fn own_transform(layer: &RenderPaintLayer) -> Affine2 {
        match &layer.content.nodes[0] {
            RenderPaintLayerContentNode::Transform { transform, .. } => *transform,
            other => panic!("expected the layer's Transform node, got {other:?}"),
        }
    }

    #[test]
    fn sample_matches_the_tree_transform() {
        let started_at = Instant::now();
        let layer = animated_layer(started_at, None);
        let animation = layer.renderer_animation.as_deref().unwrap();
        let (transform, alpha) = animation.sample(started_at + Duration::from_millis(250));
        let expected = crate::tree::transform::element_transform(
            frame(),
            &Attrs {
                rotate: Some(90.0),
                ..Attrs::default()
            },
        );
        for (a, b) in [
            (transform.xx, expected.xx),
            (transform.yx, expected.yx),
            (transform.xy, expected.xy),
            (transform.yy, expected.yy),
            (transform.tx, expected.tx),
            (transform.ty, expected.ty),
        ] {
            assert!((a - b).abs() < 1e-3, "{transform:?} != {expected:?}");
        }
        assert_eq!(alpha, 1.0);
    }

    #[test]
    fn advance_patches_own_nodes_only_and_reports_damage() {
        let started_at = Instant::now();
        let child = child_layer(1);
        let mut scene = RenderScene {
            scroll_fling: None,
            nodes: vec![RenderNode::PaintLayer(animated_layer(
                started_at,
                Some(child.clone()),
            ))],
            ..RenderScene::default()
        };
        let advance = scene.advance_renderer_animations(started_at + Duration::from_millis(250));
        assert!(advance.found);
        let RenderNode::PaintLayer(layer) = &scene.nodes[0] else {
            panic!("scene root should stay a layer");
        };
        assert!(!own_transform(layer).is_identity());
        assert_eq!(
            layer
                .renderer_animation
                .as_deref()
                .unwrap()
                .current_transform,
            own_transform(layer)
        );
        // The child layer is untouched, and the damage covers the old and
        // the rotated position.
        let RenderPaintLayerContentNode::Transform { children, .. } = &layer.content.nodes[0]
        else {
            unreachable!()
        };
        assert!(children.iter().any(|node| matches!(
            node,
            RenderPaintLayerContentNode::Child(c) if c == &child
        )));
        let damage = advance.damage.expect("damage");
        assert!(damage.x <= BOUNDS.x && damage.y <= BOUNDS.y);
        assert!(damage.x + damage.width >= BOUNDS.x + BOUNDS.width);
    }

    #[test]
    fn scene_without_renderer_animations_finds_none() {
        let mut scene = RenderScene {
            scroll_fling: None,
            nodes: vec![RenderNode::PaintLayer(child_layer(1))],
            ..RenderScene::default()
        };
        let advance = scene.advance_renderer_animations(Instant::now());
        assert!(!advance.found);
        assert!(advance.damage.is_none());
    }

    #[test]
    fn snapshot_key_ignores_own_motion_but_not_content() {
        let started_at = Instant::now();
        let layer = animated_layer(started_at, Some(child_layer(1)));
        let key = renderer_animation_snapshot_key(&layer);

        let mut scene = RenderScene {
            scroll_fling: None,
            nodes: vec![RenderNode::PaintLayer(layer.clone())],
            ..RenderScene::default()
        };
        scene.advance_renderer_animations(started_at + Duration::from_millis(250));
        let RenderNode::PaintLayer(moved) = &scene.nodes[0] else {
            unreachable!()
        };
        assert_eq!(renderer_animation_snapshot_key(moved), key);

        let changed = animated_layer(started_at, Some(child_layer(2)));
        // A rebuilt child is a new payload, so the key changes.
        assert_ne!(renderer_animation_snapshot_key(&changed), key);
    }

    #[test]
    fn snapshot_eligibility() {
        let started_at = Instant::now();
        assert!(renderer_animation_snapshot_eligible(&animated_layer(
            started_at, None
        )));

        let mut nested = animated_layer(started_at, Some(animated_layer(started_at, None)));
        assert!(!renderer_animation_snapshot_eligible(&nested));

        let mut fading = animated_layer(started_at, None);
        let animation = Arc::make_mut(fading.renderer_animation.as_mut().unwrap());
        Arc::make_mut(&mut animation.spec).keyframes[0].alpha = Some(0.5);
        assert!(!renderer_animation_snapshot_eligible(&fading));

        nested.renderer_animation = None;
        assert!(!renderer_animation_snapshot_eligible(&nested));
    }

    fn static_layer(id: u64) -> RenderNode {
        RenderNode::PaintLayer(RenderPaintLayer::from_children(
            id,
            BOUNDS,
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::Cacheable,
            PaintLayerReason::ScrollContent,
            1,
            vec![rect_node(), rect_node()],
        ))
    }

    /// A page of `statics` static layers with `animated` nested inside a
    /// clip and a static parent layer, as a dashboard card would hold it.
    fn page(statics: u64, animated: RenderPaintLayer) -> RenderScene {
        let mut children: Vec<RenderNode> = (0..statics).map(|id| static_layer(100 + id)).collect();
        let card = RenderPaintLayer::from_children(
            50,
            BOUNDS,
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::Cacheable,
            PaintLayerReason::ScrollContent,
            1,
            vec![],
        )
        .with_children(vec![
            rect_node(),
            RenderNode::Clip {
                clips: vec![ClipShape {
                    rect: BOUNDS,
                    radii: None,
                }],
                children: vec![RenderNode::PaintLayer(animated)],
            },
        ]);
        children.insert(statics as usize / 2, RenderNode::PaintLayer(card));
        let root = RenderPaintLayer::from_children(
            1,
            BOUNDS,
            PaintLayerPlacement::Fixed,
            PaintLayerPolicy::DirectOnly,
            PaintLayerReason::Root,
            1,
            vec![],
        )
        .with_children(children);
        RenderScene {
            scroll_fling: None,
            nodes: vec![RenderNode::PaintLayer(root)],
            ..RenderScene::default()
        }
    }

    fn search_nodes(advance: impl FnOnce()) -> u64 {
        ANIMATION_SEARCH_NODES.with(|count| count.set(0));
        advance();
        ANIMATION_SEARCH_NODES.with(|count| count.get())
    }

    #[test]
    fn the_index_advances_exactly_what_a_full_search_does_without_searching() {
        let started_at = Instant::now();
        let animated = animated_layer(started_at, Some(child_layer(1)));
        let mut searched = page(200, animated.clone());
        let mut indexed = page(200, animated);
        let mut index = RendererAnimationIndex::default();
        for ms in [16, 33, 250, 251, 900] {
            let at = started_at + Duration::from_millis(ms);
            let mut full = RendererAnimationAdvance::default();
            let full_nodes = search_nodes(|| full = searched.advance_renderer_animations(at));
            let mut fast = RendererAnimationAdvance::default();
            let fast_nodes = search_nodes(|| fast = index.advance(&mut indexed, at));
            assert_eq!(indexed, searched, "at {ms} ms");
            assert_eq!(
                (fast.found, fast.changed, fast.damage),
                (full.found, full.changed, full.damage),
                "at {ms} ms"
            );
            // The full search walks the whole page every frame; the index
            // goes straight to the animated layer.
            assert!(full_nodes > 200, "{full_nodes}");
            assert_eq!(fast_nodes, 0, "at {ms} ms");
        }
    }

    #[test]
    fn the_index_follows_a_scene_replaced_behind_its_back() {
        let started_at = Instant::now();
        let mut index = RendererAnimationIndex::default();
        let mut scene = page(3, animated_layer(started_at, None));
        assert!(index.advance(&mut scene, started_at + Duration::from_millis(100)).changed);
        // A new scene without `invalidate`: no animation any more.
        let mut still = page(3, child_layer(1));
        let advance = index.advance(&mut still, started_at + Duration::from_millis(200));
        assert!(!advance.found && !advance.changed);
        // And one where it moved elsewhere.
        let mut moved = page(9, animated_layer(started_at, None));
        let advance = index.advance(&mut moved, started_at + Duration::from_millis(300));
        assert!(advance.found && advance.changed);
    }

    #[test]
    fn a_sample_that_rounds_to_the_drawn_one_is_not_applied() {
        let started_at = Instant::now();
        let mut layer = animated_layer(started_at, None);
        // A loop that holds still: two equal keyframes.
        let animation = Arc::make_mut(layer.renderer_animation.as_mut().unwrap());
        let held = Attrs {
            rotate: Some(0.0),
            ..Attrs::default()
        };
        animation.spec = Arc::new(AnimationSpec {
            keyframes: vec![held.clone(), held],
            duration_ms: 1000.0,
            curve: AnimationCurve::Linear,
            repeat: AnimationRepeat::Loop,
        });
        let mut scene = RenderScene {
            scroll_fling: None,
            nodes: vec![RenderNode::PaintLayer(layer)],
            ..RenderScene::default()
        };
        let before = scene.clone();
        let advance = scene.advance_renderer_animations(started_at + Duration::from_millis(300));
        assert!(advance.found);
        assert!(!advance.changed);
        assert!(advance.damage.is_none());
        assert_eq!(scene, before);

        // Far below a pixel per frame: 0.36 degrees over 100 s turns the
        // box's far corner ~0.28 px, ~3e-6 px per ms.
        let mut slow = animated_layer(started_at, None);
        let animation = Arc::make_mut(slow.renderer_animation.as_mut().unwrap());
        animation.spec = Arc::new(AnimationSpec {
            keyframes: vec![
                Attrs {
                    rotate: Some(0.0),
                    ..Attrs::default()
                },
                Attrs {
                    rotate: Some(0.36),
                    ..Attrs::default()
                },
            ],
            duration_ms: 100_000.0,
            curve: AnimationCurve::Linear,
            repeat: AnimationRepeat::Loop,
        });
        let mut scene = RenderScene {
            scroll_fling: None,
            nodes: vec![RenderNode::PaintLayer(slow)],
            ..RenderScene::default()
        };
        assert!(!scene
            .advance_renderer_animations(started_at + Duration::from_millis(16))
            .changed);
        // Skipped samples aren't applied, so motion still shows up once it
        // adds up: after 10 s the corner has moved ~0.03 px.
        let advance = scene.advance_renderer_animations(started_at + Duration::from_secs(10));
        assert!(advance.changed);
        assert!(advance.damage.is_some());
    }

    #[test]
    fn damage_that_cannot_be_bounded_is_not_lost_to_a_later_layer() {
        let started_at = Instant::now();
        let mut degenerate = animated_layer(started_at, None);
        let animation = Arc::make_mut(degenerate.renderer_animation.as_mut().unwrap());
        // Bounds measured under a singular transform: no screen mapping.
        animation.bounds_transform = Affine2 {
            xx: 0.0,
            yx: 0.0,
            xy: 0.0,
            yy: 0.0,
            tx: 0.0,
            ty: 0.0,
        };
        let mut second = animated_layer(started_at, None);
        second.id = PaintLayerId::new(8, PaintLayerReason::Animation);
        let mut scene = RenderScene {
            scroll_fling: None,
            nodes: vec![
                RenderNode::PaintLayer(degenerate),
                RenderNode::PaintLayer(second),
            ],
            ..RenderScene::default()
        };
        let advance = scene.advance_renderer_animations(started_at + Duration::from_millis(250));
        assert!(advance.changed);
        assert!(advance.damage.is_none(), "{:?}", advance.damage);
    }

    fn with_spec(keyframes: Vec<Attrs>, duration_ms: f64, curve: AnimationCurve) -> RenderPaintLayer {
        let mut layer = animated_layer(Instant::now(), None);
        Arc::make_mut(layer.renderer_animation.as_mut().unwrap()).spec = Arc::new(AnimationSpec {
            keyframes,
            duration_ms,
            curve,
            repeat: AnimationRepeat::Loop,
        });
        layer
    }

    fn motion(layer: &RenderPaintLayer) -> AnimationMotion {
        measure_animation_motion(layer, layer.renderer_animation.as_deref().unwrap())
    }

    #[test]
    fn motion_is_the_fastest_corner_and_alpha_change_in_a_period() {
        // Measurements carry a 10% margin (MOTION_MARGIN).
        // One turn a second: the corners (44.7 px from the centre of the
        // 80x40 box) move 2 pi r = 281 px/s.
        let spin = motion(&animated_layer(Instant::now(), None));
        assert!((spin.px_per_second - 281.0 * 1.1).abs() < 281.0 * 0.03, "{spin:?}");
        assert_eq!(spin.alpha_per_second, 0.0);

        // A slide of 20 px over 4 s, eased: peak speed above the 5 px/s average.
        let at = |x: f32| Attrs {
            move_x: Some(x as f64),
            ..Attrs::default()
        };
        let slide = motion(&with_spec(vec![at(0.0), at(20.0)], 4000.0, AnimationCurve::EaseInOut));
        assert!(slide.px_per_second > 6.0 && slide.px_per_second < 16.5, "{slide:?}");
        let linear = motion(&with_spec(vec![at(0.0), at(20.0)], 4000.0, AnimationCurve::Linear));
        assert!((linear.px_per_second - 5.5).abs() < 0.1, "{linear:?}");

        // A fade 0.2 -> 1 over 2 s; the wrap back to 0.2 isn't motion.
        let fade = |a: f64| Attrs {
            alpha: Some(a),
            ..Attrs::default()
        };
        let pulse = motion(&with_spec(vec![fade(0.2), fade(1.0)], 2000.0, AnimationCurve::Linear));
        assert!((pulse.alpha_per_second - 0.44).abs() < 0.01, "{pulse:?}");
        assert_eq!(pulse.px_per_second, 0.0);

        // Holding still.
        let held = motion(&with_spec(vec![fade(0.5), fade(0.5)], 1000.0, AnimationCurve::Linear));
        assert_eq!(held, AnimationMotion::default());
    }

    #[test]
    fn motions_are_measured_once_per_animation_and_layout() {
        let started_at = Instant::now();
        let animated = animated_layer(started_at, None);
        let mut index = RendererAnimationIndex::default();
        assert_eq!(index.motions(&page(3, animated.clone())).len(), 1);
        // Mark the stored measurement: a re-measure would overwrite it.
        let marker = AnimationMotion {
            px_per_second: -1.0,
            alpha_per_second: -1.0,
        };
        index.motions.values_mut().next().unwrap().motion = marker;
        // The same animation in a new scene keeps its measurement.
        index.invalidate();
        assert_eq!(index.motions(&page(5, animated.clone())), vec![marker]);
        // A new layout (the box moved) is measured again.
        let mut moved = animated.clone();
        moved.bounds.x += 10.0;
        index.invalidate();
        assert_ne!(index.motions(&page(5, moved)), vec![marker]);
        // A scene without it drops the entry.
        index.invalidate();
        assert!(index.motions(&page(5, child_layer(1))).is_empty());
        assert!(index.motions.is_empty());
    }
}
