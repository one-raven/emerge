//! One Raven patch: the render thread carries a fling on while the tree is
//! busy.
//!
//! A fling is a fixed curve from the moment of release (`AdaptiveScrollSimulation`
//! in events/runtime.rs), so where the content should be at any instant is
//! known in advance. The event runtime shares the running fling here
//! (`ScrollFlingSlot`) and, after each scroll step it sends the tree, tells the
//! tree which point on the curve that step reached. The tree stamps every
//! scene with an anchor: the container's offset as placed in that scene, the
//! curve position it reflects, and the offsets the built content window can
//! show (`ScrollFlingAnchor`). Each frame the render thread works out the
//! offset for the frame's scanout from the anchor and the curve, and puts the
//! container's content layer there (`place`), so a tree refresh that takes
//! 80 ms no longer freezes the scroll. A scroller nested in another's content
//! (a list under a page that scrolls) is carried the same way. The tree keeps supplying content as
//! before; the window it builds reaches well past the viewport, and the offset
//! is clamped to it.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::render_scene::{
    PaintLayerId, PaintLayerReason, RenderNode, RenderPaintLayer, RenderPaintLayerContentNode,
};

/// `ADAPTIVE_SCROLL_DECELERATION_RATE` in events/runtime.rs.
pub(crate) const DECELERATION_RATE: f32 = 2.358_201_7;

/// A running fling, as the event runtime started it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollFling {
    /// Distinct for every fling.
    pub id: u64,
    /// The scroll container (wire id).
    pub node: u64,
    pub axis_y: bool,
    pub started_at: Instant,
    /// Curve position (scroll request units: positive moves toward the start).
    pub distance: f32,
    pub duration_secs: f32,
}

impl ScrollFling {
    /// The curve position at `at`, as `AdaptiveScrollSimulation::x`.
    pub fn position(&self, at: Instant) -> f32 {
        let elapsed = at.saturating_duration_since(self.started_at).as_secs_f32();
        let t = (elapsed / self.duration_secs).clamp(0.0, 1.0);
        self.distance * (1.0 - (1.0 - t).powf(DECELERATION_RATE))
    }

    pub fn done(&self, at: Instant) -> bool {
        at.saturating_duration_since(self.started_at).as_secs_f32() >= self.duration_secs
    }
}

/// Where a fling stands in one scene.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollFlingAnchor {
    pub fling: u64,
    pub node: u64,
    pub axis_y: bool,
    /// The container's offset as placed in this scene (whole pixels).
    pub offset: f32,
    /// The curve position that offset reflects.
    pub sample: f32,
    /// The offsets the scene can show: the built window and the scroll range.
    pub min: f32,
    pub max: f32,
}

impl ScrollFlingAnchor {
    /// The offset for `at`, if `fling` is the one this scene is anchored to.
    pub fn offset_at(&self, fling: &ScrollFling, at: Instant) -> Option<f32> {
        if fling.id != self.fling || fling.node != self.node || fling.axis_y != self.axis_y {
            return None;
        }
        let offset = self.offset - (fling.position(at) - self.sample);
        Some(offset.clamp(self.min, self.max.max(self.min)).round())
    }
}

/// The running fling, shared by the event runtime and the presenter.
#[cfg_attr(not(all(feature = "drm", target_os = "linux")), allow(dead_code))]
pub(crate) type ScrollFlingSlot = Arc<Mutex<Option<ScrollFling>>>;

#[cfg_attr(not(all(feature = "drm", target_os = "linux")), allow(dead_code))]
pub(crate) fn slot() -> ScrollFlingSlot {
    Arc::new(Mutex::new(None))
}

#[cfg_attr(not(all(feature = "drm", target_os = "linux")), allow(dead_code))]
pub(crate) fn current(slot: &ScrollFlingSlot) -> Option<ScrollFling> {
    slot.lock().ok().and_then(|fling| *fling)
}

#[cfg_attr(not(all(feature = "drm", target_os = "linux")), allow(dead_code))]
pub(crate) fn set(slot: &ScrollFlingSlot, fling: Option<ScrollFling>) {
    if let Ok(mut current) = slot.lock() {
        *current = fling;
    }
}

