//! One Raven patch: a structural fingerprint of an `AnimationSpec`, for
//! telling whether an element's declared animation changed. Upstream hashed
//! the spec's `Debug` text, which formats every one of the ~100 fields of
//! every keyframe (`None`s included) into a fresh string: ~6% of the tree
//! actor's time on the RK3566 while a screen with animations was patched.
//!
//! Floats hash by their bits, so two specs share a fingerprint exactly when
//! their `Debug` text would match (`0.0` and `-0.0` differ, as their text
//! does). Each struct is destructured without `..`, so a new field doesn't
//! compile until it is hashed here.

use std::hash::{Hash, Hasher};
use std::sync::Arc;

use super::change::ChangePolicy;
use super::{AnimationCurve, AnimationRepeat, AnimationSpec};
use crate::tree::attrs::{
    AlignX, AlignY, Attrs, Background, BorderRadius, BorderStyle, BorderWidth, BoxShadow, Color,
    Font, FontStyle, FontWeight, ImageFit, ImageSource, KeyBindingSpec, Length, MouseOverAttrs,
    Padding, SolidColor, TextAlign, VirtualKeySpec,
};

pub(crate) trait Fingerprint {
    fn fingerprint<H: Hasher>(&self, state: &mut H);
}

macro_rules! via_hash {
    ($($t:ty),* $(,)?) => {
        $(impl Fingerprint for $t {
            #[inline]
            fn fingerprint<H: Hasher>(&self, state: &mut H) {
                Hash::hash(self, state);
            }
        })*
    };
}

via_hash!(
    bool,
    u8,
    u16,
    u32,
    u64,
    String,
    AlignX,
    AlignY,
    TextAlign,
    ImageFit,
    ImageSource,
    BorderStyle,
    SolidColor,
    Font,
    FontWeight,
    FontStyle,
    KeyBindingSpec,
    VirtualKeySpec,
    AnimationCurve,
    AnimationRepeat,
    super::change::Field,
    crate::events::scroll::ScrollPosition,
);

impl Fingerprint for f64 {
    #[inline]
    fn fingerprint<H: Hasher>(&self, state: &mut H) {
        state.write_u64(self.to_bits());
    }
}

impl<T: Fingerprint> Fingerprint for Option<T> {
    #[inline]
    fn fingerprint<H: Hasher>(&self, state: &mut H) {
        match self {
            None => state.write_u8(0),
            Some(value) => {
                state.write_u8(1);
                value.fingerprint(state);
            }
        }
    }
}

impl<T: Fingerprint> Fingerprint for [T] {
    fn fingerprint<H: Hasher>(&self, state: &mut H) {
        state.write_usize(self.len());
        for value in self {
            value.fingerprint(state);
        }
    }
}

impl<T: Fingerprint> Fingerprint for Vec<T> {
    fn fingerprint<H: Hasher>(&self, state: &mut H) {
        self.as_slice().fingerprint(state);
    }
}

impl<T: Fingerprint + ?Sized> Fingerprint for Box<T> {
    fn fingerprint<H: Hasher>(&self, state: &mut H) {
        (**self).fingerprint(state);
    }
}

impl<T: Fingerprint + ?Sized> Fingerprint for Arc<T> {
    fn fingerprint<H: Hasher>(&self, state: &mut H) {
        (**self).fingerprint(state);
    }
}

impl<A: Fingerprint, B: Fingerprint> Fingerprint for (A, B) {
    fn fingerprint<H: Hasher>(&self, state: &mut H) {
        self.0.fingerprint(state);
        self.1.fingerprint(state);
    }
}

