//! Process-wide monotonic clock in microseconds.
//!
//! Every timestamp that crosses the wire is in this time base; peers
//! translate between bases with the offset estimated by `net::media` pings.

use std::sync::OnceLock;
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

pub fn init() {
    let _ = START.get_or_init(Instant::now);
    #[cfg(windows)]
    unsafe {
        // 1 ms scheduler granularity so short sleeps/timeouts are honoured.
        windows_sys::Win32::Media::timeBeginPeriod(1);
    }
}

pub fn now_us() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_micros() as u64
}