/// What one frame did with the fling.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlingAdvance {
    /// A fling is being carried on: keep drawing frames.
    pub active: bool,
    /// This frame moved the content.
    pub changed: bool,
}

/// The presenter's side: moves the scene it holds, frame to frame.
#[derive(Debug, Default)]
pub struct Carrier;

impl Carrier {
    /// Put the flinging container of `scene` where `fling` puts it at `at`.
    /// With no fling (it ended, or a touch cancelled it), it goes back to the
    /// offset the tree placed, which is what the tree hit-tests against.
    ///
    /// The offset is set, not moved by a difference from what the scene was
    /// built with: the renderer may hand the next scene content this already
    /// moved (`adopt_advanced_scroll_content`), and a nested scroller's
    /// placement is inside the content of the one around it.
    pub fn advance(
        &mut self,
        scene: &mut crate::render_scene::RenderScene,
        fling: Option<ScrollFling>,
        at: Instant,
    ) -> FlingAdvance {
        let Some(anchor) = scene.scroll_fling else {
            return FlingAdvance::default();
        };
        let Some(shown) = shown_offset(&scene.nodes, anchor.node, anchor.axis_y) else {
            return FlingAdvance::default();
        };
        let (target, active) = match fling.and_then(|fling| anchor.offset_at(&fling, at)) {
            Some(target) => (target, true),
            None => (anchor.offset, false),
        };
        let changed = shown != target && place(&mut scene.nodes, anchor.node, anchor.axis_y, target);
        // Past the end of its curve, a fling that moved nothing needs no more
        // frames.
        let finished = fling.is_none_or(|fling| fling.done(at)) && !changed;
        FlingAdvance {
            active: active && !finished,
            changed,
        }
    }
}

/// Put the content layer of scroll container `node` at `offset` px along the
/// axis. Only the translation that places the layer changes; everything on
/// the way to it is copied on write.
pub fn place(nodes: &mut [RenderNode], node: u64, axis_y: bool, offset: f32) -> bool {
    let id = PaintLayerId::new(node, PaintLayerReason::ScrollContent);
    place_nodes(nodes, id, axis_y, offset)
}

/// The offset the content layer of scroll container `node` is placed at.
pub fn shown_offset(nodes: &[RenderNode], node: u64, axis_y: bool) -> Option<f32> {
    let id = PaintLayerId::new(node, PaintLayerReason::ScrollContent);
    nodes_offset(nodes, id, axis_y)
}

fn nodes_offset(nodes: &[RenderNode], id: PaintLayerId, axis_y: bool) -> Option<f32> {
    nodes.iter().find_map(|node| match node {
        RenderNode::Transform {
            transform,
            children,
        } if matches!(children.as_slice(), [RenderNode::PaintLayer(layer)] if is_target(layer, id)) => {
            Some(offset_of(transform, axis_y))
        }
        RenderNode::ShadowPass { children }
        | RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => nodes_offset(children, id, axis_y),
        RenderNode::PaintLayer(layer) => content_offset(&layer.content.nodes, id, axis_y),
        RenderNode::Primitive(_) => None,
    })
}

fn content_offset(
    nodes: &[RenderPaintLayerContentNode],
    id: PaintLayerId,
    axis_y: bool,
) -> Option<f32> {
    nodes.iter().find_map(|node| match node {
        RenderPaintLayerContentNode::Transform {
            transform,
            children,
        } if matches!(children.as_slice(), [RenderPaintLayerContentNode::Child(layer)] if is_target(layer, id)) => {
            Some(offset_of(transform, axis_y))
        }
        RenderPaintLayerContentNode::Own(run) => nodes_offset(&run.nodes, id, axis_y),
        RenderPaintLayerContentNode::Child(layer) => {
            content_offset(&layer.content.nodes, id, axis_y)
        }
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => {
            content_offset(children, id, axis_y)
        }
    })
}

fn offset_of(transform: &crate::tree::transform::Affine2, axis_y: bool) -> f32 {
    -if axis_y { transform.ty } else { transform.tx }
}

fn is_target(layer: &RenderPaintLayer, id: PaintLayerId) -> bool {
    layer.id == id
}

fn set_offset(transform: &mut crate::tree::transform::Affine2, axis_y: bool, offset: f32) {
    if axis_y {
        transform.ty = -offset;
    } else {
        transform.tx = -offset;
    }
}

