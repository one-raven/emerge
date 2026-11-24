use std::sync::Mutex;

use rustler::LocalPid;

use crate::events::send_log_event;

#[cfg_attr(not(all(feature = "drm", target_os = "linux")), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeLogLevel {
    Debug,
    Info,
    Warning,
    Error,
}

#[derive(Default)]
pub struct NativeLogRelay {
    target: Mutex<Option<LocalPid>>,
}

impl NativeLogRelay {
    #[cfg_attr(
        not(any(
            all(feature = "wayland", target_os = "linux"),
            all(feature = "drm", target_os = "linux")
        )),
        allow(dead_code)
    )]
    pub fn new(target: Option<LocalPid>) -> Self {
        report_panics_to(target);
        Self {
            target: Mutex::new(target),
        }
    }

    pub fn set_target(&self, target: Option<LocalPid>) {
        report_panics_to(target);
        let mut guard = self
            .target
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *guard = target;
    }

    #[cfg_attr(not(all(feature = "drm", target_os = "linux")), allow(dead_code))]
    pub fn log(&self, level: NativeLogLevel, source: &'static str, message: impl Into<String>) {
        let target = *self
            .target
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());

        if let Some(pid) = target {
            send_log_event(pid, level, source, &message.into());
        }
    }

    #[cfg_attr(not(all(feature = "drm", target_os = "linux")), allow(dead_code))]
    pub fn debug(&self, source: &'static str, message: impl Into<String>) {
        self.log(NativeLogLevel::Debug, source, message);
    }

    #[cfg_attr(not(all(feature = "drm", target_os = "linux")), allow(dead_code))]
    pub fn info(&self, source: &'static str, message: impl Into<String>) {
        self.log(NativeLogLevel::Info, source, message);
    }

    #[cfg_attr(not(all(feature = "drm", target_os = "linux")), allow(dead_code))]
    pub fn warning(&self, source: &'static str, message: impl Into<String>) {
        self.log(NativeLogLevel::Warning, source, message);
    }

    #[cfg_attr(not(all(feature = "drm", target_os = "linux")), allow(dead_code))]
    pub fn error(&self, source: &'static str, message: impl Into<String>) {
        self.log(NativeLogLevel::Error, source, message);
    }
}

/// One Raven patch: where a panic on any native thread is reported (the
/// latest log target). A renderer thread's panic was caught and the renderer
/// restarted with no trace of why: its message went to stderr, which a
/// device usually discards.
static PANIC_TARGET: Mutex<Option<LocalPid>> = Mutex::new(None);

fn report_panics_to(target: Option<LocalPid>) {
    static HOOK: std::sync::Once = std::sync::Once::new();
    if target.is_none() {
        return;
    }
    *PANIC_TARGET
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = target;
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let target = *PANIC_TARGET
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if let Some(pid) = target {
                let thread = std::thread::current();
                let payload = info
                    .payload()
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| info.payload().downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "(no message)".into());
                let location = info
                    .location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_default();
                // Release builds are stripped: the load address lets the
                // frames be resolved against an unstripped build.
                let base = std::fs::read_to_string("/proc/self/maps")
                    .ok()
                    .and_then(|maps| {
                        maps.lines()
                            .find(|line| line.contains("emerge_skia") && line.contains("r-xp"))
                            .map(str::to_string)
                    })
                    .unwrap_or_default();
                send_log_event(
                    pid,
                    NativeLogLevel::Error,
                    "panic",
                    &format!(
                        "thread {:?} panicked at {location}: {payload}\nmapped: {base}\n{}",
                        thread.name().unwrap_or("?"),
                        std::backtrace::Backtrace::force_capture()
                    ),
                );
            }
            previous(info);
        }));
    });
}
