// One Raven patch: a small change inside a big tree costs what changed, not
// the tree. The screen: a scroll body (as the hub's panel has) holding a
// badge and a scroll list of rows.
use super::common::solid_fill_attrs;
use super::*;

const ROWS: u8 = 30;

struct Screen {
    tree: ElementTree,
    badge_id: NodeId,
}

fn element(id: NodeId, kind: ElementKind, attrs: Attrs, frame: Frame) -> Element {
    let mut element = Element::with_attrs(id, kind, Vec::new(), attrs);
    element.layout.frame = Some(frame);
    element
}

fn frame(x: f32, y: f32, width: f32, height: f32) -> Frame {
    Frame {
        x,
        y,
        width,
        height,
        content_width: width,
        content_height: height,
    }
}

fn scroll_attrs(rgb: (u8, u8, u8)) -> Attrs {
    Attrs {
        scrollbar_y: Some(true),
        ..solid_fill_attrs(rgb)
    }
}

fn screen(badge_rgb: (u8, u8, u8)) -> Screen {
    let root_id = NodeId::from_u64(70_000);
    let body_id = NodeId::from_u64(70_001);
    let page_id = NodeId::from_u64(70_002);
    let badge_id = NodeId::from_u64(70_003);
    let list_id = NodeId::from_u64(70_004);

    let mut tree = ElementTree::new();
    tree.set_root_id(root_id);
    let mut root = element(
        root_id,
        ElementKind::El,
        solid_fill_attrs((8, 8, 8)),
        frame(0.0, 0.0, 360.0, 640.0),
    );
    root.children = vec![body_id];
    tree.insert(root);

    let mut body = element(
        body_id,
        ElementKind::El,
        scroll_attrs((12, 14, 20)),
        frame(0.0, 0.0, 360.0, 640.0),
    );
    body.children = vec![page_id];
    tree.insert(body);

    let mut page = element(
        page_id,
        ElementKind::Column,
        Attrs::default(),
        frame(0.0, 0.0, 360.0, 640.0),
    );
    page.children = vec![badge_id, list_id];
    tree.insert(page);

    tree.insert(element(
        badge_id,
        ElementKind::El,
        solid_fill_attrs(badge_rgb),
        frame(16.0, 16.0, 80.0, 24.0),
    ));

    let mut list = element(
        list_id,
        ElementKind::Column,
        scroll_attrs((24, 28, 40)),
        Frame {
            content_height: f32::from(ROWS) * 40.0,
            ..frame(0.0, 56.0, 360.0, 584.0)
        },
    );
    list.layout.scroll_y_max = f32::from(ROWS) * 40.0 - 584.0;
    list.children = (0..ROWS)
        .map(|index| {
            let row_id = NodeId::from_u64(71_000 + u64::from(index));
            let mut attrs = solid_fill_attrs((60, 70 + index, 90));
            attrs.border_radius = Some(BorderRadius::Uniform(8.0));
            // Shadows escape their row's clip; every third row has one.
            if index % 3 == 0 {
                attrs.box_shadows = Some(vec![BoxShadow {
                    offset_x: 0.0,
                    offset_y: 2.0,
                    blur: 6.0,
                    size: 0.0,
                    color: Color::Rgb { r: 0, g: 0, b: 0 },
                    inset: false,
                }]);
            }
            tree.insert(element(
                row_id,
                ElementKind::El,
                attrs,
                frame(8.0, 56.0 + f32::from(index) * 40.0, 344.0, 36.0),
            ));
            row_id
        })
        .collect();
    tree.insert(list);

    Screen { tree, badge_id }
}

#[test]
fn a_change_beside_a_clean_scroll_list_reuses_the_lists_canonical_layer() {
    let Screen {
        mut tree,
        badge_id,
    } = screen((200, 40, 40));
    let _ = super::super::render_tree_scene_with_scroll_layers(&tree);
    tree.clear_refresh_dirty();
    let _ = super::super::render_tree_scene_with_scroll_layers(&tree);
    let _ = super::super::take_layer_rewrites_computed();

    tree.get_mut(&badge_id).expect("badge").layout.effective = solid_fill_attrs((40, 200, 40)).into();
    tree.mark_render_and_registry_refresh_dirty(&badge_id);
    super::super::reset_render_traversal_diagnostics_for_benchmark();
    let changed = super::super::render_tree_scene_with_scroll_layers(&tree).scene;
    let diagnostics = super::super::take_render_traversal_diagnostics_for_benchmark();
    // Root, body, page and badge rebuild; the list is a reused fragment (a
    // visit) and its rows aren't visited.
    assert_eq!(diagnostics.element_visits, 5, "{diagnostics:?}");

    // The body strips its inherited clips from, and offsets, the list's
    // layer: both are reused from the last refresh.
    assert_eq!(super::super::take_layer_rewrites_computed(), 0);

    // And the result is what a cold build of the changed screen gives.
    let Screen { tree: fresh, .. } = screen((40, 200, 40));
    let cold = super::super::render_tree_scene_with_scroll_layers(&fresh).scene;
    assert!(super::super::take_layer_rewrites_computed() > 0);
    assert_eq!(changed.nodes, cold.nodes);
}

#[test]
fn a_changed_list_is_canonicalized_again() {
    let Screen { mut tree, .. } = screen((200, 40, 40));
    let _ = super::super::render_tree_scene_with_scroll_layers(&tree);
    tree.clear_refresh_dirty();
    let _ = super::super::take_layer_rewrites_computed();

    let row_id = NodeId::from_u64(71_003);
    tree.get_mut(&row_id).expect("row").layout.effective = solid_fill_attrs((1, 2, 3)).into();
    tree.mark_render_and_registry_refresh_dirty(&row_id);
    let changed = super::super::render_tree_scene_with_scroll_layers(&tree).scene;
    assert!(super::super::take_layer_rewrites_computed() > 0);

    let mut cold_tree = screen((200, 40, 40)).tree;
    cold_tree.get_mut(&row_id).expect("row").layout.effective = solid_fill_attrs((1, 2, 3)).into();
    let cold = super::super::render_tree_scene_with_scroll_layers(&cold_tree).scene;
    assert_eq!(changed.nodes, cold.nodes);
}
