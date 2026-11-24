//! One Raven patch: how often the presenter draws frames whose only change
//! is a renderer-driven animation.
//!
//! A wall panel idles most of the day with ambient loops running (a slow sky,
//! a pulsing dot). Drawing those at the display rate costs the same as a
//! fast spinner. A loop whose fastest step at a lower rate is still under a
//! device pixel (and a few 8-bit alpha levels) looks the same at that rate, so
//! it is drawn every second or third vblank. Anything faster, and every frame
//! that carries a new scene from the tree (input, tree-driven animations),
//! stays at the display rate.
//!
//! Off with `EMERGE_RENDERER_ANIMATION_FULL_RATE=1` in the environment.

use std::time::{Duration, Instant};

use crate::render_scene::AnimationMotion;

/// Largest corner movement, in device pixels, a reduced-rate frame may make.
pub(crate) const MAX_STEP_PX: f32 = 1.0;
/// Largest alpha change a reduced-rate frame may make.
pub(crate) const MAX_ALPHA_STEP: f32 = 4.0 / 255.0;
/// Display frames per animation frame, most first: 1/3 and 1/2 of the
/// display rate (18 and 27 fps at the hub's 54 Hz, 20 and 30 at 60 Hz).
const DIVISORS: [u32; 2] = [3, 2];
/// Never draw an animation slower than this, whatever its motion.
const MIN_FPS: f64 = 15.0;

#[cfg_attr(test, allow(dead_code))]
pub(crate) const FULL_RATE_ENV: &str = "EMERGE_RENDERER_ANIMATION_FULL_RATE";

/// Whether reduced rates are allowed (not opted out in the environment).
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn reduced_rates_enabled() -> bool {
    reduced_rates_enabled_for(std::env::var(FULL_RATE_ENV).ok().as_deref())
}

fn reduced_rates_enabled_for(value: Option<&str>) -> bool {
    !matches!(value.map(str::trim), Some(value) if !value.is_empty() && value != "0" && !value.eq_ignore_ascii_case("false"))
}

/// Display frames per drawn animation frame for these animations: the
/// largest divisor at which every animation's step stays within
/// `MAX_STEP_PX` and `MAX_ALPHA_STEP`, else 1 (every vblank). No animations
/// (or reduced rates off) is 1.
pub(crate) fn frame_divisor(
    motions: &[AnimationMotion],
    display_interval: Duration,
    reduced_rates: bool,
) -> u32 {
    if !reduced_rates || motions.is_empty() || display_interval.is_zero() {
        return 1;
    }
    DIVISORS
        .into_iter()
        .find(|&divisor| {
            let period = display_interval.as_secs_f64() * f64::from(divisor);
            period * MIN_FPS <= 1.0
                && motions.iter().all(|motion| {
                    let px = f64::from(motion.px_per_second) * period;
                    let alpha = f64::from(motion.alpha_per_second) * period;
                    px.is_finite()
                        && alpha.is_finite()
                        && px <= f64::from(MAX_STEP_PX)
                        && alpha <= f64::from(MAX_ALPHA_STEP)
                })
        })
        .unwrap_or(1)
}

/// When to start drawing the next animation-only frame, given when the last
/// one reached the screen. With a divisor of 1 that is at once; with n it is
/// just after the (n-1)th vblank that follows, so the frame is drawn and
/// committed in time for the nth.
pub(crate) fn next_frame_at(
    last_presented_at: Instant,
    display_interval: Duration,
    divisor: u32,
) -> Instant {
    if divisor <= 1 {
        return last_presented_at;
    }
    last_presented_at + display_interval * (divisor - 1) + display_interval / 8
}

/// When the next animation-only frame is due: `next_frame_at`, or later if
/// the last one was skipped as unchanged.
pub(crate) fn frame_due_at(
    last_presented_at: Instant,
    retry_at: Option<Instant>,
    display_interval: Duration,
    divisor: u32,
) -> Instant {
    let due = next_frame_at(last_presented_at, display_interval, divisor);
    retry_at.map_or(due, |retry_at| retry_at.max(due))
}

