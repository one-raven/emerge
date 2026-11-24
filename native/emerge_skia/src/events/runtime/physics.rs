//! One Raven patch: tunable scroll physics.
//!
//! The model (unchanged defaults):
//!
//! - A press becomes a drag once the pointer moves `touch_slop` px from where
//!   it went down (the drag's axis is the dominant one). From then on the
//!   content follows the pointer one to one.
//! - Velocity: by default a one-pole filter over per-event velocities (events
//!   closer than 4 ms are merged into the next), alpha 0.25. With
//!   `velocity_window` set, the least-squares slope of the pointer samples of
//!   the last `velocity_window` before release, so a finger that stops before
//!   lifting does not fling (the filter keeps the last moving value).
//! - Release: below `min_velocity` nothing; otherwise the velocity (clamped
//!   to `max_velocity`) starts a fling whose duration and distance follow
//!   Android's `OverScroller` spline fit (`AdaptiveScrollSimulation`, as
//!   Flutter's `ClampingScrollSimulation`): duration grows as
//!   `(v / v_ref)^(1/(D-1))` with D = 2.358 and v_ref = friction x 51890 / 0.35,
//!   and the position eases as `1 - (1 - t)^D`.
//! - Edges: `Overscroll::None` (default) clamps, and a fling stops at the
//!   edge. `Overscroll::Bounce` rubber-bands a drag past the edge (iOS's
//!   `(1 - 1/(x * 0.55 / d + 1)) * d`, d the viewport length) and springs back
//!   (critically damped) on release; a fling reaching the edge carries its
//!   velocity into the spring.
use std::time::{Duration, Instant};

/// How a drag or fling meets the end of the content.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Overscroll {
    #[default]
    None,
    Bounce,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScrollPhysics {
    pub friction: f32,
    pub min_velocity: f32,
    pub max_velocity: f32,
    pub velocity_window: Option<Duration>,
    pub touch_slop: f32,
    pub overscroll: Overscroll,
    /// Compositor scrolling (tree/render.rs `CompositorScroll` and the
    /// renderer's scroll tiles).
    pub compositor: bool,
}

impl Default for ScrollPhysics {
    fn default() -> Self {
        Self {
            friction: super::ADAPTIVE_SCROLL_FRICTION,
            min_velocity: super::ADAPTIVE_SCROLL_MIN_VELOCITY,
            max_velocity: super::ADAPTIVE_SCROLL_MAX_VELOCITY,
            velocity_window: None,
            touch_slop: 10.0,
            overscroll: Overscroll::None,
            compositor: true,
        }
    }
}

impl ScrollPhysics {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let positive = |name: &str, value: f32| {
            if value.is_finite() && value > 0.0 {
                Ok(())
            } else {
                Err(format!("scroll {name} must be a positive number"))
            }
        };
        positive("friction", self.friction)?;
        positive("min_velocity", self.min_velocity)?;
        positive("max_velocity", self.max_velocity)?;
        positive("touch_slop", self.touch_slop)?;
        if self.max_velocity < self.min_velocity {
            return Err("scroll max_velocity must be at least min_velocity".into());
        }
        Ok(())
    }
}

/// iOS rubber band: how far content moves when the finger pulls `excess`
/// past the edge of a viewport `dimension` long.
pub(crate) fn rubber_band(excess: f32, dimension: f32) -> f32 {
    const C: f32 = 0.55;
    let d = dimension.max(1.0);
    let x = excess.abs();
    (1.0 - 1.0 / (x * C / d + 1.0)) * d * excess.signum()
}

/// The finger excess that `rubber_band` maps to `offset`.
pub(crate) fn rubber_band_inverse(offset: f32, dimension: f32) -> f32 {
    const C: f32 = 0.55;
    let d = dimension.max(1.0);
    let r = offset.abs().min(d * 0.999);
    r * d / (C * (d - r)) * offset.signum()
}

/// Critically damped return of an overscroll to zero.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SpringBack {
    pub from: f32,
    pub velocity: f32,
    pub started_at: Instant,
}

/// Rate of the spring back (1/s): settles within ~0.5 s.
const SPRING_OMEGA: f32 = 12.0;
const SPRING_REST: f32 = 0.5;

impl SpringBack {
    pub(crate) fn at(&self, now: Instant) -> (f32, bool) {
        let t = now.saturating_duration_since(self.started_at).as_secs_f32();
        let x = (self.from + (self.velocity + SPRING_OMEGA * self.from) * t)
            * (-SPRING_OMEGA * t).exp();
        let done = t > 0.05 && x.abs() < SPRING_REST;
        (if done { 0.0 } else { x }, done)
    }
}

/// Least-squares slope (px/s) of `samples` (time, position), or 0.
pub(crate) fn least_squares_velocity(samples: &[(Instant, f32)]) -> f32 {
    let Some(&(t0, p0)) = samples.first() else {
        return 0.0;
    };
    if samples.len() < 2 {
        return 0.0;
    }
    let n = samples.len() as f32;
    let (mut st, mut sp, mut stt, mut stp) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for &(at, position) in samples {
        let t = at.saturating_duration_since(t0).as_secs_f32();
        st += t;
        let p = position - p0;
        sp += p;
        stt += t * t;
        stp += t * p;
    }
    let denominator = n * stt - st * st;
    if denominator.abs() < 1e-9 {
        return 0.0;
    }
    (n * stp - st * sp) / denominator
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rubber_band_resists_more_the_further_it_pulls() {
        let d = 1000.0;
        assert!((rubber_band(0.0, d)).abs() < 1e-6);
        let small = rubber_band(100.0, d);
        let large = rubber_band(1000.0, d);
        assert!(small > 0.0 && small < 100.0);
        assert!(large < 400.0 && large > small);
        assert!(rubber_band(1.0e9, d) < d);
        assert!((rubber_band(-100.0, d) + small).abs() < 1e-3);
        assert!((rubber_band_inverse(small, d) - 100.0).abs() < 0.01);
    }

    #[test]
    fn a_spring_back_settles_at_zero() {
        let started_at = Instant::now();
        let spring = SpringBack {
            from: 120.0,
            velocity: 0.0,
            started_at,
        };
        let (start, _) = spring.at(started_at);
        assert!((start - 120.0).abs() < 1e-3);
        let (mid, done) = spring.at(started_at + Duration::from_millis(150));
        assert!(mid > 0.0 && mid < 120.0 && !done);
        let (end, done) = spring.at(started_at + Duration::from_secs(2));
        assert_eq!(end, 0.0);
        assert!(done);
        // A fling into the edge overshoots, then returns.
        let fling = SpringBack {
            from: 0.0,
            velocity: 2000.0,
            started_at,
        };
        let (peak, _) = fling.at(started_at + Duration::from_millis(80));
        assert!(peak > 20.0, "{peak}");
    }

    #[test]
    fn least_squares_velocity_ignores_a_pause_outside_the_window() {
        let t0 = Instant::now();
        let moving: Vec<_> = (0..10)
            .map(|i| (t0 + Duration::from_millis(i * 8), i as f32 * 24.0))
            .collect();
        assert!((least_squares_velocity(&moving) - 3000.0).abs() < 1.0);
        let still: Vec<_> = (0..10)
            .map(|i| (t0 + Duration::from_millis(i * 8), 240.0))
            .collect();
        assert_eq!(least_squares_velocity(&still), 0.0);
    }
}
