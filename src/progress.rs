//! Startup progress reporting for the scan phases.
//!
//! Scanning happens before the TUI opens, so this renders to the normal
//! terminal on a single line rewritten in place with `\r`. Everything is
//! written to stderr, which keeps `--report`'s stdout output clean enough to
//! pipe.
//!
//! Nothing is drawn when stderr is not a terminal: progress bars in a log file
//! or a pipe are noise.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Frames of the indeterminate spinner, for phases with no known total.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

const BAR_WIDTH: usize = 24;

/// Redraw rate. Fast enough to look live, slow enough that a scan of thousands
/// of tiny store paths is not dominated by terminal writes.
const TICK: Duration = Duration::from_millis(80);

/// A single-line progress display for one phase of the scan.
///
/// `total` of `None` means the denominator is unknown — a walk that discovers
/// its own extent — so only a count and a spinner are shown. With a total, the
/// display gains a bar, a percentage and an ETA.
pub struct Progress {
    label: &'static str,
    total: Option<usize>,
    done: AtomicUsize,
    /// Micros since `start`, of the last redraw. Atomic so `tick` stays `&self`
    /// and can be called from rayon worker threads.
    last_draw: AtomicU64,
    start: Instant,
    enabled: bool,
}

impl Progress {
    pub fn new(label: &'static str, total: Option<usize>) -> Self {
        let p = Progress {
            label,
            total,
            done: AtomicUsize::new(0),
            last_draw: AtomicU64::new(0),
            start: Instant::now(),
            enabled: std::io::stderr().is_terminal(),
        };
        // An immediate first frame: phases that finish quickly should still
        // show that they happened, and slow ones must not look like a hang.
        p.draw(0);
        p
    }

    /// Record `n` more units of work and redraw if the tick has elapsed.
    ///
    /// Safe to call from many threads at once; at most one of them draws.
    pub fn advance(&self, n: usize) {
        let done = self.done.fetch_add(n, Ordering::Relaxed) + n;
        self.tick(done);
    }

    fn tick(&self, done: usize) {
        if !self.enabled {
            return;
        }
        let elapsed = self.start.elapsed().as_micros() as u64;
        let last = self.last_draw.load(Ordering::Relaxed);
        if elapsed.saturating_sub(last) < TICK.as_micros() as u64 {
            return;
        }
        // Whoever wins this exchange owns the redraw; the others skip it, so
        // concurrent callers never interleave writes on the same line.
        if self
            .last_draw
            .compare_exchange(last, elapsed, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }
        self.draw(done);
    }

    fn draw(&self, done: usize) {
        if !self.enabled {
            return;
        }
        let elapsed = self.start.elapsed();
        let mut err = std::io::stderr().lock();

        let body = match self.total {
            Some(total) => {
                let frac = if total == 0 {
                    1.0
                } else {
                    (done as f64 / total as f64).clamp(0.0, 1.0)
                };
                let filled = (frac * BAR_WIDTH as f64).round() as usize;
                let bar: String = "█".repeat(filled) + &"░".repeat(BAR_WIDTH - filled);
                format!(
                    "{bar} {:>3}%  {done}/{total}  {}",
                    (frac * 100.0) as usize,
                    eta(elapsed, done, total),
                )
            }
            None => format!(
                "{}  {done} found  {}",
                SPINNER[(elapsed.as_millis() / 80) as usize % SPINNER.len()],
                human_duration(elapsed),
            ),
        };

        // \r plus a clear-to-end-of-line: without the clear, a shorter frame
        // leaves the tail of the previous one on screen.
        let _ = write!(err, "\r\x1b[2K  {:<16} {body}", self.label);
        let _ = err.flush();
    }

    /// Overwrite the count. Used when the final figure differs from the number
    /// of `advance` calls — overlapping scan roots can find the same lockfile
    /// twice, and the summary should report the deduplicated count.
    pub fn set(&self, n: usize) {
        self.done.store(n, Ordering::Relaxed);
    }

    /// Finish the phase, leaving a one-line summary and moving to a new line so
    /// the next phase (or the TUI) starts clean.
    pub fn finish(&self) {
        if !self.enabled {
            return;
        }
        let done = self.done.load(Ordering::Relaxed);
        let mut err = std::io::stderr().lock();
        let _ = write!(
            err,
            "\r\x1b[2K  {:<16} {done} in {}\n",
            self.label,
            human_duration(self.start.elapsed())
        );
        let _ = err.flush();
    }
}

/// Remaining time, extrapolated from the average rate so far.
///
/// Deliberately blank until some work is done: an ETA computed from one sample
/// is worse than no ETA, and store paths vary enough in size that early
/// estimates swing wildly.
fn eta(elapsed: Duration, done: usize, total: usize) -> String {
    if done == 0 || done >= total {
        return String::new();
    }
    let per_item = elapsed.as_secs_f64() / done as f64;
    let remaining = Duration::from_secs_f64(per_item * (total - done) as f64);
    format!("ETA {}", human_duration(remaining))
}

fn human_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else if secs >= 10 {
        format!("{secs}s")
    } else {
        format!("{:.1}s", d.as_secs_f64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eta_is_blank_before_any_progress() {
        // One sample would extrapolate from nothing; better to show nothing.
        assert_eq!(eta(Duration::from_secs(1), 0, 10), "");
    }

    #[test]
    fn eta_is_blank_when_complete() {
        assert_eq!(eta(Duration::from_secs(1), 10, 10), "");
    }

    #[test]
    fn eta_extrapolates_from_average_rate() {
        // 2 of 10 done in 4s → 2s per item → 16s left.
        assert_eq!(eta(Duration::from_secs(4), 2, 10), "ETA 16s");
    }

    #[test]
    fn durations_switch_units() {
        assert_eq!(human_duration(Duration::from_millis(1500)), "1.5s");
        assert_eq!(human_duration(Duration::from_secs(42)), "42s");
        assert_eq!(human_duration(Duration::from_secs(125)), "2m05s");
    }

    #[test]
    fn advance_accumulates_across_calls() {
        let p = Progress::new("test", Some(10));
        p.advance(3);
        p.advance(4);
        assert_eq!(p.done.load(Ordering::Relaxed), 7);
    }
}
