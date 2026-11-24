//! One Raven patch: scroll content drawn from tiles, as a browser compositor
//! scrolls.
//!
//! The tree builds a scroll container's content as one paint layer for a
//! window wider than the viewport and places it with a whole-pixel
//! translation (`CompositorScroll` in tree/render.rs). Drawing that layer
//! directly re-rasterized every visible primitive on every scroll frame (on
//! the hub's Home screen: 28 ms of draw and 16 ms of GPU flush, much of it
//! borders through Skia's software path renderer). Here the layer is cut into
//! a grid of device-pixel tiles anchored at the layer's origin; each tile is
//! keyed by a hash of exactly what the content draws in it (each primitive's
//! wrappers, values and resource generation, in paint order), rasterized once
//! and composited at the layer's current position. A scroll frame draws
//! textures. When the content changes (an element changes, or the culling
//! window moves and brings in new content), only tiles whose key changed are
//! rasterized again; the rest keep their textures because the grid does not
//! move with the content.
//!
//! What cannot be baked into a texture is drawn every frame. A layer the
//! render thread animates (patch 4), or a nested scroll container, is left
//! out of the tiles and drawn over them, in its place in the content, when
//! nothing painted after it overlaps where it can move (the usual case: a
//! spinning icon on a card, a horizontal strip in a page). Otherwise,
//! and for video or an image still loading, the tiles it touches are
//! "dynamic" and drawn through the ordinary path, clipped to those tiles.
//! The layer falls back to the ordinary path entirely when its transform is
//! not a translation plus a uniform positive scale.

use super::*;

/// One Raven patch: compositor scroll tiles on or off for every renderer in
/// the process (`ScrollPhysics::compositor`).
static SCROLL_TILES_ENABLED: AtomicBool = AtomicBool::new(true);

pub(crate) fn set_scroll_tiles_enabled(enabled: bool) {
    SCROLL_TILES_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Tile size in device pixels.
pub(super) const SCROLL_TILE_WIDTH: i32 = 512;
pub(super) const SCROLL_TILE_HEIGHT: i32 = 256;
/// Texture memory the tiles of all scroll layers may hold.
const SCROLL_TILE_MAX_BYTES: u64 = 64 << 20;
/// Frames a layer that is not drawn keeps its tiles.
const SCROLL_TILE_MAX_UNSEEN_FRAMES: u64 = 600;
/// Largest offscreen surface one rasterization pass may make.
const SCROLL_TILE_MAX_PASS_PIXELS: i32 = 4096;

/// Tile activity since the renderer was made.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScrollTileCounts {
    /// Content plans made (one per content change).
    pub plans: u64,
    /// Tiles rasterized.
    pub rasterized: u64,
    /// Tiles composited from a texture.
    pub drawn: u64,
    /// Tiles drawn directly because they hold dynamic content.
    pub direct: u64,
    /// Layers drawn without tiles (unsupported transform or size).
    pub fallbacks: u64,
}

#[derive(Default)]
pub(super) struct ScrollTiles {
    pub(super) enabled: bool,
    /// Tiles on a CPU raster surface too (tests); by default only a GPU
    /// renderer tiles, where a texture draw is much cheaper than the content.
    pub(super) on_cpu: bool,
    layers: HashMap<PaintLayerId, LayerTiles>,
    bytes: u64,
    pub(super) counts: ScrollTileCounts,
}

struct LayerTiles {
    /// The content the plan was made for (kept alive, so the pointer is
    /// never reused while it is compared).
    content: Arc<crate::render_scene::RenderPaintLayerContent>,
    plan: TilePlan,
    tiles: HashMap<(i32, i32), Tile>,
    last_used: u64,
}