fn place_nodes(nodes: &mut [RenderNode], id: PaintLayerId, axis_y: bool, offset: f32) -> bool {
    for node in nodes.iter_mut() {
        let found = match node {
            RenderNode::Transform {
                transform,
                children,
            } if matches!(children.as_slice(), [RenderNode::PaintLayer(layer)] if is_target(layer, id)) =>
            {
                set_offset(transform, axis_y, offset);
                true
            }
            RenderNode::ShadowPass { children }
            | RenderNode::Clip { children, .. }
            | RenderNode::RelaxedClip { children, .. }
            | RenderNode::Transform { children, .. }
            | RenderNode::Alpha { children, .. } => place_nodes(children, id, axis_y, offset),
            RenderNode::PaintLayer(layer) => place_layer(layer, id, axis_y, offset),
            RenderNode::Primitive(_) => false,
        };
        if found {
            return true;
        }
    }
    false
}

fn place_layer(layer: &mut RenderPaintLayer, id: PaintLayerId, axis_y: bool, offset: f32) -> bool {
    if is_target(layer, id) || !content_holds(&layer.content.nodes, id) {
        return false;
    }
    place_content(
        &mut Arc::make_mut(&mut layer.content).nodes,
        id,
        axis_y,
        offset,
    )
}

fn place_content(
    nodes: &mut [RenderPaintLayerContentNode],
    id: PaintLayerId,
    axis_y: bool,
    offset: f32,
) -> bool {
    for node in nodes.iter_mut() {
        let found = match node {
            RenderPaintLayerContentNode::Transform {
                transform,
                children,
            } if matches!(children.as_slice(), [RenderPaintLayerContentNode::Child(layer)] if is_target(layer, id)) =>
            {
                set_offset(transform, axis_y, offset);
                true
            }
            RenderPaintLayerContentNode::Own(run) => {
                nodes_hold(&run.nodes, id)
                    && place_nodes(
                        Arc::make_mut(&mut run.nodes).as_mut_slice(),
                        id,
                        axis_y,
                        offset,
                    )
            }
            RenderPaintLayerContentNode::Child(layer) => place_layer(layer, id, axis_y, offset),
            RenderPaintLayerContentNode::ShadowPass { children }
            | RenderPaintLayerContentNode::Clip { children, .. }
            | RenderPaintLayerContentNode::RelaxedClip { children, .. }
            | RenderPaintLayerContentNode::Transform { children, .. }
            | RenderPaintLayerContentNode::Alpha { children, .. } => {
                place_content(children, id, axis_y, offset)
            }
        };
        if found {
            return true;
        }
    }
    false
}

fn nodes_hold(nodes: &[RenderNode], id: PaintLayerId) -> bool {
    nodes.iter().any(|node| match node {
        RenderNode::PaintLayer(layer) => {
            is_target(layer, id) || content_holds(&layer.content.nodes, id)
        }
        RenderNode::ShadowPass { children }
        | RenderNode::Clip { children, .. }
        | RenderNode::RelaxedClip { children, .. }
        | RenderNode::Transform { children, .. }
        | RenderNode::Alpha { children, .. } => nodes_hold(children, id),
        RenderNode::Primitive(_) => false,
    })
}

