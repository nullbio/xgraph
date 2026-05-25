//! Init-time progress UX: shimmer spinner + gradient bar.
//!
//! Drives a side thread that owns terminal output. The main thread emits
//! coarse phase transitions; the side thread renders an animated line at
//! a fixed cadence (50 ms) and overwrites it with `\r\x1b[K`. When the
//! main thread changes phases, the prior line is flushed to a green
//! checkmark + summary and a new animation begins on the next line.
//!
//! Modeled after codegraph's `ShimmerProgress`. Same visual language:
//! truecolor gradient sweep across the filled portion of a 25-char bar,
//! the spinner rotates every ~450 ms, finish-phase emits the phaseDone
//! glyph.
//!
//! Falls back to a no-op renderer when stdout is not a TTY (CI, redirected
//! output) so test harnesses and log files don't get ANSI noise.

use std::io::{IsTerminal, Write};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};

const ANIM_INTERVAL_MS: u64 = 150;
const FRAMES_PER_GLYPH: u32 = 3;
const TICK_INTERVAL_MS: u64 = 50;
const BAR_WIDTH: u32 = 25;
const SHIMMER_CYCLE_FRAMES: u32 = 24;
const SHIMMER_WIDTH: f64 = 3.0;

const SPINNER_GLYPHS: &[&str] = &["·", "✢", "✳", "✶", "✻", "✽"];
const BAR_FILLED: &str = "█";
const BAR_EMPTY: &str = "░";
const RAIL: &str = "│";
const PHASE_DONE: &str = "◆";
const DASH: &str = "—";

const RST: &str = "\x1b[0m";
const DM: &str = "\x1b[2m";
const GRN: &str = "\x1b[32m";
const BOLD: &str = "\x1b[1m";

/// Coarse phases reported by the indexer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Scanning,
    Parsing,
    Storing,
    Resolving,
}

impl Phase {
    fn label(&self) -> &'static str {
        match self {
            Self::Scanning => "Scanning files",
            Self::Parsing => "Parsing code",
            Self::Storing => "Storing data",
            Self::Resolving => "Resolving refs",
        }
    }
}

#[derive(Debug)]
enum Msg {
    StartPhase { phase: Phase, total: Option<u64> },
    Tick { current: u64 },
    FinishPhase,
    Stop,
}

/// Handle held by the main thread; sends phase / tick events to the
/// renderer thread.
pub struct Progress {
    tx: Option<Sender<Msg>>,
    thread: Option<JoinHandle<()>>,
}

impl Progress {
    /// Start a progress renderer. When stdout is not a TTY this is a no-op:
    /// the returned `Progress` accepts every event but writes nothing.
    pub fn start() -> Self {
        if !std::io::stdout().is_terminal() {
            return Self {
                tx: None,
                thread: None,
            };
        }
        let (tx, rx) = bounded::<Msg>(32);
        let thread = thread::Builder::new()
            .name("xgraph-progress".into())
            .spawn(move || render_loop(rx))
            .ok();
        Self {
            tx: Some(tx),
            thread,
        }
    }

    /// Begin a new phase. `total` is the expected denominator for percent
    /// reporting; pass `None` for indeterminate progress (counts only).
    pub fn phase(&self, phase: Phase, total: Option<u64>) {
        self.send(Msg::StartPhase { phase, total });
    }

    /// Update the current item count within the active phase.
    pub fn tick(&self, current: u64) {
        self.send(Msg::Tick { current });
    }

    /// Finish the current phase, emitting a checkmark line. The next
    /// `phase()` call starts a new animation on a new line.
    pub fn finish_phase(&self) {
        self.send(Msg::FinishPhase);
    }

    /// Stop the renderer thread and flush any in-flight phase.
    pub fn stop(mut self) {
        self.send(Msg::Stop);
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }

    fn send(&self, msg: Msg) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(msg);
        }
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Stop);
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

struct RenderState {
    current_phase: Option<Phase>,
    current_total: Option<u64>,
    current_count: u64,
    start: Instant,
}

fn render_loop(rx: Receiver<Msg>) {
    let mut state = RenderState {
        current_phase: None,
        current_total: None,
        current_count: 0,
        start: Instant::now(),
    };
    loop {
        // Drain any pending messages without blocking, then sleep one
        // tick. Stop on the Stop message; flush the current line first.
        loop {
            match rx.try_recv() {
                Ok(Msg::StartPhase { phase, total }) => {
                    finish_phase(&state);
                    state.current_phase = Some(phase);
                    state.current_total = total;
                    state.current_count = 0;
                    state.start = Instant::now();
                }
                Ok(Msg::Tick { current }) => {
                    state.current_count = current;
                }
                Ok(Msg::FinishPhase) => {
                    finish_phase(&state);
                    state.current_phase = None;
                    state.current_total = None;
                    state.current_count = 0;
                }
                Ok(Msg::Stop) => {
                    finish_phase(&state);
                    return;
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    finish_phase(&state);
                    return;
                }
            }
        }
        if state.current_phase.is_some() {
            render(&state);
        }
        thread::sleep(Duration::from_millis(TICK_INTERVAL_MS));
    }
}

