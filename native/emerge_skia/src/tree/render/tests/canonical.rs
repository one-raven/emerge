// One Raven patch: scroll content is built canonical in place (see
// `ScrollCanon`). It must equal building the content raw and canonicalizing it
// at the container, for every kind of element the content can hold.
use super::common::solid_fill_attrs;
use super::*;
use crate::render_scene::RenderScene;

fn element(id: u64, kind: ElementKind, attrs: Attrs, frame: Frame) -> Element {
    let mut element = Element::with_attrs(NodeId::from_u64(id), kind, Vec::new(), attrs);
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

fn shadow() -> Option<Vec<BoxShadow>> {
    Some(vec![BoxShadow {
        offset_x: 0.0,
        offset_y: 2.0,
        blur: 6.0,
        size: 0.0,
        color: Color::Rgb { r: 0, g: 0, b: 0 },
        inset: false,
    }])
}

const CHANGED_ROW: u64 = 120;

/// A scrolled body holding a page of rows of every shape: plain, rounded and
/// shadowed, see-through, rotated, an image, a nearby overlay, and a nested
/// scrolled list.
fn screen() -> ElementTree {
    let mut tree = ElementTree::new();
    let root = NodeId::from_u64(1);
    tree.set_root_id(root);
    let insert = |tree: &mut ElementTree, parent: Option<u64>, el: Element| {
        let id = el.id;
        tree.insert(el);
        if let Some(parent) = parent {
            let parent = NodeId::from_u64(parent);
            let mut children = tree.child_ids(&parent);
            children.push(id);
            tree.set_children(&parent, children).unwrap();
        }
    };
    insert(
        &mut tree,
        None,
        element(
            1,
            ElementKind::El,
            solid_fill_attrs((8, 8, 8)),
            frame(0.0, 0.0, 360.0, 640.0),
        ),
    );
    let mut body = element(
        2,
        ElementKind::El,
        Attrs {
            scrollbar_y: Some(true),
            ..solid_fill_attrs((12, 14, 20))
        },
        Frame {
            content_height: 1400.0,
            ..frame(0.0, 40.0, 360.0, 600.0)
        },
    );
    body.layout.scroll_y = 130.0;
    body.layout.scroll_y_max = 800.0;
    insert(&mut tree, Some(1), body);
    insert(
        &mut tree,
        Some(2),
        element(
            3,
            ElementKind::Column,
            Attrs::default(),
            frame(0.0, 40.0, 360.0, 1400.0),
        ),
    );

    let mut y = 48.0;
    let mut next_row = |height: f32| {
        let row = frame(8.0, y, 344.0, height);
        y += height + 8.0;
        row
    };
    for (index, id) in (100..110).enumerate() {
        let mut attrs = solid_fill_attrs((60, 70 + index as u8, 90));
        if index % 2 == 0 {
            attrs.border_radius = Some(BorderRadius::Uniform(8.0));
        }
        if index % 3 == 0 {
            attrs.box_shadows = shadow();
        }
        let row = next_row(40.0);
        insert(&mut tree, Some(3), element(id, ElementKind::El, attrs, row));
        // A child in each row, so rows are hosts too.
        insert(
            &mut tree,
            Some(id),
            element(
                id + 1000,
                ElementKind::El,
                solid_fill_attrs((200, 200, 200)),
                frame(row.x + 8.0, row.y + 8.0, 24.0, 24.0),
            ),
        );
    }
    // The row the incremental cases change: a plain host with a child.
    let row = next_row(40.0);
    insert(
        &mut tree,
        Some(3),
        element(
            CHANGED_ROW,
            ElementKind::El,
            solid_fill_attrs((1, 2, 3)),
            row,
        ),
    );
    insert(
        &mut tree,
        Some(CHANGED_ROW),
        element(
            CHANGED_ROW + 1000,
            ElementKind::El,
            solid_fill_attrs((4, 5, 6)),
            frame(row.x + 4.0, row.y + 4.0, 20.0, 20.0),
        ),
    );
    // See-through, with a shadowed child.
    let row = next_row(50.0);
    insert(
        &mut tree,
        Some(3),
        element(
            130,
            ElementKind::El,
            Attrs {
                alpha: Some(0.5),
                ..solid_fill_attrs((90, 30, 30))
            },
            row,
        ),
    );
    insert(
        &mut tree,
        Some(130),
        element(
            1130,
            ElementKind::El,
            Attrs {
                box_shadows: shadow(),
                ..solid_fill_attrs((30, 90, 30))
            },
            frame(row.x + 10.0, row.y + 10.0, 30.0, 30.0),
        ),
    );
    // Rotated, with a child.
    let row = next_row(50.0);
    insert(
        &mut tree,
        Some(3),
        element(
            140,
            ElementKind::El,
            Attrs {
                rotate: Some(10.0),
                ..solid_fill_attrs((30, 30, 90))
            },
            row,
        ),
    );
    insert(
        &mut tree,
        Some(140),
        element(
            1140,
            ElementKind::El,
            solid_fill_attrs((90, 90, 30)),
            frame(row.x + 10.0, row.y + 10.0, 30.0, 30.0),
        ),
    );
    // An image without its own box.
    let row = next_row(40.0);
    insert(
        &mut tree,
        Some(3),
        element(150, ElementKind::Image, Attrs::default(), row),
    );
    // A host with an overlay in front.
    let row = next_row(40.0);
    insert(
        &mut tree,
        Some(3),
        element(160, ElementKind::El, solid_fill_attrs((50, 50, 50)), row),
    );
    tree.insert(element(
        161,
        ElementKind::El,
        solid_fill_attrs((250, 0, 0)),
        frame(row.x + 300.0, row.y - 10.0, 30.0, 20.0),
    ));
    tree.set_nearby_mounts(
        &NodeId::from_u64(160),
        vec![NearbyMount {
            slot: NearbySlot::InFront,
            id: NodeId::from_u64(161),
        }],
    )
    .unwrap();
    // A nested scrolled list.
    let list_frame = next_row(200.0);
    let mut list = element(
        170,
        ElementKind::Column,
        Attrs {
            scrollbar_y: Some(true),
            ..solid_fill_attrs((24, 28, 40))
        },
        Frame {
            content_height: 600.0,
            ..list_frame
        },
    );
    list.layout.scroll_y = 50.0;
    list.layout.scroll_y_max = 400.0;
    insert(&mut tree, Some(3), list);
    for index in 0..12u8 {
        let mut attrs = solid_fill_attrs((40, 40 + index, 40));
        if index % 4 == 0 {
            attrs.box_shadows = shadow();
        }
        insert(
            &mut tree,
            Some(170),
            element(
                200 + u64::from(index),
                ElementKind::El,
                attrs,
                frame(
                    list_frame.x + 4.0,
                    list_frame.y + f32::from(index) * 48.0,
                    300.0,
                    44.0,
                ),
            ),
        );
    }
    tree
}

fn change(tree: &mut ElementTree, id: u64) {
    let id = NodeId::from_u64(id);
    tree.get_mut(&id).expect("node").layout.effective = solid_fill_attrs((7, 7, 7)).into();
    tree.mark_render_and_registry_refresh_dirty(&id);
}

/// The scene of a cold build, then of a rebuild after `changed` changes.
fn scenes(after_build: bool, changed: u64) -> (RenderScene, RenderScene, usize) {
    super::super::set_canonicalize_scroll_content_after_build(after_build);
    let mut tree = screen();
    let cold = super::super::render_tree_scene_with_scroll_layers(&tree).scene;
    tree.clear_refresh_dirty();
    change(&mut tree, changed);
    let _ = super::super::take_canonicalized_nodes();
    let warm = super::super::render_tree_scene_with_scroll_layers(&tree).scene;
    let canonicalized = super::super::take_canonicalized_nodes();
    super::super::set_canonicalize_scroll_content_after_build(false);
    (cold, warm, canonicalized)
}

#[test]
fn scroll_content_built_canonical_equals_canonicalizing_it_raw() {
    for changed in [CHANGED_ROW, CHANGED_ROW + 1000, 1130, 1140, 161, 205, 3] {
        let (cold, warm, _) = scenes(false, changed);
        let (reference_cold, reference_warm, _) = scenes(true, changed);
        assert_eq!(cold.nodes, reference_cold.nodes, "cold build");
        assert_eq!(warm.nodes, reference_warm.nodes, "after changing {changed}");
    }
}

#[test]
fn a_changed_row_canonicalizes_only_itself() {
    let (_, _, in_place) = scenes(false, CHANGED_ROW + 1000);
    let (_, _, reference) = scenes(true, CHANGED_ROW + 1000);
    // Built in place, only the changed row's own nodes are stripped (its
    // background and the clip around it); the reference strips every node of
    // the page again (62 here).
    assert!(reference > 50, "{reference}");
    assert!(in_place <= 2, "{in_place}");
}

/// An overlay (a nearby, as the hub's panel mounts its page) holding a
/// scrolled list.
fn overlay_with_list() -> ElementTree {
    let mut tree = ElementTree::new();
    tree.set_root_id(NodeId::from_u64(1));
    tree.insert(element(
        1,
        ElementKind::El,
        solid_fill_attrs((8, 8, 8)),
        frame(0.0, 0.0, 360.0, 640.0),
    ));
    tree.insert(element(
        2,
        ElementKind::El,
        solid_fill_attrs((20, 20, 20)),
        frame(0.0, 0.0, 360.0, 640.0),
    ));
    let mut list = element(
        3,
        ElementKind::Column,
        Attrs {
            scrollbar_y: Some(true),
            ..solid_fill_attrs((24, 28, 40))
        },
        Frame {
            content_height: 2000.0,
            ..frame(0.0, 40.0, 360.0, 600.0)
        },
    );
    list.layout.scroll_y = 300.0;
    list.layout.scroll_y_max = 1400.0;
    tree.insert(list);
    let rows: Vec<NodeId> = (0..40u8)
        .map(|index| {
            let id = 100 + u64::from(index);
            let mut attrs = solid_fill_attrs((40, 40 + index, 40));
            attrs.border_radius = Some(BorderRadius::Uniform(6.0));
            tree.insert(element(
                id,
                ElementKind::El,
                attrs,
                frame(8.0, 40.0 + f32::from(index) * 50.0, 344.0, 44.0),
            ));
            NodeId::from_u64(id)
        })
        .collect();
    tree.set_children(&NodeId::from_u64(3), rows).unwrap();
    tree.set_children(&NodeId::from_u64(2), vec![NodeId::from_u64(3)])
        .unwrap();
    tree.set_nearby_mounts(
        &NodeId::from_u64(1),
        vec![NearbyMount {
            slot: NearbySlot::InFront,
            id: NodeId::from_u64(2),
        }],
    )
    .unwrap();
    tree
}

#[test]
fn an_overlay_does_not_rebuild_a_changed_list_layer_it_has_nothing_to_strip_from() {
    let mut tree = overlay_with_list();
    let _ = super::super::render_tree_scene_with_scroll_layers(&tree);
    tree.clear_refresh_dirty();
    change(&mut tree, 110);
    let _ = super::super::take_canonicalized_nodes();
    let warm = super::super::render_tree_scene_with_scroll_layers(&tree).scene;
    // The row's own nodes and the overlay's top level; the list's content
    // is not copied out and stripped again (88 when it is).
    let canonicalized = super::super::take_canonicalized_nodes();
    assert!(canonicalized <= 8, "{canonicalized}");

    let mut cold_tree = overlay_with_list();
    change(&mut cold_tree, 110);
    let cold = super::super::render_tree_scene_with_scroll_layers(&cold_tree).scene;
    assert_eq!(warm.nodes, cold.nodes);
}
