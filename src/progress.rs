/*
 * SPDX-FileCopyrightText: 2020 Stalwart Labs LLC <hello@stalw.art>
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 */

//! Optional `--progress` reporting for long import and export runs.
//!
//! A single process-wide reporter, like the logger: it owns one line of the
//! terminal and is written to from whichever thread is accounting results.
//! Disabled by default, in which case every entry point is a no-op.

use std::io::{IsTerminal, Write};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Minimum gap between redraws, so a fast run does not spend its time in
/// `write!` and a piped run does not produce thousands of lines.
const TTY_INTERVAL: Duration = Duration::from_millis(100);
const PIPE_INTERVAL: Duration = Duration::from_secs(5);

static PROGRESS: OnceLock<Progress> = OnceLock::new();

pub fn init(enabled: bool) {
    let _ = PROGRESS.set(Progress::new(enabled));
}

fn get() -> Option<&'static Progress> {
    PROGRESS.get().filter(|p| p.enabled)
}

/// Begin a phase. `total` is the number of items expected, when known.
pub fn start(label: &str, total: Option<u64>) {
    if let Some(p) = get() {
        p.start(label, total);
    }
}

/// Record `n` completed items in the current phase.
pub fn advance(n: u64) {
    if let Some(p) = get() {
        p.advance(n);
    }
}

/// End the current phase, leaving a final line in the scrollback.
pub fn finish() {
    if let Some(p) = get() {
        p.finish();
    }
}

struct Progress {
    enabled: bool,
    tty: bool,
    state: Mutex<Option<Phase>>,
}

struct Phase {
    label: String,
    total: Option<u64>,
    done: u64,
    started: Instant,
    last_render: Instant,
}

impl Progress {
    fn new(enabled: bool) -> Progress {
        Progress {
            enabled,
            tty: std::io::stderr().is_terminal(),
            state: Mutex::new(None),
        }
    }

    fn interval(&self) -> Duration {
        if self.tty { TTY_INTERVAL } else { PIPE_INTERVAL }
    }

    fn start(&self, label: &str, total: Option<u64>) {
        let now = Instant::now();
        let mut guard = lock(&self.state);
        *guard = Some(Phase {
            label: label.to_owned(),
            total,
            done: 0,
            started: now,
            // Force the first redraw rather than waiting out an interval.
            last_render: now - self.interval(),
        });
        if let Some(phase) = guard.as_mut() {
            self.render(phase, false);
        }
    }

    fn advance(&self, n: u64) {
        let mut guard = lock(&self.state);
        let Some(phase) = guard.as_mut() else {
            return;
        };
        phase.done += n;
        if phase.last_render.elapsed() >= self.interval() {
            self.render(phase, false);
        }
    }

    fn finish(&self) {
        let mut guard = lock(&self.state);
        if let Some(phase) = guard.as_mut() {
            self.render(phase, true);
        }
        *guard = None;
    }

    fn render(&self, phase: &mut Phase, final_line: bool) {
        phase.last_render = Instant::now();
        // A piped run only reports on the interval or at the end; the
        // intermediate redraws exist for a terminal that can overwrite them.
        if !self.tty && !final_line && phase.done == 0 {
            return;
        }
        let line = format_line(phase);
        let mut err = std::io::stderr().lock();
        if self.tty {
            let _ = write!(err, "\r\x1b[2K{line}");
            if final_line {
                let _ = writeln!(err);
            }
        } else {
            let _ = writeln!(err, "{line}");
        }
        let _ = err.flush();
    }
}

fn format_line(phase: &Phase) -> String {
    let elapsed = phase.started.elapsed().as_secs_f64();
    let rate = if elapsed > 0.0 {
        phase.done as f64 / elapsed
    } else {
        0.0
    };
    match phase.total {
        Some(total) if total > 0 => {
            let pct = (phase.done as f64 / total as f64 * 100.0).min(100.0);
            format!(
                "{}: {}/{} ({pct:.0}%) {rate:.1}/s eta {}",
                phase.label,
                phase.done,
                total,
                eta(phase.done, total, rate),
            )
        }
        _ => format!("{}: {} ({rate:.1}/s)", phase.label, phase.done),
    }
}

fn eta(done: u64, total: u64, rate: f64) -> String {
    if rate <= 0.0 || done >= total {
        return "--:--".to_owned();
    }
    let secs = ((total - done) as f64 / rate).round() as u64;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match m.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(done: u64, total: Option<u64>) -> Phase {
        Phase {
            label: "Email".to_owned(),
            total,
            done,
            started: Instant::now() - Duration::from_secs(10),
            last_render: Instant::now(),
        }
    }

    #[test]
    fn line_reports_percentage_rate_and_eta() {
        let line = format_line(&phase(100, Some(400)));
        assert!(line.starts_with("Email: 100/400 (25%)"), "{line}");
        assert!(line.contains("10.0/s"), "{line}");
        assert!(line.contains("eta 00:30"), "{line}");
    }

    #[test]
    fn line_without_total_omits_percentage() {
        let line = format_line(&phase(42, None));
        assert_eq!(line, "Email: 42 (4.2/s)");
    }

    #[test]
    fn eta_is_unknown_at_zero_rate_or_when_complete() {
        assert_eq!(eta(0, 100, 0.0), "--:--");
        assert_eq!(eta(100, 100, 5.0), "--:--");
    }

    #[test]
    fn percentage_never_exceeds_one_hundred() {
        let line = format_line(&phase(500, Some(400)));
        assert!(line.contains("(100%)"), "{line}");
    }

    #[test]
    fn disabled_reporter_is_inert() {
        let p = Progress::new(false);
        assert!(!p.enabled);
        p.start("Email", Some(10));
        p.advance(1);
        p.finish();
    }
}
