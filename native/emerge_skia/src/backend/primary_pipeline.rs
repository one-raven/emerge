//! One Raven patch: when the DRM OpenGL presenter draws, commits or waits.
//!
//! The presenter keeps at most one primary staged behind one in-flight atomic
//! commit. These are the loop's decisions about that pipeline, kept free of
//! DRM and GL so they are tested on every host.

/// A dirty generation can be drawn: it is neither committed nor in flight, and
/// no other primary is already staged.
pub(crate) fn should_prepare_primary(
    desired_generation: u64,
    committed_generation: u64,
    in_flight_generation: Option<u64>,
    prepared_generation: Option<u64>,
) -> bool {
    desired_generation != committed_generation
        && in_flight_generation != Some(desired_generation)
        && prepared_generation.is_none()
}

pub(crate) fn should_submit_prepared_primary(
    commit_in_flight: bool,
    committed_generation: u64,
    prepared_generation: Option<u64>,
) -> bool {
    !commit_in_flight
        && prepared_generation
            .map(|generation| generation != committed_generation)
            .unwrap_or(false)
}

/// Whether to go round the loop again instead of waiting in poll, right after
/// this pass committed the staged primary.
///
/// A scene that arrives while a primary is staged is taken at once but can't be
/// drawn until the page flip commits that staged primary. Upstream then waited
/// in poll for the following flip (or another scene) before drawing it, so on
/// continuous input each frame paid for its draw and a whole flip in turn, and
/// the presenter sat idle with a dirty scene in hand. Drawing it now, behind
/// the commit just made, keeps one frame in flight and one staged.
pub(crate) fn should_draw_behind_new_commit(
    committed_primary_this_pass: bool,
    desired_generation: u64,
    committed_generation: u64,
    in_flight_generation: Option<u64>,
    prepared_generation: Option<u64>,
) -> bool {
    committed_primary_this_pass
        && should_prepare_primary(
            desired_generation,
            committed_generation,
            in_flight_generation,
            prepared_generation,
        )
}

