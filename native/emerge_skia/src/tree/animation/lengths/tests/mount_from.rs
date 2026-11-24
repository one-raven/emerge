// One Raven patch: `Animation.change(..., from: ...)` eases an element from a
// given value on its first mount instead of starting at its target.
use super::support::*;
use super::*;

fn policy(field: change::Field, from: Option<Attrs>) -> change::ChangePolicy {
    change::ChangePolicy {
        field,
        duration_ms: 1000.0,
        curve: AnimationCurve::Linear,
        from: from.map(Arc::new),
    }
}

fn alpha(value: f64) -> Attrs {
    Attrs {
        alpha: Some(value),
        ..Attrs::default()
    }
}

fn move_x(value: f64) -> Attrs {
    Attrs {
        move_x: Some(value),
        ..Attrs::default()
    }
}

// Node 2 fades to 1.0 and moves to 0, each with a change policy.
fn tree(policies: Vec<change::ChangePolicy>) -> ElementTree {
    let mut tree = fixture(Length::Px(40.0), Length::Fill, Axis::Width);
    let node = tree.get_mut(&id(2)).unwrap();
    // Mounted by the first patch.
    node.lifecycle.mounted_at_revision = 1;
    let attrs = &mut node.spec.declared;
    attrs.animate = None;
    attrs.width = Some(Length::Px(40.0));
    attrs.alpha = Some(1.0);
    attrs.move_x = Some(0.0);
    attrs.animate_change = Some(Arc::new(policies));
    tree
}

fn effective(d: &Driver) -> (Option<f64>, Option<f64>) {
    let attrs = &d.tree.get(&id(2)).unwrap().layout.effective;
    (attrs.alpha, attrs.move_x)
}

fn close_to(value: Option<f64>, expected: f64) {
    let value = value.expect("value");
    assert!((value - expected).abs() < 1e-3, "{value} != {expected}");
}

#[test]
fn a_first_mount_eases_from_the_given_value() {
    let mut d = Driver::new(
        tree(vec![
            policy(change::Field::Alpha, Some(alpha(0.0))),
            policy(change::Field::MoveX, Some(move_x(-300.0))),
        ]),
        Instant::now(),
        Mode::Full,
    );
    assert!(d.step(0, vec![]).result.is_ok());
    let (a, x) = effective(&d);
    close_to(a, 0.0);
    close_to(x, -300.0);
    assert!(d.step(500_000, vec![]).result.is_ok());
    let (a, x) = effective(&d);
    close_to(a, 0.5);
    close_to(x, -150.0);
    assert!(d.step(1_000_000, vec![]).result.is_ok());
    assert!(d.step(1_100_000, vec![]).result.is_ok());
    let (a, x) = effective(&d);
    close_to(a, 1.0);
    close_to(x, 0.0);
    assert_eq!(d.runtime.changes.record_counts(), (0, 0));
}

#[test]
fn without_from_a_first_mount_is_immediate() {
    let mut d = Driver::new(
        tree(vec![
            policy(change::Field::Alpha, None),
            policy(change::Field::MoveX, Some(move_x(0.0))),
        ]),
        Instant::now(),
        Mode::Full,
    );
    assert!(d.step(0, vec![]).result.is_ok());
    let (a, x) = effective(&d);
    close_to(a, 1.0);
    close_to(x, 0.0);
    assert_eq!(d.runtime.changes.record_counts(), (0, 0));
}

// `from` is read on mount only: on a retained element a new target eases
// from the current presentation, as without it.
#[test]
fn a_retained_element_ignores_from() {
    let mut d = Driver::new(
        tree(vec![policy(change::Field::Alpha, Some(alpha(0.0)))]),
        Instant::now(),
        Mode::Full,
    );
    assert!(d.step(0, vec![]).result.is_ok());
    assert!(d.step(2_000_000, vec![]).result.is_ok());
    close_to(effective(&d).0, 1.0);
    let mut target = d.attrs(id(2));
    target.alpha = Some(0.5);
    assert!(
        d.step(2_000_000, vec![Event::Attrs(id(2), Box::new(target))])
            .result
            .is_ok()
    );
    assert!(d.step(2_500_000, vec![]).result.is_ok());
    close_to(effective(&d).0, 0.75);
}

