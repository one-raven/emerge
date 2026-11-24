//! One Raven patch: where a single-threaded presenter loop spends its time.
//!
//! The DRM OpenGL presenter draws, handles page flips and commits on one
//! thread. Per-step averages can't tell whether a slow output is bound by that
//! thread's CPU, by page flips, by GBM buffers or by the tree's scene supply.
//! This charges every part of the loop's wall time to one of those and reports
//! the split once per window.

use std::time::{Duration, Instant};

/// Work the presenter thread does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopWork {
    /// Reading DRM events and page-flip bookkeeping.
    Events,
    /// Taking scenes from the tree, including dropping the replaced scene.
    Scenes,
    /// The unchanged-frame fingerprint.
    SkipCheck,
    /// Preparing a primary: damage, draw, flush, swap, lock, framebuffer.
    Draw,
    /// Building and submitting an atomic commit.
    Commit,
}

/// Why the presenter thread is blocked in poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoopWait {
    /// A drawn frame is staged behind the in-flight commit's page flip.
    Flip,
    /// A dirty primary had no free GBM buffer to draw into.
    Buffer,
    /// Waiting out an atomic commit EBUSY retry.
    Retry,
    /// Nothing to draw: waiting for a scene, input, video or a timer.
    Idle,
}

const WORK_KINDS: usize = 5;
const WAIT_KINDS: usize = 4;

impl LoopWork {
    fn index(self) -> usize {
        self as usize
    }
}

impl LoopWait {
    fn index(self) -> usize {
        self as usize
    }
}

pub(crate) fn classify_loop_wait(
    staged: bool,
    commit_in_flight: bool,
    buffer_starved: bool,
    retry_pending: bool,
) -> LoopWait {
    if staged && commit_in_flight {
        LoopWait::Flip
    } else if retry_pending {
        LoopWait::Retry
    } else if buffer_starved {
        LoopWait::Buffer
    } else {
        LoopWait::Idle
    }
}

pub(crate) struct LoopAccounting {
    window: Duration,
    started_at: Instant,
    work: [Duration; WORK_KINDS],
    wait: [Duration; WAIT_KINDS],
    draws: u32,
}

impl LoopAccounting {
    pub(crate) fn new(window: Duration, now: Instant) -> Self {
        Self {
            window,
            started_at: now,
            work: [Duration::ZERO; WORK_KINDS],
            wait: [Duration::ZERO; WAIT_KINDS],
            draws: 0,
        }
    }

    pub(crate) fn add_work(&mut self, work: LoopWork, duration: Duration) {
        self.work[work.index()] += duration;
    }

    pub(crate) fn add_wait(&mut self, wait: LoopWait, duration: Duration) {
        self.wait[wait.index()] += duration;
    }

    pub(crate) fn drew(&mut self) {
        self.draws = self.draws.saturating_add(1);
    }

    /// Returns the window's report and starts a new window once `window` has passed.
    pub(crate) fn take_report(&mut self, now: Instant) -> Option<String> {
        let elapsed = now.saturating_duration_since(self.started_at);
        if elapsed < self.window || elapsed.is_zero() {
            return None;
        }
        let report = self.format(elapsed);
        *self = Self::new(self.window, now);
        Some(report)
    }