struct Tile {
    key: u64,
    image: Image,
    src: skia_safe::IRect,
    last_used: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct TileInfo {
    key: u64,
    dynamic: bool,
}

/// Renderer-animated layers drawn over the tiles, and the runs holding any
/// of them (by node `Arc` address; the plan's content keeps them alive).
#[derive(Default)]
pub(super) struct Overlays {
    pub(super) layers: HashSet<PaintLayerId>,
    pub(super) runs: HashSet<usize>,
    /// One Raven patch: grouped content holding any of them (by the address
    /// of its node list), the only content the overlay pass goes into.
    pub(super) paths: HashSet<usize>,
}

impl Overlays {
    /// Index `layers` in `nodes`: the runs and grouped content holding them.
    fn index(nodes: &[RenderPaintLayerContentNode], layers: HashSet<PaintLayerId>) -> Self {
        let mut overlays = Self {
            layers,
            runs: HashSet::new(),
            paths: HashSet::new(),
        };
        overlay_runs(nodes, None, &overlays.layers, &mut overlays.runs, &mut overlays.paths);
        overlays
    }
}

struct TilePlan {
    overlays: Arc<Overlays>,
    scale: f32,
    font_generation: Option<u64>,
    /// Image ids drawn, with the render generation each had.
    images: Vec<(String, Option<u64>)>,
    /// Tiles with something in them.
    tiles: HashMap<(i32, i32), TileInfo>,
}

impl TilePlan {
    fn current(&self, scale: f32) -> bool {
        self.scale == scale
            && self.font_generation == Some(font_render_generation())
            && self
                .images
                .iter()
                .all(|(id, generation)| asset_render_generation(id) == *generation)
    }
}

struct PlanBuilder {
    scale: f32,
    tiles: HashMap<(i32, i32), TileInfo>,
    images: Vec<(String, Option<u64>)>,
    leaves: u64,
    /// Renderer-animated layers seen so far that may still be overlays:
    /// (id, bounds, run holding it), until something painted later overlaps.
    candidates: Vec<(PaintLayerId, GeometryRect, Option<usize>, bool)>,
    run: Option<usize>,
    /// One Raven patch: the clip so far, in the layer's space (axis-aligned
    /// bounds of the clip shapes); a dynamic layer cannot draw outside it.
    clip: Option<GeometryRect>,
}

impl PlanBuilder {
    /// Run `inside` with `clips` (in `transform`'s space) added to the clip.
    fn clipped(&mut self, clips: &[ClipShape], transform: Affine2, inside: impl FnOnce(&mut Self)) {
        let outer = self.clip;
        for clip in clips {
            let rect = transform.map_rect_aabb(clip.rect);
            self.clip = Some(match self.clip {
                Some(current) => current.intersect(rect).unwrap_or(GeometryRect {
                    x: rect.x,
                    y: rect.y,
                    width: 0.0,
                    height: 0.0,
                }),
                None => rect,
            });
        }
        inside(self);
        self.clip = outer;
    }
}

fn mix(hash: u64, value: u64) -> u64 {
    (hash.rotate_left(7) ^ value).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

impl PlanBuilder {
    fn tile_range(&self, rect: GeometryRect) -> Option<(i32, i32, i32, i32)> {
        if !(rect.width.is_finite() && rect.height.is_finite()) || rect.width < 0.0 {
            return None;
        }
        let s = self.scale;
        let left = (rect.x * s).floor() as i64;
        let top = (rect.y * s).floor() as i64;
        let right = ((rect.x + rect.width) * s).ceil() as i64;
        let bottom = ((rect.y + rect.height) * s).ceil() as i64;
        let tw = i64::from(SCROLL_TILE_WIDTH);
        let th = i64::from(SCROLL_TILE_HEIGHT);
        let c0 = left.div_euclid(tw);
        let r0 = top.div_euclid(th);
        let c1 = (right.max(left + 1) - 1).div_euclid(tw);
        let r1 = (bottom.max(top + 1) - 1).div_euclid(th);
        // A primitive spanning thousands of tiles (an unbounded clip, a
        // huge shadow) is clamped to the layer anyway by its own bounds.
        let clamp = |v: i64| v.clamp(-1_000_000, 1_000_000) as i32;
        Some((clamp(c0), clamp(r0), clamp(c1), clamp(r1)))
    }

    fn mark_dynamic(&mut self, rect: GeometryRect) {
        let Some((c0, r0, c1, r1)) = self.tile_range(rect) else {
            return;
        };
        if (c1 - c0) > 256 || (r1 - r0) > 4096 {
            return;
        }
        for row in r0..=r1 {
            for col in c0..=c1 {
                self.tiles.entry((col, row)).or_default().dynamic = true;
            }
        }
    }

    fn leaf(&mut self, bounds: GeometryRect, hash: u64, dynamic: bool) {
        self.leaves += 1;
        // Painted after an overlay and over where it can draw: the overlay
        // would draw over it, so the tiles they share are drawn the ordinary
        // way, in paint order, and the overlay is left out of them. One Raven
        // patch: only those tiles. The whole overlay went through the tiles,
        // so the label under a music equaliser made every tile the equaliser
        // could reach dynamic.
        // A nested scroller is large and draws from its own tiles: it goes
        // through the tiles whole instead, as before.
        let mut shared = Vec::new();
        for candidate in &mut self.candidates {
            if !candidate.3 {
                continue;
            }
            if let Some(rect) = candidate.1.intersect(bounds) {
                if candidate.0.role == PaintLayerReason::ScrollContent {
                    candidate.3 = false;
                } else {
                    shared.push(rect);
                }
            }
        }
        for rect in shared {
            self.mark_dynamic(rect);
        }
        let Some((c0, r0, c1, r1)) = self.tile_range(bounds) else {
            return;
        };
        // Leaves larger than ~256 tiles in either direction are bounded by
        // the layer's own content anyway; avoid pathological loops.
        if (c1 - c0) > 256 || (r1 - r0) > 4096 {
            return;
        }
        // One Raven patch: the key is what this tile draws, in order (`mix`
        // is order-sensitive). It mixed in each leaf's ordinal in the whole
        // layer too, so content culled above the viewport when the window
        // moved changed the key of every tile below it: a whole viewport
        // rasterized again (24 tiles, 35-40 ms) at every re-anchor.
        for row in r0..=r1 {
            for col in c0..=c1 {
                let info = self.tiles.entry((col, row)).or_default();
                info.key = mix(info.key, hash);
                info.dynamic |= dynamic;
            }
        }
    }
}

fn layer_is_dynamic(layer: &RenderPaintLayer) -> bool {
    layer_moves_alone(layer)
        || layer.id.role == PaintLayerReason::DirectMedia
        || layer.policy == PaintLayerPolicy::DirectOnly
}

/// A layer that changes on its own: a renderer-driven animation, or a
/// nested scroll container (drawn from its own tiles).
fn layer_moves_alone(layer: &RenderPaintLayer) -> bool {
    layer.renderer_animation.is_some() || layer.id.role == PaintLayerReason::ScrollContent
}

/// Conservative bounds of a dynamic child layer in the parent's space. A
/// renderer-driven animation that only moves the layer covers its bounds
/// grown by the moves' range on each axis (the curves never overshoot a
/// keyframe); one that may turn or scale it covers the layer's circumscribed
/// square grown by its size. One Raven patch: both within the clip it is
/// drawn in. The square around each bar of a music equaliser overlapped its
/// neighbours and the text under it, so every tile it touched was drawn
/// directly each frame (33 of 39 on the Media page).
fn dynamic_layer_bounds(
    layer: &RenderPaintLayer,
    transform: Affine2,
    clip: Option<GeometryRect>,
) -> GeometryRect {
    let bounds = transform.map_rect_aabb(layer.bounds);
    let motion = match &layer.renderer_animation {
        None => bounds,
        Some(animation) => match translation_range(animation) {
            Some((dx, dy)) => {
                let rx = dx * (transform.xx.abs() + transform.xy.abs());
                let ry = dy * (transform.yx.abs() + transform.yy.abs());
                GeometryRect {
                    x: bounds.x - rx,
                    y: bounds.y - ry,
                    width: bounds.width + 2.0 * rx,
                    height: bounds.height + 2.0 * ry,
                }
            }
            None => {
                let cx = bounds.x + bounds.width / 2.0;
                let cy = bounds.y + bounds.height / 2.0;
                let radius =
                    (bounds.width.hypot(bounds.height)) / 2.0 + bounds.width.max(bounds.height);
                GeometryRect {
                    x: cx - radius,
                    y: cy - radius,
                    width: radius * 2.0,
                    height: radius * 2.0,
                }
            }
        },
    };
    match clip {
        Some(clip) => motion.intersect(clip).unwrap_or(GeometryRect {
            x: motion.x,
            y: motion.y,
            width: 0.0,
            height: 0.0,
        }),
        None => motion,
    }
}

/// How far a renderer-driven animation that only moves its layer can move it
/// on each axis: the range of its keyframes' moves. None if it turns or
/// scales, or the element is turned.
fn translation_range(animation: &crate::render_scene::LayerRendererAnimation) -> Option<(f32, f32)> {
    let base = animation.base;
    if base.rotate.is_some_and(|r| r != 0.0) || base.layout_rotate.is_some_and(|r| r != 0.0) {
        return None;
    }
    let mut x = (f64::INFINITY, f64::NEG_INFINITY);
    let mut y = (f64::INFINITY, f64::NEG_INFINITY);
    for keyframe in &animation.spec.keyframes {
        if keyframe.rotate.is_some() || keyframe.scale.is_some() {
            return None;
        }
        let mx = keyframe.move_x.or(base.move_x).unwrap_or(0.0);
        let my = keyframe.move_y.or(base.move_y).unwrap_or(0.0);
        x = (x.0.min(mx), x.1.max(mx));
        y = (y.0.min(my), y.1.max(my));
    }
    if !(x.0.is_finite() && y.0.is_finite()) {
        return None;
    }
    Some(((x.1 - x.0) as f32, (y.1 - y.0) as f32))
}

fn plan_content(
    builder: &mut PlanBuilder,
    nodes: &[RenderPaintLayerContentNode],
    transform: Affine2,
    context: u64,
) {
    for node in nodes {
        match node {
            RenderPaintLayerContentNode::Own(run) => {
                let outer = builder.run.replace(Arc::as_ptr(&run.nodes) as usize);
                plan_nodes(builder, &run.nodes, transform, context);
                builder.run = outer;
            }
            RenderPaintLayerContentNode::Child(layer) => {
                plan_layer(builder, layer, transform, context)
            }
            RenderPaintLayerContentNode::ShadowPass { children } => {
                plan_content(builder, children, transform, mix(context, 1))
            }
            RenderPaintLayerContentNode::Clip { clips, children } => {
                builder.clipped(clips, transform, |builder| {
                    plan_content(builder, children, transform, clip_context(context, clips, 2))
                })
            }
            RenderPaintLayerContentNode::RelaxedClip { clips, children } => plan_content(
                builder,
                children,
                transform,
                clip_context(context, clips, 3),
            ),
            RenderPaintLayerContentNode::Transform {
                transform: local,
                children,
            } => plan_content(
                builder,
                children,
                transform.then(*local),
                transform_context(context, *local),
            ),
            RenderPaintLayerContentNode::Alpha { alpha, children } => plan_content(
                builder,
                children,
                transform,
                mix(mix(context, 5), u64::from(alpha.to_bits())),
            ),
        }
    }
}

fn plan_layer(
    builder: &mut PlanBuilder,
    layer: &RenderPaintLayer,
    transform: Affine2,
    context: u64,
) {
    if layer_is_dynamic(layer) {
        let bounds = dynamic_layer_bounds(layer, transform, builder.clip);
        if layer_moves_alone(layer) {
            builder.leaves += 1;
            builder
                .candidates
                .push((layer.id, bounds, builder.run, true));
        } else {
            builder.leaf(bounds, 0, true);
        }
        return;
    }
    let mut hasher = FastHasher::new();
    layer.id.hash(&mut hasher);
    plan_content(
        builder,
        &layer.content.nodes,
        transform,
        mix(context, hasher.finish()),
    );
}

fn plan_nodes(builder: &mut PlanBuilder, nodes: &[RenderNode], transform: Affine2, context: u64) {
    for node in nodes {
        match node {
            RenderNode::ShadowPass { children } => {
                plan_nodes(builder, children, transform, mix(context, 1))
            }
            RenderNode::Clip { clips, children } => builder.clipped(clips, transform, |builder| {
                plan_nodes(builder, children, transform, clip_context(context, clips, 2))
            }),
            RenderNode::RelaxedClip { clips, children } => plan_nodes(
                builder,
                children,
                transform,
                clip_context(context, clips, 3),
            ),
            RenderNode::Transform {
                transform: local,
                children,
            } => plan_nodes(
                builder,
                children,
                transform.then(*local),
                transform_context(context, *local),
            ),
            RenderNode::Alpha { alpha, children } => plan_nodes(
                builder,
                children,
                transform,
                mix(mix(context, 5), u64::from(alpha.to_bits())),
            ),
            RenderNode::PaintLayer(layer) => plan_layer(builder, layer, transform, context),
            RenderNode::Primitive(primitive) => {
                let Some(bounds) = crate::render_scene::render_node_visual_bounds(node, transform)
                else {
                    continue;
                };
                let generation = render_primitive_resource_generation(primitive);
                if let DrawPrimitive::Image(_, _, _, _, id, _, _) = primitive
                    && !builder.images.iter().any(|(seen, _)| seen == id)
                {
                    builder.images.push((id.clone(), generation));
                }
                let mut hasher = FastHasher::new();
                hash_paint_layer_draw_primitive(&mut hasher, primitive, PaintLayerHashFloat::Exact);
                hasher.write_u64(generation.unwrap_or(0));
                builder.leaf(bounds, mix(context, hasher.finish()), generation.is_none());
            }
        }
    }
}

/// One Raven patch: the runs `make_plan` would record for `layers` (each
/// overlay's innermost run), without the rest of a plan.
/// Whether `nodes` hold any of `layers`; records the runs and grouped
/// content (`Overlays::paths`) on the way.
fn overlay_runs(
    nodes: &[RenderPaintLayerContentNode],
    run: Option<usize>,
    layers: &HashSet<PaintLayerId>,
    out: &mut HashSet<usize>,
    paths: &mut HashSet<usize>,
) -> bool {
    let mut found = false;
    for node in nodes {
        found |= match node {
            RenderPaintLayerContentNode::Own(own) => overlay_runs_in_nodes(
                &own.nodes,
                Some(Arc::as_ptr(&own.nodes) as usize),
                layers,
                out,
                paths,
            ),
            RenderPaintLayerContentNode::Child(layer) => {
                overlay_runs_in_layer(layer, run, layers, out, paths)
            }
            RenderPaintLayerContentNode::ShadowPass { children }
            | RenderPaintLayerContentNode::Clip { children, .. }
            | RenderPaintLayerContentNode::RelaxedClip { children, .. }
            | RenderPaintLayerContentNode::Transform { children, .. }
            | RenderPaintLayerContentNode::Alpha { children, .. } => {
                let inside = overlay_runs(children, run, layers, out, paths);
                if inside {
                    paths.insert(children.as_ptr() as usize);
                }
                inside
            }
        };
    }
    found
}

fn overlay_runs_in_layer(
    layer: &RenderPaintLayer,
    run: Option<usize>,
    layers: &HashSet<PaintLayerId>,
    out: &mut HashSet<usize>,
    paths: &mut HashSet<usize>,
) -> bool {
    if layer_is_dynamic(layer) {
        let overlay = layers.contains(&layer.id);
        if overlay && let Some(run) = run {
            out.insert(run);
        }
        return overlay;
    }
    let inside = overlay_runs(&layer.content.nodes, run, layers, out, paths);
    if inside {
        paths.insert(layer.content.nodes.as_ptr() as usize);
    }
    inside
}

fn overlay_runs_in_nodes(
    nodes: &[RenderNode],
    run: Option<usize>,
    layers: &HashSet<PaintLayerId>,
    out: &mut HashSet<usize>,
    paths: &mut HashSet<usize>,
) -> bool {
    let mut found = false;
    for node in nodes {
        found |= match node {
            RenderNode::ShadowPass { children }
            | RenderNode::Clip { children, .. }
            | RenderNode::RelaxedClip { children, .. }
            | RenderNode::Transform { children, .. }
            | RenderNode::Alpha { children, .. } => {
                overlay_runs_in_nodes(children, run, layers, out, paths)
            }
            RenderNode::PaintLayer(layer) => overlay_runs_in_layer(layer, run, layers, out, paths),
            RenderNode::Primitive(_) => false,
        };
    }
    found
}

fn clip_context(context: u64, clips: &[ClipShape], tag: u64) -> u64 {
    let mut hasher = FastHasher::new();
    hash_paint_layer_clip_shapes(&mut hasher, clips, PaintLayerHashFloat::Exact);
    mix(mix(context, tag), hasher.finish())
}

fn transform_context(context: u64, transform: Affine2) -> u64 {
    let mut hasher = FastHasher::new();
    hash_paint_layer_affine2(&mut hasher, transform, PaintLayerHashFloat::Exact);
    mix(mix(context, 4), hasher.finish())
}

fn make_plan(layer: &RenderPaintLayer, scale: f32) -> TilePlan {
    let mut builder = PlanBuilder {
        scale,
        tiles: HashMap::new(),
        images: Vec::new(),
        leaves: 0,
        candidates: Vec::new(),
        run: None,
        clip: None,
    };
    plan_content(&mut builder, &layer.content.nodes, Affine2::identity(), 0);
    let mut overlay_layers = HashSet::new();
    for (id, bounds, _run, overlay) in std::mem::take(&mut builder.candidates) {
        if overlay {
            overlay_layers.insert(id);
        } else {
            // Its tiles are drawn the ordinary way, with it in place.
            let leaves = builder.leaves;
            builder.leaf(bounds, 0, true);
            builder.leaves = leaves;
        }
    }
    let overlays = Overlays::index(&layer.content.nodes, overlay_layers);
    TilePlan {
        overlays: Arc::new(overlays),
        scale,
        font_generation: Some(font_render_generation()),
        images: builder.images,
        tiles: builder.tiles,
    }
}

/// The layer's device placement: a uniform positive scale and a translation.
fn layer_placement(canvas: &skia_safe::Canvas) -> Option<(f32, f32, f32)> {
    let matrix = canvas.local_to_device_as_3x3();
    if !matrix.is_scale_translate() {
        return None;
    }
    let (sx, sy) = (matrix.scale_x(), matrix.scale_y());
    if !(sx > 0.0 && sy > 0.0 && (sx - sy).abs() <= 1e-4 * sx.max(1.0)) {
        return None;
    }
    let (tx, ty) = (matrix.translate_x(), matrix.translate_y());
    (tx.is_finite() && ty.is_finite()).then_some((sx, tx.round(), ty.round()))
}

pub(super) enum TileDraw {
    /// Tiles drawn. `direct`: device rects still to draw the ordinary way;
    /// `overlays`: renderer-animated layers to draw over the tiles.
    Drawn {
        direct: Vec<skia_safe::IRect>,
        overlays: Option<Arc<Overlays>>,
    },
    /// Not tiled; draw the layer the ordinary way.
    Fallback,
}

impl std::fmt::Debug for ScrollTiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScrollTiles")
            .field("enabled", &self.enabled)
            .field("on_cpu", &self.on_cpu)
            .field("layers", &self.layers.len())
            .field("bytes", &self.bytes)
            .field("counts", &self.counts)
            .finish()
    }
}