#[test]
fn a_remount_eases_from_the_given_value_again() {
    let mut d = Driver::new(
        tree(vec![policy(change::Field::Alpha, Some(alpha(0.0)))]),
        Instant::now(),
        Mode::Full,
    );
    assert!(d.step(0, vec![]).result.is_ok());
    assert!(d.step(2_000_000, vec![]).result.is_ok());
    let attrs = d.attrs(id(2));
    let mut node = Element::with_attrs(id(2), ElementKind::El, vec![], attrs);
    node.lifecycle.mounted_at_revision = 2;
    d.tree.insert(node);
    d.tree.set_revision(2);
    d.tree.layout_model_epoch += 1;
    d.tree.mark_all_measure_dirty();
    assert!(d.step(2_000_000, vec![]).result.is_ok());
    close_to(effective(&d).0, 0.0);
    assert!(d.step(2_250_000, vec![]).result.is_ok());
    close_to(effective(&d).0, 0.25);
}

#[test]
fn an_enter_animation_of_the_same_field_owns_the_mount() {
    let mut tree = tree(vec![policy(change::Field::Alpha, Some(alpha(0.0)))]);
    tree.get_mut(&id(2)).unwrap().spec.declared.animate_enter = Some(AnimationSpec {
        keyframes: vec![alpha(0.6), alpha(1.0)],
        duration_ms: 1000.0,
        curve: AnimationCurve::Linear,
        repeat: AnimationRepeat::Once,
    });
    let mut d = Driver::new(tree, Instant::now(), Mode::Full);
    assert!(d.step(0, vec![]).result.is_ok());
    assert!(
        d.runtime
            .changes
            .get(&(id(2), change::Field::Alpha))
            .is_none()
    );
    close_to(effective(&d).0, 0.6);
}

#[test]
fn from_must_hold_only_a_compatible_value_of_its_field() {
    let attrs = |from: Attrs| {
        let mut attrs = alpha(1.0);
        attrs.animate_change = Some(Arc::new(vec![policy(change::Field::Alpha, Some(from))]));
        attrs
    };
    assert!(change::validate_policies(&attrs(alpha(0.0))).is_ok());
    let mut extra = alpha(0.0);
    extra.move_x = Some(1.0);
    assert!(change::validate_policies(&attrs(extra)).is_err());
    assert!(change::validate_policies(&attrs(move_x(1.0))).is_err());
}

// The wire: the `from` section trails the policies, so a payload without one
// is unchanged.
#[test]
fn from_values_decode_from_a_trailing_section() {
    fn string(s: &str) -> Vec<u8> {
        (s.len() as u16)
            .to_be_bytes()
            .into_iter()
            .chain(s.bytes())
            .collect()
    }
    let alpha_attr: Vec<u8> = [0, 1, 35]
        .into_iter()
        .chain(0.25_f64.to_be_bytes())
        .collect();
    let mut policy: Vec<u8> = vec![0, 1];
    policy.extend(string("alpha"));
    policy.extend(1000.0_f64.to_be_bytes());
    policy.extend(string("linear"));
    let decode = |payload: &[u8]| {
        let data: Vec<u8> = [0, 2, 35]
            .into_iter()
            .chain(1.0_f64.to_be_bytes())
            .chain([84])
            .chain((payload.len() as u32).to_be_bytes())
            .chain(payload.iter().copied())
            .collect();
        crate::tree::attrs::decode_attrs(&data)
    };
    let plain = decode(&policy).unwrap();
    assert_eq!(plain.animate_change.unwrap()[0].from, None);

    let mut with_from = policy.clone();
    with_from.extend([0, 1]);
    with_from.extend(string("alpha"));
    with_from.extend((alpha_attr.len() as u32).to_be_bytes());
    with_from.extend(alpha_attr.iter().copied());
    let decoded = decode(&with_from).unwrap();
    assert_eq!(
        decoded.animate_change.unwrap()[0].from.as_deref(),
        Some(&alpha(0.25))
    );

    // A `from` for a field without a policy is rejected.
    let mut orphan = policy;
    orphan.extend([0, 1]);
    orphan.extend(string("move_x"));
    orphan.extend((alpha_attr.len() as u32).to_be_bytes());
    orphan.extend(alpha_attr);
    assert!(decode(&orphan).is_err());
}

#[test]
fn a_width_eases_from_the_given_length_on_mount() {
    let from = Attrs {
        width: Some(Length::Px(10.0)),
        ..Attrs::default()
    };
    let mut d = Driver::new(
        tree(vec![policy(change::Field::Width, Some(from))]),
        Instant::now(),
        Mode::Full,
    );
    assert!(d.step(0, vec![]).result.is_ok());
    close(extent(&d.tree, 2, Axis::Width), 10.0);
    assert!(d.step(500_000, vec![]).result.is_ok());
    close(extent(&d.tree, 2, Axis::Width), 25.0);
    assert!(d.step(1_000_000, vec![]).result.is_ok());
    assert!(d.step(1_100_000, vec![]).result.is_ok());
    close(extent(&d.tree, 2, Axis::Width), 40.0);
    assert_eq!(d.runtime.changes.record_counts(), (0, 0));
}