    fn format(&self, elapsed: Duration) -> String {
        let ms = |duration: Duration| duration.as_secs_f64() * 1_000.0;
        let pct = |duration: Duration| 100.0 * duration.as_secs_f64() / elapsed.as_secs_f64();
        let waiting: Duration = self.wait.iter().sum();
        let worked: Duration = self.work.iter().sum();
        let busy = elapsed.saturating_sub(waiting);
        let other = busy.saturating_sub(worked);
        let draw = self.work[LoopWork::Draw.index()];
        let per_draw = if self.draws == 0 {
            0.0
        } else {
            ms(draw) / f64::from(self.draws)
        };
        format!(
            "presenter loop over {:.0} ms: draws={} ({:.1}/s, {:.1} ms each); busy {:.0} ms ({:.0}%): draw={:.0} scenes={:.0} skip_check={:.0} events={:.0} commit={:.0} other={:.0}; waiting {:.0} ms ({:.0}%): flip={:.0} buffer={:.0} retry={:.0} idle={:.0}",
            ms(elapsed),
            self.draws,
            f64::from(self.draws) / elapsed.as_secs_f64(),
            per_draw,
            ms(busy),
            pct(busy),
            ms(draw),
            ms(self.work[LoopWork::Scenes.index()]),
            ms(self.work[LoopWork::SkipCheck.index()]),
            ms(self.work[LoopWork::Events.index()]),
            ms(self.work[LoopWork::Commit.index()]),
            ms(other),
            ms(waiting),
            pct(waiting),
            ms(self.wait[LoopWait::Flip.index()]),
            ms(self.wait[LoopWait::Buffer.index()]),
            ms(self.wait[LoopWait::Retry.index()]),
            ms(self.wait[LoopWait::Idle.index()]),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_staged_frame_behind_an_in_flight_commit_waits_for_the_flip() {
        assert_eq!(classify_loop_wait(true, true, false, false), LoopWait::Flip);
        // The flip wait wins over a stale starvation flag or retry deadline.
        assert_eq!(classify_loop_wait(true, true, true, true), LoopWait::Flip);
    }

    #[test]
    fn retry_buffer_and_idle_waits_are_told_apart() {
        assert_eq!(
            classify_loop_wait(true, false, false, true),
            LoopWait::Retry
        );
        assert_eq!(
            classify_loop_wait(false, true, true, false),
            LoopWait::Buffer
        );
        // The newest primary already in flight with nothing newer: scene-bound.
        assert_eq!(
            classify_loop_wait(false, true, false, false),
            LoopWait::Idle
        );
        assert_eq!(
            classify_loop_wait(false, false, false, false),
            LoopWait::Idle
        );
    }

    #[test]
    fn no_report_before_the_window_has_passed() {
        let start = Instant::now();
        let mut accounting = LoopAccounting::new(Duration::from_secs(2), start);
        accounting.add_work(LoopWork::Draw, Duration::from_millis(17));
        assert!(
            accounting
                .take_report(start + Duration::from_millis(1_999))
                .is_none()
        );
    }

    #[test]
    fn report_splits_the_window_into_work_other_and_waits_then_resets() {
        let start = Instant::now();
        let mut accounting = LoopAccounting::new(Duration::from_secs(2), start);
        for _ in 0..4 {
            accounting.add_work(LoopWork::Draw, Duration::from_millis(250));
            accounting.drew();
        }
        accounting.add_work(LoopWork::Scenes, Duration::from_millis(40));
        accounting.add_work(LoopWork::SkipCheck, Duration::from_millis(30));
        accounting.add_work(LoopWork::Events, Duration::from_millis(20));
        accounting.add_work(LoopWork::Commit, Duration::from_millis(10));
        accounting.add_wait(LoopWait::Flip, Duration::from_millis(500));
        accounting.add_wait(LoopWait::Idle, Duration::from_millis(300));

        let report = accounting
            .take_report(start + Duration::from_secs(2))
            .expect("window passed");
        assert_eq!(
            report,
            "presenter loop over 2000 ms: draws=4 (2.0/s, 250.0 ms each); busy 1200 ms (60%): draw=1000 scenes=40 skip_check=30 events=20 commit=10 other=100; waiting 800 ms (40%): flip=500 buffer=0 retry=0 idle=300"
        );

        let next = accounting
            .take_report(start + Duration::from_secs(4))
            .expect("second window passed");
        assert!(next.contains("draws=0 (0.0/s, 0.0 ms each)"), "{next}");
        assert!(next.contains("busy 2000 ms (100%)"), "{next}");
        assert!(next.contains("flip=0 buffer=0 retry=0 idle=0"), "{next}");
    }

    #[test]
    fn other_never_goes_negative_when_measurements_overlap() {
        let start = Instant::now();
        let mut accounting = LoopAccounting::new(Duration::from_secs(1), start);
        accounting.add_work(LoopWork::Draw, Duration::from_millis(900));
        accounting.add_wait(LoopWait::Idle, Duration::from_millis(200));
        let report = accounting
            .take_report(start + Duration::from_secs(1))
            .expect("window passed");
        assert!(report.contains("other=0;"), "{report}");
    }
}