impl ScrollTiles {
    pub(super) fn new() -> Self {
        Self {
            enabled: true,
            ..Self::default()
        }
    }

    pub(super) fn clear(&mut self) {
        self.layers.clear();
        self.bytes = 0;
    }

    pub(super) fn begin_frame(&mut self, frame_index: u64) {
        self.layers.retain(|_, layer| {
            frame_index.saturating_sub(layer.last_used) <= SCROLL_TILE_MAX_UNSEEN_FRAMES
        });
        self.bytes = self
            .layers
            .values()
            .flat_map(|layer| layer.tiles.values())
            .map(|tile| tile_bytes(tile.src))
            .sum();
    }

    /// Draw `layer` from tiles at the canvas's current placement.
    pub(super) fn draw(
        &mut self,
        canvas: &skia_safe::Canvas,
        layer: &RenderPaintLayer,
        options: RenderTraversalOptions<'_>,
        gpu_context: Option<&mut gpu::DirectContext>,
        frame_index: u64,
    ) -> TileDraw {
        if (gpu_context.is_none() && !self.on_cpu) || !SCROLL_TILES_ENABLED.load(Ordering::Relaxed)
        {
            return TileDraw::Fallback;
        }
        let Some((scale, tx, ty)) = layer_placement(canvas) else {
            self.counts.fallbacks += 1;
            return TileDraw::Fallback;
        };
        let empty = TileDraw::Drawn {
            direct: Vec::new(),
            overlays: None,
        };
        let Some(clip) = canvas.device_clip_bounds() else {
            return empty;
        };
        if clip.is_empty() {
            return empty;
        }

        let entry = self.layers.entry(layer.id);
        let state = match entry {
            std::collections::hash_map::Entry::Occupied(occupied) => {
                let state = occupied.into_mut();
                // One Raven patch: the same build copied on write by the
                // render thread (to advance a renderer-driven animation in
                // it) keeps its plan; only the overlay runs moved.
                if !Arc::ptr_eq(&state.content, &layer.content)
                    && state.content.source.same_build(layer.content.source)
                    && state.plan.current(scale)
                {
                    state.plan.overlays = Arc::new(Overlays::index(
                        &layer.content.nodes,
                        state.plan.overlays.layers.clone(),
                    ));
                    state.content = Arc::clone(&layer.content);
                }
                if !Arc::ptr_eq(&state.content, &layer.content) || !state.plan.current(scale) {
                    state.content = Arc::clone(&layer.content);
                    state.plan = make_plan(layer, scale);
                    self.counts.plans += 1;
                }
                state
            }
            std::collections::hash_map::Entry::Vacant(vacant) => {
                self.counts.plans += 1;
                vacant.insert(LayerTiles {
                    content: Arc::clone(&layer.content),
                    plan: make_plan(layer, scale),
                    tiles: HashMap::new(),
                    last_used: frame_index,
                })
            }
        };
        state.last_used = frame_index;

        // Visible tiles: the device clip, in the layer's tile grid.
        let tw = SCROLL_TILE_WIDTH;
        let th = SCROLL_TILE_HEIGHT;
        let (ox, oy) = (tx as i32, ty as i32);
        let c0 = (clip.left - ox).div_euclid(tw);
        let r0 = (clip.top - oy).div_euclid(th);
        let c1 = (clip.right - 1 - ox).div_euclid(tw);
        let r1 = (clip.bottom - 1 - oy).div_euclid(th);

        let mut missing = Vec::new();
        let mut visible = Vec::new();
        let mut direct = Vec::new();
        for row in r0..=r1 {
            for col in c0..=c1 {
                let Some(info) = state.plan.tiles.get(&(col, row)).copied() else {
                    continue;
                };
                let dst = skia_safe::IRect::from_xywh(ox + col * tw, oy + row * th, tw, th);
                if info.dynamic {
                    direct.push(dst);
                    continue;
                }
                if state
                    .tiles
                    .get(&(col, row))
                    .is_none_or(|tile| tile.key != info.key)
                {
                    missing.push((col, row, info.key));
                }
                visible.push((col, row, dst));
            }
        }

        if !missing.is_empty() {
            let mc0 = missing.iter().map(|m| m.0).min().unwrap_or(0);
            let mc1 = missing.iter().map(|m| m.0).max().unwrap_or(0);
            let mr0 = missing.iter().map(|m| m.1).min().unwrap_or(0);
            let mr1 = missing.iter().map(|m| m.1).max().unwrap_or(0);
            let width = (mc1 - mc0 + 1) * tw;
            let height = (mr1 - mr0 + 1) * th;
            if width > SCROLL_TILE_MAX_PASS_PIXELS || height > SCROLL_TILE_MAX_PASS_PIXELS {
                self.counts.fallbacks += 1;
                return TileDraw::Fallback;
            }
            let Some(image) = rasterize(
                layer,
                options,
                gpu_context,
                &state.plan.overlays,
                scale,
                mc0,
                mr0,
                width,
                height,
            ) else {
                self.counts.fallbacks += 1;
                return TileDraw::Fallback;
            };
            for (col, row, key) in missing {
                let src = skia_safe::IRect::from_xywh((col - mc0) * tw, (row - mr0) * th, tw, th);
                if let Some(old) = state.tiles.insert(
                    (col, row),
                    Tile {
                        key,
                        image: image.clone(),
                        src,
                        last_used: frame_index,
                    },
                ) {
                    self.bytes = self.bytes.saturating_sub(tile_bytes(old.src));
                }
                self.bytes += tile_bytes(src);
                self.counts.rasterized += 1;
            }
        }

        canvas.save();
        canvas.reset_matrix();
        let paint = Paint::default();
        for (col, row, dst) in visible {
            let Some(tile) = state.tiles.get_mut(&(col, row)) else {
                continue;
            };
            tile.last_used = frame_index;
            canvas.draw_image_rect_with_sampling_options(
                &tile.image,
                Some((&skia_safe::Rect::from(tile.src), SrcRectConstraint::Strict)),
                skia_safe::Rect::from(dst),
                SamplingOptions::default(),
                &paint,
            );
            self.counts.drawn += 1;
        }
        canvas.restore();

        let overlays =
            (!state.plan.overlays.layers.is_empty()).then(|| Arc::clone(&state.plan.overlays));
        self.evict(frame_index);
        self.counts.direct += direct.len() as u64;
        TileDraw::Drawn { direct, overlays }
    }

