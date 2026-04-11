use std::time::SystemTime;

use crate::flogf;

pub struct TimeProfiler {
    what: &'static str,
    start: SystemTime,
}

impl TimeProfiler {
    pub fn new(what: &'static str) -> Self {
        let start = SystemTime::now();
        Self { what, start }
    }
}

impl Drop for TimeProfiler {
    fn drop(&mut self) {
        if let Ok(duration) = self.start.elapsed() {
            let ns_per_ms = 1_000_000;
            let ms = duration.as_millis();
            let ns = duration.as_nanos() - (ms * ns_per_ms);
            flogf!(
                profile_history,
                "%s: %d.%06d ms",
                self.what,
                ms as u64, // todo!("remove cast")
                ns as u32
            );
        } else {
            flogf!(profile_history, "%s: ??? ms", self.what);
        }
    }
}
