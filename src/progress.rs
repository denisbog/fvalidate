//! A tiny, dependency-free progress bar rendered on stderr.
//!
//! The engine feeds it bytes read; it only draws while stderr is a terminal and
//! redraws at most every [`REDRAW`]. Because it lives on stderr it never
//! interferes with the report written to stdout.

use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BAR_WIDTH: usize = 30;
const REDRAW: Duration = Duration::from_millis(100);
/// Only consider a redraw once this many bytes accumulated, keeping
/// `Instant::now()` and the draw mutex out of the per-read hot path.
const REDRAW_BYTES: u64 = 256 * 1024;

#[derive(Debug)]
pub struct Progress {
    enabled: bool,
    total: AtomicU64,
    done: AtomicU64,
    started: Instant,
    since_draw: AtomicU64,
    last_draw: Mutex<Instant>,
    draw_lock: Mutex<()>,
    finished: AtomicBool,
}

impl Progress {
    /// `enabled` is normally `stderr.is_terminal() && !--no-progress`.
    pub fn new(enabled: bool) -> Arc<Self> {
        Arc::new(Progress {
            enabled,
            total: AtomicU64::new(0),
            done: AtomicU64::new(0),
            started: Instant::now(),
            since_draw: AtomicU64::new(0),
            last_draw: Mutex::new(Instant::now()),
            draw_lock: Mutex::new(()),
            finished: AtomicBool::new(false),
        })
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    pub fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    /// Record `delta` processed units (bytes) and redraw if enough time passed.
    pub fn tick(&self, delta: u64) {
        self.done.fetch_add(delta, Ordering::Relaxed);
        if !self.enabled || delta == 0 || self.finished.load(Ordering::Relaxed) {
            return;
        }
        let since = self.since_draw.fetch_add(delta, Ordering::Relaxed) + delta;
        if since < REDRAW_BYTES {
            return;
        }
        self.since_draw.store(0, Ordering::Relaxed);
        let now = Instant::now();
        {
            let mut last = self.last_draw.lock().unwrap();
            if now.duration_since(*last) < REDRAW {
                return;
            }
            *last = now;
        }
        self.draw();
    }

    /// Clear the bar and print a one-line summary.
    pub fn finish(&self) {
        if !self.enabled || self.finished.swap(true, Ordering::Relaxed) {
            return;
        }
        self.draw();
        let done = self.done.load(Ordering::Relaxed);
        let elapsed = self.started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            done as f64 / elapsed
        } else {
            0.0
        };
        let mut stderr = io::stderr().lock();
        let _ = writeln!(
            stderr,
            "Processed {} in {:.2}s ({}/s)",
            human_bytes(done),
            elapsed,
            human_bytes(rate.max(0.0) as u64)
        );
    }

    fn draw(&self) {
        let _guard = self.draw_lock.lock().unwrap();
        let done = self.done.load(Ordering::Relaxed);
        let total = self.total.load(Ordering::Relaxed);
        let elapsed = self.started.elapsed().as_secs_f64();
        let rate = if elapsed > 0.0 {
            done as f64 / elapsed
        } else {
            0.0
        };

        let ratio = if total > 0 {
            (done as f64 / total as f64).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let filled = (ratio * BAR_WIDTH as f64).round() as usize;

        let mut line = String::with_capacity(BAR_WIDTH + 64);
        line.push('[');
        for position in 0..BAR_WIDTH {
            line.push(if position < filled {
                '='
            } else if position == filled && filled < BAR_WIDTH {
                '>'
            } else {
                ' '
            });
        }
        line.push(']');
        line.push(' ');

        if total > 0 {
            let eta = if rate > 0.0 && total > done {
                format_duration((total - done) as f64 / rate)
            } else {
                "00:00".to_string()
            };
            line.push_str(&format!(
                "{:>3.0}% {} / {}  {}/s  ETA {}",
                ratio * 100.0,
                human_bytes(done),
                human_bytes(total),
                human_bytes(rate as u64),
                eta
            ));
        } else {
            line.push_str(&format!(
                "{}  {}/s",
                human_bytes(done),
                human_bytes(rate as u64)
            ));
        }

        let mut stderr = io::stderr().lock();
        // `\r` returns to the start, `\x1b[K` clears any leftovers.
        let _ = write!(stderr, "\r{line}\x1b[K");
        let _ = stderr.flush();
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if !self.enabled || self.finished.load(Ordering::Relaxed) {
            return;
        }
        let mut stderr = io::stderr().lock();
        let _ = write!(stderr, "\r\x1b[K");
        let _ = stderr.flush();
    }
}

/// Whether a progress bar can be drawn (stderr is an interactive terminal).
pub fn stderr_is_terminal() -> bool {
    io::stderr().is_terminal()
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

fn format_duration(seconds: f64) -> String {
    let seconds = seconds.max(0.0) as u64;
    let (hours, minutes, secs) = (seconds / 3600, (seconds % 3600) / 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes:02}:{secs:02}")
    }
}