    /// Over budget: drop the tiles drawn longest ago, never this frame's.
    fn evict(&mut self, frame_index: u64) {
        if self.bytes <= SCROLL_TILE_MAX_BYTES {
            return;
        }
        let mut candidates: Vec<(u64, PaintLayerId, (i32, i32))> = self
            .layers
            .iter()
            .flat_map(|(id, layer)| {
                layer
                    .tiles
                    .iter()
                    .filter(|(_, tile)| tile.last_used != frame_index)
                    .map(move |(index, tile)| (tile.last_used, *id, *index))
            })
            .collect();
        candidates.sort_unstable_by_key(|(last_used, _, _)| *last_used);
        for (_, id, index) in candidates {
            if self.bytes <= SCROLL_TILE_MAX_BYTES {
                break;
            }
            if let Some(tile) = self
                .layers
                .get_mut(&id)
                .and_then(|layer| layer.tiles.remove(&index))
            {
                self.bytes = self.bytes.saturating_sub(tile_bytes(tile.src));
            }
        }
    }
}

fn tile_bytes(src: skia_safe::IRect) -> u64 {
    (src.width().max(0) as u64) * (src.height().max(0) as u64) * 4
}

/// Rasterize the tiles from (`col0`, `row0`) over `width` x `height` device
/// pixels in one pass, as the ordinary path would draw them.
#[allow(clippy::too_many_arguments)]
fn rasterize(
    layer: &RenderPaintLayer,
    options: RenderTraversalOptions<'_>,
    gpu_context: Option<&mut gpu::DirectContext>,
    overlays: &Overlays,
    scale: f32,
    col0: i32,
    row0: i32,
    width: i32,
    height: i32,
) -> Option<Image> {
    let info = ImageInfo::new(
        (width, height),
        ColorType::RGBA8888,
        AlphaType::Premul,
        None,
    );
    #[cfg(any(feature = "opengl", feature = "vulkan", feature = "macos"))]
    let mut surface = match gpu_context {
        Some(gr_context) => gpu::surfaces::render_target(
            gr_context,
            gpu::Budgeted::Yes,
            &info,
            0,
            gpu::SurfaceOrigin::TopLeft,
            None,
            false,
            false,
        )?,
        None => surfaces::raster(&info, None, None)?,
    };
    #[cfg(not(any(feature = "opengl", feature = "vulkan", feature = "macos")))]
    let mut surface = {
        let _ = gpu_context;
        surfaces::raster(&info, None, None)?
    };
    let canvas = surface.canvas();
    canvas.clear(Color::TRANSPARENT);
    canvas.save();
    canvas.translate((
        -(col0 * SCROLL_TILE_WIDTH) as f32,
        -(row0 * SCROLL_TILE_HEIGHT) as f32,
    ));
    canvas.scale((scale, scale));
    let mut mode = TileRasterMode { overlays };
    let mut instrumentation = NoDrawInstrumentation;
    SceneRenderer::render_paint_layer_content_in_mode(
        canvas,
        layer,
        &layer.content.nodes,
        &mut mode,
        options.with_active_clip(None),
        MovingLayerEligibility::root(),
        &mut instrumentation,
    );
    canvas.restore();
    Some(surface.image_snapshot())
}

