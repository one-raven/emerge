// One Raven patch: compositor scrolling. Scroll content is built in content
// coordinates for a window wider than the viewport and placed with a
// translation; moving the offset within the window reuses the built layer.
use super::common::*;
use super::*;
use crate::render_scene::{PaintLayerReason, RenderNode, RenderPaintLayer, RenderScene};

fn scroll_layer(scene: &RenderScene) -> Option<RenderPaintLayer> {
    fn find(nodes: &[RenderNode]) -> Option<RenderPaintLayer> {
        nodes.iter().find_map(|node| match node {
            RenderNode::PaintLayer(layer) if layer.id.role == PaintLayerReason::ScrollContent => {
                Some(layer.clone())
            }
            RenderNode::PaintLayer(layer) => layer
                .content
                .nodes
                .iter()
                .find_map(|node| find_content(std::slice::from_ref(node))),
            RenderNode::ShadowPass { children }
            | RenderNode::Clip { children, .. }
            | RenderNode::RelaxedClip { children, .. }
            | RenderNode::Transform { children, .. }
            | RenderNode::Alpha { children, .. } => find(children),
            RenderNode::Primitive(_) => None,
        })
    }
    fn find_content(
        nodes: &[crate::render_scene::RenderPaintLayerContentNode],
    ) -> Option<RenderPaintLayer> {
        use crate::render_scene::RenderPaintLayerContentNode as C;
        nodes.iter().find_map(|node| match node {
            C::Own(run) => find(&run.nodes),
            C::Child(layer) if layer.id.role == PaintLayerReason::ScrollContent => {
                Some(layer.clone())
            }
            C::Child(layer) => find_content(&layer.content.nodes),
            C::ShadowPass { children }
            | C::Clip { children, .. }
            | C::RelaxedClip { children, .. }
            | C::Transform { children, .. }
            | C::Alpha { children, .. } => find_content(children),
        })
    }
    find(&scene.nodes)
}

fn scene(tree: &ElementTree) -> RenderScene {
    super::super::render_tree_scene_with_scroll_layers(tree).scene
}

fn pixels(tree: &ElementTree, width: u32, height: u32) -> Vec<u8> {
    render_scene_to_pixels(width, height, scene(tree))
}

fn set_offset(tree: &mut ElementTree, id: NodeId, offset: f32) {
    let max = tree.get(&id).unwrap().layout.scroll_y_max;
    let invalidation = tree.set_scroll_position(&id, [0.0, f64::from(offset / max)]);
    assert!(invalidation.is_dirty() || tree.get(&id).unwrap().layout.scroll_y == offset);
}

fn rows_root() -> NodeId {
    NodeId::from_u64(800_000)
}

#[test]
fn compositor_scroll_draws_what_viewport_culling_draws() {
    for offset in [0.0, 130.0, 400.0, 517.0, 800.0] {
        let mut tree = super::canonical::screen();
        set_offset(&mut tree, NodeId::from_u64(2), offset);
        let compositor = pixels(&tree, 360, 640);
        tree.compositor_scroll = false;
        let viewport = pixels(&tree, 360, 640);
        assert!(compositor == viewport, "screen at offset {offset}");
    }
    for offset in [0.0, 37.0, 500.0, 1000.0] {
        let mut tree = super::pipeline::build_manual_scroll_row_tree(120);
        set_offset(&mut tree, rows_root(), offset);
        let compositor = pixels(&tree, 100, 60);
        tree.compositor_scroll = false;
        let viewport = pixels(&tree, 100, 60);
        assert!(compositor == viewport, "rows at offset {offset}");
    }
}

