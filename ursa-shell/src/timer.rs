//! `.timer on`: how long a statement took, as DuckDB reports it: the wall
//! time (`real`) and the CPU time the process spent in user code and in the
//! kernel (`user`, `sys`), summed over its threads, so a parallel query can
//! use more CPU time than real time.

use std::time::Duration;

// std's Instant panics in the browser (wasm32-unknown-unknown); web-time's
// is std's elsewhere and reads performance.now() there.
use web_time::Instant;

/// The times when a statement started.
pub struct Timer {
    wall: Instant,
    cpu: Option<CpuTime>,
}

impl Timer {
    pub fn start() -> Self {
        Self {
            wall: Instant::now(),
            cpu: CpuTime::now(),
        }
    }

    /// `Run Time (s): real 0.012 user 0.030000 sys 0.002000`, since
    /// [`Self::start`]. Where the process's CPU time is not known, only the
    /// real time.
    pub fn report(&self) -> String {
        let real = self.wall.elapsed().as_secs_f64();
        match (&self.cpu, CpuTime::now()) {
            (Some(start), Some(end)) => {
                let user = end.user.saturating_sub(start.user).as_secs_f64();
                let sys = end.sys.saturating_sub(start.sys).as_secs_f64();
                format!("Run Time (s): real {real:.3} user {user:.6} sys {sys:.6}")
            }
            _ => format!("Run Time (s): real {real:.3}"),
        }
    }
}

/// The CPU time the process has used so far.
struct CpuTime {
    user: Duration,
    sys: Duration,
}

impl CpuTime {
    #[cfg(unix)]
    fn now() -> Option<Self> {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: getrusage writes a whole `rusage` to the pointer it is given
        // when it returns 0, and it is read only then.
        let usage = unsafe {
            if libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) != 0 {
                return None;
            }
            usage.assume_init()
        };
        let duration = |t: libc::timeval| {
            Duration::from_secs(u64::try_from(t.tv_sec).unwrap_or(0))
                + Duration::from_micros(u64::try_from(t.tv_usec).unwrap_or(0))
        };
        Some(Self {
            user: duration(usage.ru_utime),
            sys: duration(usage.ru_stime),
        })
    }

    #[cfg(not(unix))]
    fn now() -> Option<Self> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_real_user_and_sys() {
        let timer = Timer::start();
        // Spend a little CPU time.
        let mut x = 0u64;
        for i in 0..2_000_000u64 {
            x = x.wrapping_add(i * i);
        }
        std::hint::black_box(x);
        let report = timer.report();
        let words: Vec<&str> = report.split(' ').collect();
        assert_eq!(&words[..3], ["Run", "Time", "(s):"], "{report}");
        assert_eq!(words[3], "real", "{report}");
        assert!(words[4].parse::<f64>().is_ok(), "{report}");
        if cfg!(unix) {
            assert_eq!((words[5], words[7]), ("user", "sys"), "{report}");
            assert!(words[6].parse::<f64>().is_ok() && words[8].parse::<f64>().is_ok());
        }
    }
}