/// Draws content as `DirectRenderMode` does, without the overlay layers.
struct TileRasterMode<'a> {
    overlays: &'a Overlays,
}

impl<'video> RenderTraversalMode<'video> for TileRasterMode<'_> {
    const TRACK_ELIGIBILITY: bool = false;

    fn render_paint_layer<I: DrawInstrumentation>(
        &mut self,
        canvas: &skia_safe::Canvas,
        layer: &RenderPaintLayer,
        options: RenderTraversalOptions<'video>,
        eligibility: MovingLayerEligibility,
        instrumentation: &mut I,
    ) {
        if self.overlays.layers.contains(&layer.id) {
            return;
        }
        SceneRenderer::render_paint_layer_content_in_mode(
            canvas,
            layer,
            &layer.content.nodes,
            self,
            options,
            eligibility,
            instrumentation,
        );
    }

    fn render_paint_run<I: DrawInstrumentation>(
        &mut self,
        canvas: &skia_safe::Canvas,
        _layer: &RenderPaintLayer,
        run: &RenderPaintRun,
        options: RenderTraversalOptions<'video>,
        eligibility: MovingLayerEligibility,
        instrumentation: &mut I,
    ) {
        SceneRenderer::render_nodes_in_mode(
            canvas,
            &run.nodes,
            self,
            options,
            eligibility,
            instrumentation,
        );
    }
}