fn render(state: &RenderState) {
    let Some(phase) = state.current_phase else {
        return;
    };
    let frame = anim_frame(state.start);
    let glyph_idx = (frame / FRAMES_PER_GLYPH) as usize % SPINNER_GLYPHS.len();
    let glyph = SPINNER_GLYPHS[glyph_idx];
    let color = shimmer_color(frame);

    let line = match (state.current_total, state.current_count) {
        (Some(total), current) if total > 0 => {
            let percent = ((current as f64 / total as f64) * 100.0).round() as u32;
            let percent = percent.min(100);
            let filled = ((BAR_WIDTH * percent) / 100).min(BAR_WIDTH);
            let empty = BAR_WIDTH - filled;
            let bar = render_bar(frame, filled, empty);
            format!(
                "{DM}{RAIL}{RST}  {color}{glyph}{RST} {label}  {bar}  {percent}%",
                label = phase.label(),
            )
        }
        (None, count) if count > 0 => format!(
            "{DM}{RAIL}{RST}  {color}{glyph}{RST} {label}... {count} found",
            label = phase.label(),
            count = format_number(count),
        ),
        _ => format!(
            "{DM}{RAIL}{RST}  {color}{glyph}{RST} {label}...",
            label = phase.label(),
        ),
    };
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\r\x1b[K{line}");
    let _ = out.flush();
}

fn finish_phase(state: &RenderState) {
    let Some(phase) = state.current_phase else {
        return;
    };
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "\r\x1b[K");
    let detail = match (state.current_total, state.current_count) {
        (Some(_), _) => format!(" {DASH} done"),
        (None, count) if count > 0 => {
            format!(" {DASH} {} found", format_number(count))
        }
        _ => String::new(),
    };
    let _ = writeln!(
        out,
        "{DM}{RAIL}{RST}  {GRN}{PHASE_DONE}{RST} {label}{detail}",
        label = phase.label(),
    );
    let _ = out.flush();
}

fn anim_frame(start: Instant) -> u32 {
    (start.elapsed().as_millis() / ANIM_INTERVAL_MS as u128) as u32
}

fn lerp(a: f64, b: f64, t: f64) -> u32 {
    (a + (b - a) * t).round() as u32
}

fn shimmer_color(frame: u32) -> String {
    let t = (((frame as f64 * 2.0 * std::f64::consts::PI) / 13.0).sin() + 1.0) / 2.0;
    let r = lerp(160.0, 251.0, t);
    let g = lerp(100.0, 191.0, t);
    let b = lerp(9.0, 36.0, t);
    format!("\x1b[38;2;{r};{g};{b}m{BOLD}")
}

fn render_bar(frame: u32, filled: u32, empty: u32) -> String {
    if filled == 0 {
        return format!("{DM}{}{RST}", BAR_EMPTY.repeat(empty as usize));
    }
    let cycle = SHIMMER_CYCLE_FRAMES as f64;
    let shimmer_pos = ((frame as f64 % cycle) / cycle) * (filled as f64 + 6.0) - 3.0;
    let mut bar = String::new();
    for i in 0..filled {
        let dist = (i as f64 - shimmer_pos).abs();
        let t = (1.0 - dist / SHIMMER_WIDTH).max(0.0);
        let r = lerp(160.0, 251.0, t);
        let g = lerp(100.0, 191.0, t);
        let b = lerp(9.0, 36.0, t);
        bar.push_str(&format!("\x1b[38;2;{r};{g};{b}m{BOLD}{BAR_FILLED}"));
    }
    bar.push_str(&format!(
        "{RST}{DM}{}{RST}",
        BAR_EMPTY.repeat(empty as usize)
    ));
    bar
}

fn format_number(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, ch) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_number_inserts_commas() {
        assert_eq!(format_number(0), "0");
        assert_eq!(format_number(42), "42");
        assert_eq!(format_number(1_000), "1,000");
        assert_eq!(format_number(1_234_567), "1,234,567");
    }

    #[test]
    fn phase_labels_are_human_readable() {
        assert_eq!(Phase::Scanning.label(), "Scanning files");
        assert_eq!(Phase::Parsing.label(), "Parsing code");
        assert_eq!(Phase::Storing.label(), "Storing data");
        assert_eq!(Phase::Resolving.label(), "Resolving refs");
    }

    #[test]
    fn progress_is_safe_to_stop_immediately() {
        let p = Progress::start();
        p.stop();
    }

    #[test]
    fn progress_sends_phase_then_stops() {
        let p = Progress::start();
        p.phase(Phase::Scanning, Some(100));
        p.tick(50);
        p.finish_phase();
        p.stop();
    }
}