#[test]
fn a_scroll_within_the_window_reuses_the_content_layer() {
    let mut tree = super::pipeline::build_manual_scroll_row_tree(120);
    let before = scroll_layer(&scene(&tree)).expect("scroll layer");
    tree.clear_refresh_dirty();

    set_offset(&mut tree, rows_root(), 510.0);
    super::super::reset_render_traversal_diagnostics_for_benchmark();
    let after = scene(&tree);
    let diagnostics = super::super::take_render_traversal_diagnostics_for_benchmark();

    let layer = scroll_layer(&after).expect("scroll layer");
    assert!(std::sync::Arc::ptr_eq(&before.content, &layer.content));
    // The container only: no row is visited.
    assert!(diagnostics.element_visits <= 1, "{diagnostics:?}");
    let expected = {
        let mut tree = super::pipeline::build_manual_scroll_row_tree(120);
        tree.compositor_scroll = false;
        set_offset(&mut tree, rows_root(), 510.0);
        pixels(&tree, 100, 60)
    };
    assert!(render_scene_to_pixels(100, 60, after) == expected);
}

#[test]
fn moving_the_window_builds_only_what_enters_it() {
    let mut tree = super::pipeline::build_manual_scroll_row_tree(120);
    super::super::reset_render_traversal_diagnostics_for_benchmark();
    let _ = scene(&tree);
    let cold = super::super::take_render_traversal_diagnostics_for_benchmark();
    tree.clear_refresh_dirty();

    // Two window steps (64 px each here): the window moves from rows 39..61
    // to 52..74, so rows 53..60 stay wholly inside it.
    set_offset(&mut tree, rows_root(), 628.0);
    super::super::reset_render_traversal_diagnostics_for_benchmark();
    let warm_scene = scene(&tree);
    let warm = super::super::take_render_traversal_diagnostics_for_benchmark();
    assert!(cold.element_visits >= 20, "{cold:?}");
    // Only the container, the column straddling the window and the rows that
    // entered it are built; rows that stay inside it are reused.
    let built = warm.element_visits - warm.fragment_reuses;
    assert_eq!(warm.fragment_reuses, 8, "cold {cold:?}, warm {warm:?}");
    assert!(built <= 17, "cold {cold:?}, warm {warm:?}");

    let expected = {
        let mut tree = super::pipeline::build_manual_scroll_row_tree(120);
        tree.compositor_scroll = false;
        set_offset(&mut tree, rows_root(), 628.0);
        pixels(&tree, 100, 60)
    };
    assert!(render_scene_to_pixels(100, 60, warm_scene) == expected);
}

#[test]
fn a_change_in_the_content_rebuilds_the_layer() {
    let mut tree = super::pipeline::build_manual_scroll_row_tree(120);
    let before = scroll_layer(&scene(&tree)).expect("scroll layer");
    tree.clear_refresh_dirty();

    let row = NodeId::from_u64(801_052);
    tree.get_mut(&row).unwrap().layout.effective = solid_fill_attrs((255, 0, 0)).into();
    tree.mark_render_and_registry_refresh_dirty(&row);
    let after = scene(&tree);
    let layer = scroll_layer(&after).expect("scroll layer");
    assert!(!std::sync::Arc::ptr_eq(&before.content, &layer.content));
    // Row 52 is at 520..530, 19.5..29.5 on screen at offset 500.
    let pixels = render_scene_to_pixels(100, 60, after);
    assert_eq!(rgba_at(&pixels, 100, 50, 25), (255, 0, 0, 255));
}

#[test]
fn a_scroll_marks_the_registry_only_when_one_is_built() {
    let mut tree = super::pipeline::build_manual_scroll_row_tree(120);
    let _ = scene(&tree);
    tree.clear_refresh_dirty();
    set_offset(&mut tree, rows_root(), 510.0);
    assert!(tree.has_render_refresh_damage());
    assert!(!tree.has_registry_refresh_damage());
    assert!(tree.has_deferred_scroll_registry());
    tree.flush_deferred_scroll_registry();
    assert!(tree.has_registry_refresh_damage());
    assert!(!tree.has_deferred_scroll_registry());
}

// Scroll tiles (renderer/scroll_tiles.rs), on a CPU surface here.