/// Draws only the overlay layers, each in its place in the content (its
/// clips, transforms and alpha), through `inner` (which draws a renderer-
/// animated layer from its snapshot). Runs without an overlay are skipped.
pub(super) struct OverlayMode<'a, M> {
    pub(super) inner: &'a mut M,
    pub(super) overlays: &'a Overlays,
    /// Device rects drawn the ordinary way (with the overlays in them).
    /// One Raven patch: each overlay is clipped out of only those it reaches;
    /// clipping every overlay out of all of them made a complex clip, which
    /// Skia applies through a software mask for every draw under it.
    pub(super) direct: &'a [skia_safe::IRect],
}

impl<'video, M: RenderTraversalMode<'video>> RenderTraversalMode<'video> for OverlayMode<'_, M> {
    const TRACK_ELIGIBILITY: bool = false;
    const RENDER_PRIMITIVES: bool = false;

    fn visits(&self, children: &[RenderPaintLayerContentNode]) -> bool {
        self.overlays.paths.contains(&(children.as_ptr() as usize))
    }

    fn render_paint_layer<I: DrawInstrumentation>(
        &mut self,
        canvas: &skia_safe::Canvas,
        layer: &RenderPaintLayer,
        options: RenderTraversalOptions<'video>,
        eligibility: MovingLayerEligibility,
        instrumentation: &mut I,
    ) {
        if self.overlays.layers.contains(&layer.id) {
            let local = dynamic_layer_bounds(layer, Affine2::identity(), None);
            let reach = canvas
                .local_to_device_as_3x3()
                .map_rect(skia_safe::Rect::from_xywh(
                    local.x,
                    local.y,
                    local.width,
                    local.height,
                ))
                .0;
            let mut region = skia_safe::Region::new();
            for rect in self.direct {
                if skia_safe::Rect::from(*rect).intersects(reach) {
                    region.op_rect(*rect, skia_safe::region::RegionOp::Union);
                }
            }
            if region.is_empty() {
                self.inner
                    .render_paint_layer(canvas, layer, options, eligibility, instrumentation);
            } else {
                let matrix = canvas.local_to_device();
                canvas.save();
                canvas.reset_matrix();
                canvas.clip_region(&region, skia_safe::ClipOp::Difference);
                canvas.set_matrix(&matrix);
                self.inner
                    .render_paint_layer(canvas, layer, options, eligibility, instrumentation);
                canvas.restore();
            }
            return;
        }
        if !self
            .overlays
            .paths
            .contains(&(layer.content.nodes.as_ptr() as usize))
        {
            return;
        }
        SceneRenderer::render_paint_layer_content_in_mode(
            canvas,
            layer,
            &layer.content.nodes,
            self,
            options,
            eligibility,
            instrumentation,
        );
    }