/// Hashes every field of a struct, by an exhaustive destructuring.
macro_rules! fingerprint_struct {
    ($t:ident { $($(#[$attr:meta])* $field:ident),* $(,)? }) => {
        impl Fingerprint for $t {
            fn fingerprint<H: Hasher>(&self, state: &mut H) {
                let $t { $($(#[$attr])* $field),* } = self;
                $($(#[$attr])* $field.fingerprint(state);)*
            }
        }
    };
}

/// Hashes the variant, then its fields.
macro_rules! fingerprint_enum {
    ($t:ident { $($variant:ident $({ $($field:ident),* })? $(( $($tuple:ident),* ))?),* $(,)? }) => {
        impl Fingerprint for $t {
            fn fingerprint<H: Hasher>(&self, state: &mut H) {
                std::mem::discriminant(self).hash(state);
                match self {
                    $($t::$variant $({ $($field),* })? $(( $($tuple),* ))? => {
                        $($($field.fingerprint(state);)*)?
                        $($($tuple.fingerprint(state);)*)?
                    })*
                }
            }
        }
    };
}

fingerprint_enum!(Length {
    Fill,
    Content,
    Px(value),
    FillWeighted(weight),
    Min(a, b),
    Max(a, b),
});

fingerprint_enum!(Padding {
    Uniform(value),
    Sides { top, right, bottom, left },
});

fingerprint_enum!(Color {
    Gradient { colors, angle },
    Rgb { r, g, b },
    Rgba { r, g, b, a },
    Named(name),
});

fingerprint_enum!(Background {
    Color(color),
    Image { source, fit },
});

fingerprint_enum!(BorderRadius {
    Uniform(value),
    Corners { tl, tr, br, bl },
});

fingerprint_enum!(BorderWidth {
    Uniform(value),
    Sides { top, right, bottom, left },
});

fingerprint_struct!(BoxShadow {
    offset_x,
    offset_y,
    blur,
    size,
    color,
    inset,
});

fingerprint_struct!(MouseOverAttrs {
    background,
    border_radius,
    border_width,
    border_style,
    border_color,
    box_shadows,
    font,
    font_weight,
    font_style,
    font_color,
    svg_color,
    font_size,
    font_underline,
    font_strike,
    font_letter_spacing,
    font_word_spacing,
    text_align,
    move_x,
    move_y,
    rotate,
    scale,
    alpha,
});

fingerprint_struct!(ChangePolicy {
    field,
    duration_ms,
    curve,
    from,
});

fingerprint_struct!(AnimationSpec {
    keyframes,
    duration_ms,
    curve,
    repeat,
});

#[cfg(test)]
impl Fingerprint for crate::tree::attrs::TextFragment {
    fn fingerprint<H: Hasher>(&self, state: &mut H) {
        let Self {
            x,
            y,
            text,
            font_size,
            color,
            family,
            weight,
            italic,
            underline,
            strike,
            ascent,
        } = self;
        for value in [x, y, font_size, ascent] {
            state.write_u32(value.to_bits());
        }
        text.fingerprint(state);
        color.hash(state);
        family.fingerprint(state);
        weight.fingerprint(state);
        italic.fingerprint(state);
        underline.fingerprint(state);
        strike.fingerprint(state);
    }
}

#[cfg(test)]
via_hash!(crate::tree::attrs::ScrollbarHoverAxis);

fingerprint_struct!(Attrs {
    width,
    height,
    layout_scale,
    layout_rotate,
    padding,
    spacing,
    spacing_x,
    spacing_y,
    align_x,
    align_y,
    scrollbar_y,
    scrollbar_x,
    ghost_scrollbar_y,
    ghost_scrollbar_x,
    #[cfg(test)]
    scrollbar_hover_axis,
    scroll_x,
    scroll_y,
    #[cfg(test)]
    scroll_x_max,
    #[cfg(test)]
    scroll_y_max,
    on_click,
    on_mouse_down,
    on_mouse_up,
    on_mouse_enter,
    on_mouse_leave,
    on_mouse_move,
    on_press,
    on_swipe_up,
    on_swipe_down,
    on_swipe_left,
    on_swipe_right,
    on_change,
    on_scroll,
    scroll_position_x,
    scroll_position_y,
    on_focus,
    on_blur,
    focus_on_mount,
    clip_nearby,
    on_key_down,
    on_key_up,
    on_key_press,
    virtual_key,
    mouse_over,
    focused,
    mouse_down,
    #[cfg(test)]
    mouse_over_active,
    #[cfg(test)]
    mouse_down_active,
    #[cfg(test)]
    focused_active,
    #[cfg(test)]
    text_input_focused,
    #[cfg(test)]
    text_input_cursor,
    #[cfg(test)]
    text_input_selection_anchor,
    #[cfg(test)]
    text_input_preedit,
    #[cfg(test)]
    text_input_preedit_cursor,
    background,
    border_radius,
    border_width,
    border_style,
    border_color,
    box_shadows,
    font_size,
    font_color,
    font,
    font_weight,
    font_style,
    font_underline,
    font_strike,
    font_letter_spacing,
    font_word_spacing,
    svg_color,
    svg_expected,
    image_src,
    image_fit,
    image_size,
    slider_min,
    slider_max,
    slider_value,
    slider_step,
    video_target,
    text_align,
    content,
    snap_layout,
    snap_text_metrics,
    move_x,
    move_y,
    rotate,
    scale,
    alpha,
    animate,
    animate_change,
    animate_enter,
    animate_exit,
    space_evenly,
    #[cfg(test)]
    paragraph_fragments,
});

/// The fingerprint `AnimationRuntimeEntry::spec_hash` holds.
pub fn spec_fingerprint(spec: &AnimationSpec) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    spec.fingerprint(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::attrs::Length;

    fn spec(keyframes: Vec<Attrs>) -> AnimationSpec {
        AnimationSpec {
            keyframes,
            duration_ms: 900.0,
            curve: AnimationCurve::EaseInOut,
            repeat: AnimationRepeat::Loop,
        }
    }

    /// A keyframe like the hub's: a card sliding, turning and fading in,
    /// with a shadow and a colour.
    fn card_keyframe(t: f64) -> Attrs {
        Attrs {
            width: Some(Length::Px(320.0)),
            height: Some(Length::Fill),
            move_x: Some(24.0 * t),
            move_y: Some(-8.0 * t),
            rotate: Some(6.0 * t),
            scale: Some(1.0 + 0.05 * t),
            alpha: Some(t),
            background: Some(Background::Color(Color::Rgba {
                r: 20,
                g: 40,
                b: 60,
                a: 255,
            })),
            border_radius: Some(BorderRadius::Uniform(16.0)),
            box_shadows: Some(vec![BoxShadow {
                offset_x: 0.0,
                offset_y: 4.0 * t,
                blur: 12.0,
                size: 0.0,
                color: Color::Rgba {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 60,
                },
                inset: false,
            }]),
            ..Attrs::default()
        }
    }

    fn card_spec() -> AnimationSpec {
        spec(vec![
            card_keyframe(0.0),
            card_keyframe(0.5),
            card_keyframe(1.0),
        ])
    }

    #[test]
    fn equal_specs_share_a_fingerprint_and_any_changed_field_changes_it() {
        let base = card_spec();
        assert_eq!(spec_fingerprint(&base), spec_fingerprint(&base.clone()));
        let mut variants = Vec::new();
        let mut changed = base.clone();
        changed.duration_ms = 901.0;
        variants.push(changed);
        let mut changed = base.clone();
        changed.curve = AnimationCurve::Linear;
        variants.push(changed);
        let mut changed = base.clone();
        changed.repeat = AnimationRepeat::Times(2);
        variants.push(changed);
        let mut changed = base.clone();
        changed.keyframes[1].move_x = Some(12.5);
        variants.push(changed);
        let mut changed = base.clone();
        changed.keyframes[1].move_x = None;
        variants.push(changed);
        let mut changed = base.clone();
        changed.keyframes[2].alpha = Some(0.25);
        variants.push(changed);
        let mut changed = base.clone();
        changed.keyframes[0].box_shadows.as_mut().unwrap()[0].inset = true;
        variants.push(changed);
        let mut changed = base.clone();
        changed.keyframes[0].width = Some(Length::Min(
            Box::new(Length::Px(320.0)),
            Box::new(Length::Fill),
        ));
        variants.push(changed);
        let mut changed = base.clone();
        changed.keyframes[0].content = Some("x".into());
        variants.push(changed);
        let mut changed = base.clone();
        changed.keyframes.pop();
        variants.push(changed);
        // A value moved to the neighbouring field.
        let mut changed = base.clone();
        changed.keyframes[1].move_y = changed.keyframes[1].move_x;
        changed.keyframes[1].move_x = None;
        variants.push(changed);

        let mut seen = std::collections::HashSet::new();
        assert!(seen.insert(spec_fingerprint(&base)));
        for (index, variant) in variants.iter().enumerate() {
            assert_ne!(variant, &base, "variant {index} must differ");
            assert!(seen.insert(spec_fingerprint(variant)), "variant {index}");
        }
    }

    #[test]
    fn negative_zero_differs_as_its_debug_text_does() {
        let mut a = card_spec();
        let mut b = card_spec();
        a.keyframes[0].move_x = Some(0.0);
        b.keyframes[0].move_x = Some(-0.0);
        assert_ne!(spec_fingerprint(&a), spec_fingerprint(&b));
    }

    /// `cargo test --release --lib spec_fingerprint_timing -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn spec_fingerprint_timing() {
        fn debug_fingerprint(spec: &AnimationSpec) -> u64 {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            format!("{spec:?}").hash(&mut hasher);
            hasher.finish()
        }
        let spec = card_spec();
        let runs = 20_000;
        let time = |f: &dyn Fn(&AnimationSpec) -> u64| {
            let started = std::time::Instant::now();
            let mut sink = 0u64;
            for _ in 0..runs {
                sink ^= f(std::hint::black_box(&spec));
            }
            std::hint::black_box(sink);
            started.elapsed().as_nanos() as f64 / runs as f64
        };
        let debug = time(&debug_fingerprint);
        let structural = time(&spec_fingerprint);
        eprintln!(
            "spec_fingerprint, 3 keyframes: Debug text {debug:.0} ns, structural {structural:.0} ns"
        );
    }
}