/// Whether the unchanged-frame fingerprint still describes the newest drawn
/// primary once `drawn_generation` is drawn.
///
/// The fingerprint is taken only when no commit is in flight. A primary drawn
/// behind an in-flight commit skips that check, so the fingerprint would still
/// describe an older frame, and a later scene equal to that older frame would
/// be skipped as "unchanged" while the screen shows something else.
pub(crate) fn fingerprint_describes_drawn_primary(
    fingerprinted_generation: Option<u64>,
    drawn_generation: u64,
) -> bool {
    fingerprinted_generation == Some(drawn_generation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scene_taken_while_a_primary_was_staged_is_drawn_behind_its_commit() {
        // Generation 4 was staged behind 3; the flip of 3 committed it, and
        // scene 5 arrived meanwhile.
        assert!(should_draw_behind_new_commit(true, 5, 3, Some(4), None));
    }

    #[test]
    fn nothing_newer_than_the_commit_waits_for_the_flip() {
        assert!(!should_draw_behind_new_commit(true, 4, 3, Some(4), None));
    }

    #[test]
    fn only_a_primary_commit_skips_the_wait() {
        // A cursor-only commit or no commit at all: the loop waits as before.
        assert!(!should_draw_behind_new_commit(false, 5, 3, Some(4), None));
        assert!(!should_draw_behind_new_commit(false, 5, 3, None, None));
    }

    #[test]
    fn never_stages_a_second_primary() {
        assert!(!should_draw_behind_new_commit(true, 6, 3, Some(4), Some(5)));
    }

    #[test]
    fn a_primary_drawn_without_its_own_check_invalidates_the_fingerprint() {
        assert!(fingerprint_describes_drawn_primary(Some(7), 7));
        assert!(!fingerprint_describes_drawn_primary(Some(6), 7));
        assert!(!fingerprint_describes_drawn_primary(None, 7));
    }

    /// A small model of the presenter loop's order (flip events, scenes, draw,
    /// commit, then poll), driven by the same decisions as the real loop.
    struct Model {
        scene_interval_ms: f64,
        draw_ms: f64,
        gpu_ms: f64,
        vblank_ms: f64,
    }

    struct Outcome {
        presented: u32,
        /// Scene arrival to its first draw, for the scenes that were drawn.
        worst_scene_to_draw_ms: f64,
    }

    impl Model {
        fn run(&self, duration_ms: f64) -> Outcome {
            let mut now = 0.0_f64;
            let mut next_scene_at = 0.0_f64;
            let mut desired = 0_u64;
            let mut desired_since = 0.0_f64;
            let mut committed = 0_u64;
            let mut in_flight: Option<(u64, f64)> = None;
            let mut prepared: Option<u64> = None;
            let mut presented = 0;
            let mut worst = 0.0_f64;
            let mut passes = 0;
            while now < duration_ms {
                passes += 1;
                assert!(passes < 100_000, "the loop spun without progress");
                if let Some((generation, flip_at)) = in_flight
                    && now >= flip_at
                {
                    committed = generation;
                    in_flight = None;
                    presented += 1;
                }
                while next_scene_at <= now {
                    desired += 1;
                    desired_since = next_scene_at;
                    next_scene_at += self.scene_interval_ms;
                }
                if should_prepare_primary(
                    desired,
                    committed,
                    in_flight.map(|(generation, _)| generation),
                    prepared,
                ) {
                    worst = worst.max(now - desired_since);
                    prepared = Some(desired);
                    now += self.draw_ms;
                }
                let mut committed_primary = false;
                if should_submit_prepared_primary(in_flight.is_some(), committed, prepared) {
                    let ready = now + self.gpu_ms;
                    let flip_at = (ready / self.vblank_ms).ceil() * self.vblank_ms;
                    in_flight = Some((prepared.take().unwrap(), flip_at));
                    committed_primary = true;
                }
                if should_draw_behind_new_commit(
                    committed_primary,
                    desired,
                    committed,
                    in_flight.map(|(generation, _)| generation),
                    prepared,
                ) {
                    continue;
                }
                // poll: woken by the next scene or the in-flight flip.
                let wake = in_flight
                    .map(|(_, flip_at)| flip_at.min(next_scene_at))
                    .unwrap_or(next_scene_at);
                now = now.max(wake);
            }
            Outcome {
                presented,
                worst_scene_to_draw_ms: worst,
            }
        }
    }

    #[test]
    fn continuous_scenes_overlap_drawing_with_the_flip() {
        // The hub's sheet drag: a scene every 24 ms, an 18.5 ms draw, a 54 Hz
        // panel, ~26 ms from commit to flip. Waiting in poll after each commit,
        // this model presents 35 fps and leaves a scene up to 23 ms undrawn;
        // drawing behind the commit keeps up with the scenes (41 fps).
        let model = Model {
            scene_interval_ms: 24.0,
            draw_ms: 18.5,
            gpu_ms: 16.0,
            vblank_ms: 1000.0 / 54.0,
        };
        let outcome = model.run(2_000.0);
        let fps = f64::from(outcome.presented) / 2.0;
        assert!(fps >= 40.0, "presented {fps} fps");
        assert!(
            outcome.worst_scene_to_draw_ms <= 10.0,
            "a scene waited {} ms to be drawn",
            outcome.worst_scene_to_draw_ms
        );
    }

    #[test]
    fn idle_presenter_draws_each_scene_once_and_does_not_spin() {
        let model = Model {
            scene_interval_ms: 500.0,
            draw_ms: 18.5,
            gpu_ms: 6.0,
            vblank_ms: 1000.0 / 54.0,
        };
        let outcome = model.run(2_000.0);
        assert_eq!(outcome.presented, 4);
    }
}
