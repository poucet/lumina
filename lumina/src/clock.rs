//! Process-relative monotonic clock, shared by the voice heartbeats and
//! `/debug`. Times are stored as milliseconds since process start so they
//! fit in an `AtomicU64`; 0 is reserved for "never".

use std::sync::LazyLock;
use std::time::{Duration, Instant};

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Pin the process start. Call first thing in `main` so uptime is honest.
pub fn init() {
    LazyLock::force(&START);
}

pub fn uptime() -> Duration {
    START.elapsed()
}

/// Milliseconds since process start, never 0.
pub fn now_ms() -> u64 {
    (START.elapsed().as_millis() as u64).max(1)
}

/// Time elapsed since a `now_ms()` stamp.
pub fn since(stamp_ms: u64) -> Duration {
    Duration::from_millis(now_ms().saturating_sub(stamp_ms))
}

/// Compact human age: `42s`, `17m`, `5h`, `3d`.
pub fn fmt_age(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..120 => format!("{s}s"),
        120..7_200 => format!("{}m", s / 60),
        7_200..172_800 => format!("{}h", s / 3_600),
        _ => format!("{}d", s / 86_400),
    }
}