fn content_holds(nodes: &[RenderPaintLayerContentNode], id: PaintLayerId) -> bool {
    nodes.iter().any(|node| match node {
        RenderPaintLayerContentNode::Own(run) => nodes_hold(&run.nodes, id),
        RenderPaintLayerContentNode::Child(layer) => {
            is_target(layer, id) || content_holds(&layer.content.nodes, id)
        }
        RenderPaintLayerContentNode::ShadowPass { children }
        | RenderPaintLayerContentNode::Clip { children, .. }
        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
        | RenderPaintLayerContentNode::Transform { children, .. }
        | RenderPaintLayerContentNode::Alpha { children, .. } => content_holds(children, id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fling() -> ScrollFling {
        ScrollFling {
            id: 7,
            node: 42,
            axis_y: true,
            started_at: Instant::now(),
            distance: -3000.0,
            duration_secs: 2.0,
        }
    }

    fn anchor(offset: f32, sample: f32) -> ScrollFlingAnchor {
        ScrollFlingAnchor {
            fling: 7,
            node: 42,
            axis_y: true,
            offset,
            sample,
            min: 0.0,
            max: 2500.0,
        }
    }

    #[test]
    fn the_offset_follows_the_curve_from_the_anchor() {
        let fling = fling();
        let at = fling.started_at + Duration::from_millis(500);
        let position = fling.position(at);
        assert!(position < -1000.0 && position > -3000.0, "{position}");
        // Anchored at the start: offset 100 at curve position 0.
        let offset = anchor(100.0, 0.0).offset_at(&fling, at).unwrap();
        assert_eq!(offset, (100.0 - position).round());
        // Anchored later on the same curve: the same answer.
        let earlier = fling.started_at + Duration::from_millis(200);
        let then = anchor(100.0 - fling.position(earlier), fling.position(earlier));
        assert!((then.offset_at(&fling, at).unwrap() - offset).abs() <= 1.0);
    }

    #[test]
    fn the_offset_stays_inside_what_the_scene_can_show() {
        let fling = fling();
        let late = fling.started_at + Duration::from_secs(5);
        assert_eq!(anchor(100.0, 0.0).offset_at(&fling, late), Some(2500.0));
        let other = ScrollFling { id: 8, ..fling };
        assert_eq!(anchor(100.0, 0.0).offset_at(&other, late), None);
    }

    #[test]
    fn place_moves_only_the_flinging_layer() {
        use crate::render_scene::{
            PaintLayerPlacement, PaintLayerPolicy, RenderPaintLayerBuildParts,
            RenderPaintLayerContent,
        };
        use crate::tree::geometry::Rect;
        use crate::tree::transform::Affine2;
        let layer = |node: u64, role: PaintLayerReason, nodes: Vec<RenderNode>| {
            RenderNode::PaintLayer(RenderPaintLayer::from_prepared_children(
                RenderPaintLayerBuildParts {
                    id: PaintLayerId::new(node, role),
                    bounds: Rect::default(),
                    placement: PaintLayerPlacement::ScrollMoving,
                    policy: PaintLayerPolicy::Cacheable,
                    content_generation: 0,
                    visual_bounds: None,
                },
                RenderPaintLayerContent::from_composition_nodes(nodes),
            ))
        };
        let placed = |node: u64, offset: f32| RenderNode::Transform {
            transform: Affine2::translation(0.0, -offset),
            children: vec![layer(node, PaintLayerReason::ScrollContent, Vec::new())],
        };
        let mut nodes = vec![layer(
            1,
            PaintLayerReason::Root,
            vec![placed(41, 10.0), placed(42, 300.0)],
        )];
        assert!(place(&mut nodes, 42, true, 325.0));
        assert_eq!(shown_offset(&nodes, 42, true), Some(325.0));
        let translations: Vec<f32> = {
            let RenderNode::PaintLayer(root) = &nodes[0] else {
                unreachable!()
            };
            fn collect(nodes: &[RenderPaintLayerContentNode], out: &mut Vec<f32>) {
                for node in nodes {
                    match node {
                        RenderPaintLayerContentNode::Transform {
                            transform,
                            children,
                        } => {
                            out.push(transform.ty);
                            collect(children, out);
                        }
                        RenderPaintLayerContentNode::Own(run) => {
                            for node in run.nodes.iter() {
                                if let RenderNode::Transform { transform, .. } = node {
                                    out.push(transform.ty);
                                }
                            }
                        }
                        RenderPaintLayerContentNode::ShadowPass { children }
                        | RenderPaintLayerContentNode::Clip { children, .. }
                        | RenderPaintLayerContentNode::RelaxedClip { children, .. }
                        | RenderPaintLayerContentNode::Alpha { children, .. } => {
                            collect(children, out)
                        }
                        RenderPaintLayerContentNode::Child(_) => {}
                    }
                }
            }
            let mut out = Vec::new();
            collect(&root.content.nodes, &mut out);
            out
        };
        assert_eq!(translations, vec![-10.0, -325.0]);
        assert!(!place(&mut nodes, 99, true, 5.0));
        assert_eq!(shown_offset(&nodes, 99, true), None);
    }

    #[test]
    fn a_scroller_in_the_content_of_another_is_carried() {
        use crate::render_scene::{
            PaintLayerPlacement, PaintLayerPolicy, RenderPaintLayerBuildParts,
            RenderPaintLayerContent, RenderScene,
        };
        use crate::tree::geometry::Rect;
        use crate::tree::transform::Affine2;
        let layer = |node: u64, nodes: Vec<RenderNode>| {
            RenderNode::PaintLayer(RenderPaintLayer::from_prepared_children(
                RenderPaintLayerBuildParts {
                    id: PaintLayerId::new(node, PaintLayerReason::ScrollContent),
                    bounds: Rect::default(),
                    placement: PaintLayerPlacement::ScrollMoving,
                    policy: PaintLayerPolicy::Cacheable,
                    content_generation: 0,
                    visual_bounds: None,
                },
                RenderPaintLayerContent::from_composition_nodes(nodes),
            ))
        };
        let inner = RenderNode::Transform {
            transform: Affine2::translation(0.0, -100.0),
            children: vec![layer(42, Vec::new())],
        };
        let mut scene = RenderScene {
            scroll_fling: Some(anchor(100.0, 0.0)),
            nodes: vec![RenderNode::Transform {
                transform: Affine2::translation(0.0, 0.0),
                children: vec![layer(41, vec![inner])],
            }],
            ..Default::default()
        };
        let fling = fling();
        let at = fling.started_at + Duration::from_millis(300);
        let expected = anchor(100.0, 0.0).offset_at(&fling, at).unwrap();
        let mut carrier = Carrier::default();
        let moved = carrier.advance(&mut scene, Some(fling), at);
        assert!(moved.active && moved.changed);
        assert_eq!(shown_offset(&scene.nodes, 42, true), Some(expected));
        assert_eq!(shown_offset(&scene.nodes, 41, true), Some(0.0));
        // A scene whose content already shows the carried offset (an adopted
        // copy) is not moved twice.
        let again = carrier.advance(&mut scene, Some(fling), at);
        assert!(again.active && !again.changed);
        assert_eq!(shown_offset(&scene.nodes, 42, true), Some(expected));
        let stopped = carrier.advance(&mut scene, None, at);
        assert!(!stopped.active && stopped.changed);
        assert_eq!(shown_offset(&scene.nodes, 42, true), Some(100.0));
    }

    #[test]
    fn the_carrier_moves_the_scene_and_puts_it_back_when_the_fling_stops() {
        use crate::render_scene::{
            PaintLayerPlacement, PaintLayerPolicy, RenderPaintLayerBuildParts,
            RenderPaintLayerContent, RenderScene,
        };
        use crate::tree::geometry::Rect;
        use crate::tree::transform::Affine2;
        let content = RenderNode::PaintLayer(RenderPaintLayer::from_prepared_children(
            RenderPaintLayerBuildParts {
                id: PaintLayerId::new(42, PaintLayerReason::ScrollContent),
                bounds: Rect::default(),
                placement: PaintLayerPlacement::ScrollMoving,
                policy: PaintLayerPolicy::Cacheable,
                content_generation: 0,
                visual_bounds: None,
            },
            RenderPaintLayerContent::default(),
        ));
        let mut scene = RenderScene {
            scroll_fling: Some(anchor(100.0, 0.0)),
            nodes: vec![RenderNode::Transform {
                transform: Affine2::translation(0.0, -100.0),
                children: vec![content],
            }],
            ..Default::default()
        };
        let ty = |scene: &RenderScene| match &scene.nodes[0] {
            RenderNode::Transform { transform, .. } => transform.ty,
            _ => unreachable!(),
        };
        let fling = fling();
        let at = fling.started_at + Duration::from_millis(300);
        let mut carrier = Carrier::default();
        let first = carrier.advance(&mut scene, Some(fling), at);
        assert!(first.active && first.changed);
        let expected = anchor(100.0, 0.0).offset_at(&fling, at).unwrap();
        assert_eq!(ty(&scene), -expected);
        // The same instant again: nothing to move.
        let again = carrier.advance(&mut scene, Some(fling), at);
        assert!(again.active && !again.changed);
        // Cancelled: back to where the tree placed it.
        let stopped = carrier.advance(&mut scene, None, at);
        assert!(!stopped.active && stopped.changed);
        assert_eq!(ty(&scene), -100.0);
    }
}