/// When to look again after a frame was skipped because nothing it would
/// draw differs from what is on screen.
pub(crate) fn retry_after_unchanged_at(now: Instant, display_interval: Duration, divisor: u32) -> Instant {
    now + display_interval * divisor.max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HZ54: Duration = Duration::from_nanos(18_518_519);
    const HZ60: Duration = Duration::from_nanos(16_666_667);

    fn moving(px_per_second: f32) -> AnimationMotion {
        AnimationMotion {
            px_per_second,
            alpha_per_second: 0.0,
        }
    }

    fn fading(alpha_per_second: f32) -> AnimationMotion {
        AnimationMotion {
            px_per_second: 0.0,
            alpha_per_second,
        }
    }

    #[test]
    fn fast_animations_stay_at_the_display_rate() {
        // A 24 px fan turning once a second moves its corners ~107 px/s.
        assert_eq!(frame_divisor(&[moving(107.0)], HZ54, true), 1);
        // 30 px/s: 1.1 px per frame at 27 fps, too much.
        assert_eq!(frame_divisor(&[moving(30.0)], HZ54, true), 1);
        // An alpha pulse 0.3 -> 1 over 1 s: ~0.7/s, 6.6 levels at 27 fps.
        assert_eq!(frame_divisor(&[fading(0.7)], HZ54, true), 1);
    }

    #[test]
    fn slow_ambient_loops_drop_to_a_half_or_a_third() {
        // At most a pixel per frame at 18 fps (54 / 3).
        assert_eq!(frame_divisor(&[moving(17.9)], HZ54, true), 3);
        assert_eq!(frame_divisor(&[moving(18.1)], HZ54, true), 2);
        assert_eq!(frame_divisor(&[moving(2.0)], HZ54, true), 3);
        // Between 18 and 27 px/s: 27 fps.
        assert_eq!(frame_divisor(&[moving(26.0)], HZ54, true), 2);
        // 60 Hz: 20 and 30 fps.
        assert_eq!(frame_divisor(&[moving(19.0)], HZ60, true), 3);
        assert_eq!(frame_divisor(&[moving(29.0)], HZ60, true), 2);
        // A slow alpha pulse (0.1/s): under 4/255 per frame at 18 fps.
        assert_eq!(frame_divisor(&[fading(0.1)], HZ54, true), 3);
    }

    #[test]
    fn the_fastest_animation_sets_the_rate() {
        assert_eq!(frame_divisor(&[moving(2.0), moving(26.0)], HZ54, true), 2);
        assert_eq!(frame_divisor(&[moving(2.0), moving(107.0)], HZ54, true), 1);
        assert_eq!(frame_divisor(&[moving(2.0), fading(0.7)], HZ54, true), 1);
    }

    #[test]
    fn unmeasurable_motion_none_or_opt_out_is_full_rate() {
        assert_eq!(frame_divisor(&[moving(f32::INFINITY)], HZ54, true), 1);
        assert_eq!(frame_divisor(&[moving(f32::NAN)], HZ54, true), 1);
        assert_eq!(frame_divisor(&[], HZ54, true), 1);
        assert_eq!(frame_divisor(&[moving(0.0)], HZ54, false), 1);
        assert_eq!(frame_divisor(&[moving(0.0)], Duration::ZERO, true), 1);
    }

    #[test]
    fn never_below_fifteen_fps() {
        // A 30 Hz display: a third would be 10 fps; a half (15) is allowed.
        let hz30 = Duration::from_nanos(33_333_333);
        assert_eq!(frame_divisor(&[moving(0.0)], hz30, true), 2);
        let hz20 = Duration::from_millis(50);
        assert_eq!(frame_divisor(&[moving(0.0)], hz20, true), 1);
    }

    #[test]
    fn the_next_frame_starts_just_after_the_vblank_before_its_own() {
        let presented = Instant::now();
        assert_eq!(next_frame_at(presented, HZ54, 1), presented);
        assert_eq!(next_frame_at(presented, HZ54, 0), presented);
        let half = next_frame_at(presented, HZ54, 2);
        assert!(half > presented + HZ54 && half < presented + HZ54 * 2);
        let third = next_frame_at(presented, HZ54, 3);
        assert!(third > presented + HZ54 * 2 && third < presented + HZ54 * 3);
        assert_eq!(
            retry_after_unchanged_at(presented, HZ54, 3),
            presented + HZ54 * 3
        );
        assert_eq!(retry_after_unchanged_at(presented, HZ54, 0), presented + HZ54);
    }

    #[test]
    fn a_skipped_frame_pushes_the_next_one_back() {
        let presented = Instant::now();
        assert_eq!(frame_due_at(presented, None, HZ54, 1), presented);
        let retry = presented + HZ54 * 2;
        assert_eq!(frame_due_at(presented, Some(retry), HZ54, 1), retry);
        // A retry earlier than the rate allows doesn't speed it up.
        assert_eq!(
            frame_due_at(presented, Some(presented), HZ54, 3),
            next_frame_at(presented, HZ54, 3)
        );
    }

    #[test]
    fn the_opt_out_takes_any_true_looking_value() {
        assert!(reduced_rates_enabled_for(None));
        assert!(reduced_rates_enabled_for(Some("")));
        assert!(reduced_rates_enabled_for(Some("0")));
        assert!(reduced_rates_enabled_for(Some("false")));
        assert!(!reduced_rates_enabled_for(Some("1")));
        assert!(!reduced_rates_enabled_for(Some("yes")));
    }
}