fn tiled_renderer() -> crate::renderer::SceneRenderer {
    let mut renderer =
        crate::renderer::SceneRenderer::with_cache_config(crate::renderer::RendererCacheConfig {
            enabled: true,
            ..crate::renderer::RendererCacheConfig::default()
        });
    renderer.set_scroll_tiles_on_cpu(true);
    renderer
}

fn assert_close(actual: &[u8], expected: &[u8], what: &str) {
    assert_eq!(actual.len(), expected.len());
    let worst = actual
        .iter()
        .zip(expected)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap_or(0);
    // Text and edges are blended onto a transparent tile, then composited.
    assert!(worst <= 3, "{what}: max channel difference {worst}");
}

#[test]
fn tiles_draw_what_the_content_draws_as_it_scrolls() {
    let mut renderer = tiled_renderer();
    let mut tree = super::canonical::screen();
    for offset in [130.0, 131.0, 200.0, 517.0, 800.0, 130.0] {
        set_offset(&mut tree, NodeId::from_u64(2), offset);
        let tiled = super::pipeline::render_scene_with_renderer_to_pixels_and_timings(
            &mut renderer,
            scene(&tree),
            360,
            640,
        )
        .0;
        let direct = pixels(&tree, 360, 640);
        assert_close(&tiled, &direct, &format!("offset {offset}"));
    }
    let counts = renderer.scroll_tile_counts();
    assert!(counts.drawn > 0 && counts.fallbacks == 0, "{counts:?}");
}

#[test]
fn a_scroll_rasterizes_only_tiles_that_come_into_view() {
    let mut renderer = tiled_renderer();
    let mut tree = super::canonical::screen();
    set_offset(&mut tree, NodeId::from_u64(2), 130.0);
    let draw = |renderer: &mut crate::renderer::SceneRenderer, tree: &ElementTree| {
        super::pipeline::render_scene_with_renderer_to_pixels_and_timings(
            renderer,
            scene(tree),
            360,
            640,
        )
    };
    let _ = draw(&mut renderer, &tree);
    let first = renderer.scroll_tile_counts();
    assert!(first.rasterized > 0);
    // A few pixels: the same tiles, placed lower.
    set_offset(&mut tree, NodeId::from_u64(2), 134.0);
    let _ = draw(&mut renderer, &tree);
    let second = renderer.scroll_tile_counts();
    assert_eq!(second.rasterized, first.rasterized, "{first:?} {second:?}");
    // A whole tile row further: at most one new row (one tile wide here).
    set_offset(&mut tree, NodeId::from_u64(2), 134.0 + 256.0);
    let _ = draw(&mut renderer, &tree);
    let third = renderer.scroll_tile_counts();
    assert!(
        third.rasterized - second.rasterized <= 2,
        "{second:?} {third:?}"
    );
}

#[test]
fn a_change_rasterizes_only_the_tiles_it_touches() {
    let mut renderer = tiled_renderer();
    let mut tree = super::pipeline::build_manual_scroll_row_tree(120);
    let draw = |renderer: &mut crate::renderer::SceneRenderer, tree: &ElementTree| {
        super::pipeline::render_scene_with_renderer_to_pixels_and_timings(
            renderer,
            scene(tree),
            100,
            60,
        )
        .0
    };
    let _ = draw(&mut renderer, &tree);
    tree.clear_refresh_dirty();
    let before = renderer.scroll_tile_counts();
    let row = NodeId::from_u64(801_052);
    tree.get_mut(&row).unwrap().layout.effective = solid_fill_attrs((255, 0, 0)).into();
    tree.mark_render_and_registry_refresh_dirty(&row);
    let pixels = draw(&mut renderer, &tree);
    let after = renderer.scroll_tile_counts();
    assert_eq!(after.plans, before.plans + 1);
    assert_eq!(
        after.rasterized,
        before.rasterized + 1,
        "{before:?} {after:?}"
    );
    assert_eq!(rgba_at(&pixels, 100, 50, 25), (255, 0, 0, 255));
}