    fn render_paint_run<I: DrawInstrumentation>(
        &mut self,
        canvas: &skia_safe::Canvas,
        _layer: &RenderPaintLayer,
        run: &RenderPaintRun,
        options: RenderTraversalOptions<'video>,
        eligibility: MovingLayerEligibility,
        instrumentation: &mut I,
    ) {
        if self
            .overlays
            .runs
            .contains(&(Arc::as_ptr(&run.nodes) as usize))
        {
            SceneRenderer::render_nodes_in_mode(
                canvas,
                &run.nodes,
                self,
                options,
                eligibility,
                instrumentation,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_scene::{
        AnimatedBase, LayerRendererAnimation, PaintLayerPlacement, RenderPaintLayerBuildParts,
        RenderPaintLayerContent,
    };
    use crate::tree::animation::{AnimationCurve, AnimationRepeat, AnimationSpec};

    fn rect(x: f32, y: f32, w: f32, h: f32, color: u32) -> RenderNode {
        RenderNode::Primitive(DrawPrimitive::Rect(x, y, w, h, RenderColor::from(color)))
    }

    fn layer(id: u64, role: PaintLayerReason, nodes: Vec<RenderNode>) -> RenderPaintLayer {
        RenderPaintLayer::from_prepared_children(
            RenderPaintLayerBuildParts {
                id: PaintLayerId::new(id, role),
                bounds: GeometryRect {
                    x: 0.0,
                    y: 0.0,
                    width: 1000.0,
                    height: 3000.0,
                },
                placement: PaintLayerPlacement::ScrollMoving,
                policy: PaintLayerPolicy::Cacheable,
                content_generation: 1,
                visual_bounds: None,
            },
            RenderPaintLayerContent::from_composition_nodes(nodes),
        )
    }

    fn spinner(x: f32, y: f32) -> RenderNode {
        let mut spinner = layer(
            9,
            PaintLayerReason::Animation,
            vec![rect(x, y, 40.0, 40.0, 0xff0000ff)],
        );
        spinner.bounds = GeometryRect {
            x,
            y,
            width: 40.0,
            height: 40.0,
        };
        spinner.renderer_animation = Some(Arc::new(LayerRendererAnimation {
            spec: Arc::new(AnimationSpec {
                keyframes: vec![
                    crate::tree::attrs::Attrs {
                        rotate: Some(0.0),
                        ..Default::default()
                    },
                    crate::tree::attrs::Attrs {
                        rotate: Some(360.0),
                        ..Default::default()
                    },
                ],
                duration_ms: 1000.0,
                curve: AnimationCurve::Linear,
                repeat: AnimationRepeat::Loop,
            }),
            started_at: Instant::now(),
            revision: 0,
            frame: Default::default(),
            base: AnimatedBase::default(),
            current_transform: Affine2::identity(),
            current_alpha: 1.0,
            offset: (0.0, 0.0),
            bounds_transform: Affine2::identity(),
        }));
        RenderNode::PaintLayer(spinner)
    }

    #[test]
    fn an_animated_layer_nothing_covers_is_drawn_over_the_tiles() {
        let content = layer(
            1,
            PaintLayerReason::ScrollContent,
            vec![
                rect(0.0, 0.0, 1000.0, 600.0, 0x202020ff),
                spinner(100.0, 100.0),
                rect(0.0, 1000.0, 1000.0, 100.0, 0x404040ff),
            ],
        );
        let plan = make_plan(&content, 1.0);
        assert!(
            plan.overlays
                .layers
                .contains(&PaintLayerId::new(9, PaintLayerReason::Animation))
        );
        assert!(plan.tiles.values().all(|tile| !tile.dynamic));
    }

    #[test]
    fn an_animated_layer_under_later_content_makes_the_shared_tiles_dynamic() {
        let content = layer(
            1,
            PaintLayerReason::ScrollContent,
            vec![
                spinner(100.0, 100.0),
                // Painted after the spinner, over where it turns.
                rect(110.0, 110.0, 20.0, 20.0, 0x404040ff),
            ],
        );
        let plan = make_plan(&content, 1.0);
        // It stays an overlay; the tile it shares with the later content is
        // drawn the ordinary way, in paint order.
        assert!(
            plan.overlays
                .layers
                .contains(&PaintLayerId::new(9, PaintLayerReason::Animation))
        );
        assert!(plan.tiles.get(&(0, 0)).is_some_and(|tile| tile.dynamic));
        // Far away tiles hold nothing.
        assert!(!plan.tiles.contains_key(&(0, 8)));
    }

    #[test]
    fn a_tile_key_changes_only_with_what_it_draws() {
        let base = |color| {
            layer(
                1,
                PaintLayerReason::ScrollContent,
                vec![
                    rect(0.0, 0.0, 100.0, 100.0, 0x202020ff),
                    rect(0.0, 600.0, 100.0, 100.0, color),
                ],
            )
        };
        let before = make_plan(&base(0x404040ff), 1.0);
        let after = make_plan(&base(0x808080ff), 1.0);
        assert_eq!(before.tiles[&(0, 0)].key, after.tiles[&(0, 0)].key);
        assert_ne!(before.tiles[&(0, 2)].key, after.tiles[&(0, 2)].key);
    }

    // One Raven patch: bars that only move up and down inside their own
    // clip boxes (a music equaliser) stay overlays though their neighbours
    // and the text under them are painted after them.
    #[test]
    fn moving_bars_in_clip_boxes_stay_overlays() {
        let bar = |id: u64, x: f32| {
            let mut bar = layer(id, PaintLayerReason::Animation, vec![rect(x, 140.0, 6.0, 40.0, 0xff0000ff)]);
            bar.bounds = GeometryRect { x, y: 140.0, width: 6.0, height: 40.0 };
            bar.renderer_animation = Some(Arc::new(LayerRendererAnimation {
                spec: Arc::new(AnimationSpec {
                    keyframes: vec![
                        crate::tree::attrs::Attrs { move_y: Some(0.0), ..Default::default() },
                        crate::tree::attrs::Attrs { move_y: Some(-40.0), ..Default::default() },
                    ],
                    duration_ms: 800.0,
                    curve: AnimationCurve::EaseInOut,
                    repeat: AnimationRepeat::Loop,
                }),
                started_at: Instant::now(),
                revision: 0,
                frame: Default::default(),
                base: AnimatedBase::default(),
                current_transform: Affine2::identity(),
                current_alpha: 1.0,
                offset: (0.0, 0.0),
                bounds_transform: Affine2::identity(),
            }));
            RenderNode::Clip {
                clips: vec![ClipShape {
                    rect: GeometryRect { x, y: 140.0, width: 6.0, height: 40.0 },
                    radii: None,
                }],
                children: vec![RenderNode::PaintLayer(bar)],
            }
        };
        let content = layer(
            1,
            PaintLayerReason::ScrollContent,
            vec![
                rect(0.0, 0.0, 1000.0, 600.0, 0x202020ff),
                bar(21, 100.0),
                bar(22, 110.0),
                // A label just under the bars.
                rect(90.0, 184.0, 200.0, 20.0, 0x404040ff),
            ],
        );
        let plan = make_plan(&content, 1.0);
        assert_eq!(plan.overlays.layers.len(), 2);
        assert!(plan.tiles.values().all(|tile| !tile.dynamic));
    }

    // One Raven patch: content culled above (the scroll window moved down)
    // leaves the keys of the tiles below it alone.
    #[test]
    fn content_dropped_above_leaves_the_keys_below() {
        let below = || {
            vec![
                rect(0.0, 600.0, 100.0, 100.0, 0x404040ff),
                rect(20.0, 620.0, 40.0, 40.0, 0x808080ff),
            ]
        };
        let mut with_top = vec![rect(0.0, 0.0, 100.0, 100.0, 0x202020ff)];
        with_top.extend(below());
        let before = make_plan(&layer(1, PaintLayerReason::ScrollContent, with_top), 1.0);
        let after = make_plan(&layer(1, PaintLayerReason::ScrollContent, below()), 1.0);
        assert_eq!(before.tiles[&(0, 2)].key, after.tiles[&(0, 2)].key);
        // Order within a tile still counts.
        let mut swapped = below();
        swapped.reverse();
        let swapped = make_plan(&layer(1, PaintLayerReason::ScrollContent, swapped), 1.0);
        assert_ne!(after.tiles[&(0, 2)].key, swapped.tiles[&(0, 2)].key);
    }

    // One Raven patch: a new scene takes the render thread's advanced copy
    // of the same build, and only that.
    #[test]
    fn a_new_scene_adopts_the_advanced_copy_of_its_scroll_content() {
        let content = layer(
            1,
            PaintLayerReason::ScrollContent,
            vec![rect(0.0, 0.0, 100.0, 100.0, 0x202020ff), spinner(10.0, 10.0)],
        );
        let page = |content: RenderPaintLayer| {
            vec![RenderNode::PaintLayer(layer(
                7,
                PaintLayerReason::Root,
                vec![RenderNode::Transform {
                    transform: Affine2::translation(0.0, -50.0),
                    children: vec![RenderNode::PaintLayer(content)],
                }],
            ))]
        };
        let mut advanced = content.clone();
        advanced.content = Arc::new((*content.content).clone());
        let old = page(advanced.clone());
        let mut new = page(content.clone());
        crate::render_scene::adopt_advanced_scroll_content(&mut new, &old);
        let adopted = |nodes: &[RenderNode]| {
            let RenderNode::PaintLayer(root) = &nodes[0] else {
                unreachable!()
            };
            let mut found = None;
            fn find(
                nodes: &[RenderPaintLayerContentNode],
                found: &mut Option<Arc<RenderPaintLayerContent>>,
            ) {
                for node in nodes {
                    match node {
                        RenderPaintLayerContentNode::Child(layer)
                            if layer.id.role == PaintLayerReason::ScrollContent =>
                        {
                            *found = Some(Arc::clone(&layer.content));
                        }
                        RenderPaintLayerContentNode::Transform { children, .. }
                        | RenderPaintLayerContentNode::Clip { children, .. } => {
                            find(children, found)
                        }
                        _ => {}
                    }
                }
            }
            find(&root.content.nodes, &mut found);
            found.expect("scroll content")
        };
        assert!(Arc::ptr_eq(&adopted(&new), &advanced.content));
        // A different build is left alone.
        let rebuilt = layer(1, PaintLayerReason::ScrollContent, vec![rect(0.0, 0.0, 100.0, 100.0, 0x202020ff)]);
        let mut other = page(rebuilt.clone());
        crate::render_scene::adopt_advanced_scroll_content(&mut other, &old);
        assert!(Arc::ptr_eq(&adopted(&other), &rebuilt.content));
    }

    // One Raven patch: the overlay runs of a copy of the same build are found
    // again without a new plan.
    #[test]
    fn overlay_runs_follow_a_copy_of_the_same_build() {
        // The spinner inside a run, as a transformed or clipped one is.
        let mut content = layer(1, PaintLayerReason::ScrollContent, Vec::new());
        content.content = Arc::new(RenderPaintLayerContent::new(vec![
            RenderPaintLayerContentNode::Own(crate::render_scene::RenderPaintRun {
                slot: 0,
                bounds: GeometryRect::default(),
                nodes: Arc::new(vec![
                    rect(0.0, 0.0, 1000.0, 600.0, 0x202020ff),
                    spinner(100.0, 100.0),
                ]),
                metrics: Default::default(),
            }),
        ]));
        let plan = make_plan(&content, 1.0);
        assert_eq!(plan.overlays.runs.len(), 1);
        let mut copy = (*content.content).clone();
        for node in &mut copy.nodes {
            if let RenderPaintLayerContentNode::Own(run) = node {
                run.nodes = Arc::new((*run.nodes).clone());
            }
        }
        assert!(copy.source.same_build(content.content.source));
        let mut runs = HashSet::new();
        overlay_runs(&copy.nodes, None, &plan.overlays.layers, &mut runs, &mut HashSet::new());
        assert_eq!(runs.len(), plan.overlays.runs.len());
        assert!(runs.is_disjoint(&plan.overlays.runs));
    }
}
