//! Live modes: output that keeps updating while the input changes.
//!
//! Terminal input, signals, stdin or file changes, and a render worker each run on their own
//! thread and feed one channel. The main loop owns all terminal output, and sleeps until a
//! message arrives or a deadline passes.

pub mod stream;
pub mod viewer;
pub mod watch;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvError, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};
use signal_hook::iterator::{Handle, Signals};

use crate::display::Setup;
use crate::engine::{Control, Failure};
use crate::render::{self, Frame, Renderer};
use crate::size::{Fit, Grid};
use crate::term::input::{Decoder, Event, Key};
use crate::term::tty::{RawMode, Tty};
use crate::term::{self, kitty, probe};
use crate::theme::Palette;

/// After a light/dark switch, color replies are collected this long before re-rendering.
const RECOLOR_DELAY: Duration = Duration::from_millis(150);
/// How often watched files are checked for changes.
const POLL: Duration = Duration::from_millis(150);

pub enum Msg {
    Input(Event),
    Signal(i32),
    Stdin(Vec<u8>),
    StdinEnd,
    Changed,
    Rendered(Result<Frame, Failure>),
    Viewed(Box<viewer::Viewed>),
}

/// Waits for the next message, until `deadline` if there is one. `Ok(None)` means the deadline
/// passed first; `Err` means every sender is gone.
pub fn receive(
    inbox: &Receiver<Msg>,
    deadline: Option<Instant>,
) -> Result<Option<Msg>, RecvError> {
    let Some(deadline) = deadline else {
        return inbox.recv().map(Some);
    };
    match inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(message) => Ok(Some(message)),
        Err(RecvTimeoutError::Timeout) => Ok(None),
        Err(RecvTimeoutError::Disconnected) => Err(RecvError),
    }
}

/// Runs jobs on its own thread, one at a time. The main loop waits for a job's result before
/// submitting the next, so changes that arrive meanwhile are rendered together.
pub struct Worker<J> {
    jobs: Option<Sender<(J, Control)>>,
    running: Option<Control>,
    thread: Option<JoinHandle<()>>,
}

impl<J: Send + 'static> Worker<J> {
    /// Starts a thread that calls `work` for each job and sends what it returns.
    pub fn spawn(
        messages: Sender<Msg>,
        mut work: impl FnMut(J, Control) -> Msg + Send + 'static,
    ) -> Worker<J> {
        let (jobs, queue) = mpsc::channel();
        let thread = thread::spawn(move || {
            for (job, control) in queue {
                if messages.send(work(job, control)).is_err() {
                    return;
                }
            }
        });
        Worker {
            jobs: Some(jobs),
            running: None,
            thread: Some(thread),
        }
    }

    pub fn submit(&mut self, job: J) {
        let control = Control::new();
        if let Some(jobs) = &self.jobs {
            let _ = jobs.send((job, control.clone()));
        }
        self.running = Some(control);
    }
}

impl<J> Worker<J> {
    /// Stops the job in progress early. Its result is `Failure::Cancelled`.
    pub fn cancel(&self) {
        if let Some(control) = &self.running {
            control.cancel();
        }
    }
}

impl<J> Drop for Worker<J> {
    fn drop(&mut self) {
        self.cancel();
        self.jobs = None;
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A render for the stream and watch modes.
pub struct Job {
    pub source: String,
    pub grid: Grid,
    pub fit: Fit,
    /// A new Mermaid configuration to switch to first, after a theme change.
    pub config: Option<Value>,
}

/// Starts the worker that renders frames for the stream and watch modes.
pub fn renderer(setup: &Setup, messages: Sender<Msg>) -> Worker<Job> {
    let mut renderer = Renderer::new(setup.config.clone(), setup.background);
    let scale = setup.scale;
    Worker::spawn(messages, move |job: Job, control| {
        if let Some(config) = job.config {
            renderer.set_config(config);
        }
        Msg::Rendered(renderer.frame(&job.source, &job.grid, scale, job.fit, control))
    })
}

fn spawn_input(tty: Tty, messages: Sender<Msg>, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut decoder = Decoder::default();
        let mut buf = [0u8; 4096];
        while !stop.load(Ordering::Relaxed) {
            // A lone ESC is the Escape key unless the rest of a sequence follows quickly.
            let wait = if decoder.pending_escape() { 30 } else { 100 };
            let events = match tty.read_timeout(&mut buf, Duration::from_millis(wait)) {
                Ok(Some(0)) | Err(_) => return,
                Ok(Some(n)) => decoder.feed(&buf[..n]),
                Ok(None) => decoder.flush().into_iter().collect(),
            };
            for event in events {
                if messages.send(Msg::Input(event)).is_err() {
                    return;
                }
            }
        }
    })
}

/// Forwards the signals live modes handle to the main loop, until the handle is closed.
fn spawn_signals(messages: Sender<Msg>) -> io::Result<Handle> {
    let mut signals = Signals::new([SIGINT, SIGTERM, SIGHUP, SIGWINCH])?;
    let handle = signals.handle();
    thread::spawn(move || {
        for signal in signals.forever() {
            if messages.send(Msg::Signal(signal)).is_err() {
                return;
            }
        }
    });
    Ok(handle)
}

/// Polls file size and modification time, which also catches editors that save by renaming.
pub fn spawn_poller(paths: Vec<PathBuf>, messages: Sender<Msg>, stop: Arc<AtomicBool>) {
    thread::spawn(move || {
        let stamps = || -> Vec<_> {
            paths
                .iter()
                .map(|path| fs::metadata(path).ok().map(|m| (m.len(), m.modified().ok())))
                .collect()
        };
        let mut last = stamps();
        while !stop.load(Ordering::Relaxed) {
            thread::sleep(POLL);
            let now = stamps();
            if now != last {
                last = now;
                if messages.send(Msg::Changed).is_err() {
                    return;
                }
            }
        }
    });
}

/// What the main loop should do after an event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Continue,
    /// The grid or the colors changed: render again.
    Redraw,
    Quit(u8),
}

impl Outcome {
    pub fn or(self, other: Outcome) -> Outcome {
        match (self, other) {
            (Outcome::Quit(code), _) | (_, Outcome::Quit(code)) => Outcome::Quit(code),
            (Outcome::Redraw, _) | (_, Outcome::Redraw) => Outcome::Redraw,
            _ => Outcome::Continue,
        }
    }
}

/// The terminal state shared by live modes: raw input, signals, colors and the region.
pub struct Session {
    pub region: Region,
    pub grid: Grid,
    setup: Setup,
    tty: Tty,
    palette: Palette,
    recolor_at: Option<Instant>,
    config: Option<Value>,
    signals: Handle,
    stop_input: Arc<AtomicBool>,
    input: Option<JoinHandle<()>>,
    _raw: RawMode,
}

impl Session {
    pub fn start(setup: &Setup, messages: Sender<Msg>) -> Result<Session> {
        let tty = setup.tty.clone().context("live output needs a terminal")?;
        let raw = tty.raw(true)?;
        let signals = spawn_signals(messages.clone())?;
        let stop_input = Arc::new(AtomicBool::new(false));
        let input = spawn_input(tty.clone(), messages, Arc::clone(&stop_input));
        write_stdout(b"\x1b[?25l\x1b[?2031h\x1b[?2048h")?;
        Ok(Session {
            region: Region::default(),
            grid: setup.grid,
            setup: setup.clone(),
            palette: setup.caps.palette.clone(),
            tty,
            recolor_at: None,
            config: None,
            signals,
            stop_input,
            input: Some(input),
            _raw: raw,
        })
    }

    pub fn draw(
        &mut self,
        above: &[u8],
        picture: Picture<'_>,
        notes: &[String],
        status: Option<&str>,
    ) -> io::Result<()> {
        let out = self.region.draw(above, picture, notes, status, self.grid.cols);
        write_stdout(&out)
    }

    /// Keeps what the region shows as ordinary output, without the status line.
    pub fn release(&mut self, notes: &[String]) -> io::Result<()> {
        self.draw(&[], Picture::Keep, notes, None)?;
        self.region.forget();
        Ok(())
    }

    /// A render job, carrying the new configuration after a theme change.
    pub fn job(&mut self, source: String, grid: Grid, fit: Fit) -> Job {
        Job {
            source,
            grid,
            fit,
            config: self.config.take(),
        }
    }

    /// The configuration to switch to after a theme change, handed out once.
    pub fn take_config(&mut self) -> Option<Value> {
        self.config.take()
    }

    /// Handles the input every live mode shares: quitting, resizing and color changes.
    pub fn handle(&mut self, event: &Event) -> Outcome {
        match *event {
            Event::Key(Key::Ctrl('c')) => Outcome::Quit(130),
            Event::Key(Key::Char('q') | Key::Escape) => Outcome::Quit(0),
            Event::Resize {
                cols,
                rows,
                width,
                height,
            } => self.resize(cols, rows, width, height),
            Event::ColorScheme { .. } if self.setup.theme == "terminal" => {
                let _ = self.tty.write_all(probe::color_queries().as_bytes());
                self.recolor_at = Some(Instant::now() + RECOLOR_DELAY);
                Outcome::Continue
            }
            Event::Color { index, rgb } => {
                self.palette.set(index, rgb);
                if self.recolor_at.is_some() {
                    self.recolor_at = Some(Instant::now() + RECOLOR_DELAY);
                }
                Outcome::Continue
            }
            _ => Outcome::Continue,
        }
    }

    /// Handles a signal: SIGWINCH resizes, and the others quit.
    pub fn signal(&mut self, signal: i32) -> Outcome {
        if signal != SIGWINCH {
            return Outcome::Quit(130);
        }
        let Ok(size) = self.tty.winsize() else {
            return Outcome::Continue;
        };
        self.resize(size.ws_col, size.ws_row, size.ws_xpixel.into(), size.ws_ypixel.into())
    }

    /// Switches to the terminal's new colors once their replies have settled. Call on every
    /// pass of the main loop.
    pub fn tick(&mut self) -> Outcome {
        if self.recolor_at.is_some_and(|at| Instant::now() >= at) {
            self.recolor_at = None;
            self.config = Some(self.setup.config_for(&self.palette));
            return Outcome::Redraw;
        }
        Outcome::Continue
    }

    /// When `tick` next has something to do.
    pub fn deadline(&self) -> Option<Instant> {
        self.recolor_at
    }

    fn resize(&mut self, cols: u16, rows: u16, width: u32, height: u32) -> Outcome {
        if cols == 0 || rows == 0 {
            return Outcome::Continue;
        }
        let mut grid = Grid { cols, rows, ..self.grid };
        if width / u32::from(cols) > 0 && height / u32::from(rows) > 0 {
            grid.cell_w = width / u32::from(cols);
            grid.cell_h = height / u32::from(rows);
        }
        if grid == self.grid {
            return Outcome::Continue;
        }
        self.grid = grid;
        Outcome::Redraw
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = write_stdout(b"\x1b[?2048l\x1b[?2031l\x1b[?25h");
        self.signals.close();
        self.stop_input.store(true, Ordering::Relaxed);
        if let Some(input) = self.input.take() {
            let _ = input.join();
        }
    }
}

fn write_stdout(bytes: &[u8]) -> io::Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(&term::for_terminal(bytes))?;
    out.flush()
}

pub fn dim(text: &str) -> String {
    format!("\x1b[2m{text}\x1b[22m")
}

pub enum Picture<'a> {
    /// Show a new frame.
    New(&'a Frame),
    /// Keep showing the current frame.
    Keep,
    /// Show no frame.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Shown {
    id: u32,
    cols: u16,
    rows: u16,
}

/// Lines at the bottom of the output that are redrawn in place. The cursor rests at the start
/// of the line below them.
#[derive(Default)]
pub struct Region {
    lines: u16,
    image: Option<Shown>,
}

impl Region {
    /// Erases the region, prints `above` as ordinary output that scrolls into the history, and
    /// draws the picture, `notes` and `status` as the new region. Lines are cut to `width` so
    /// none of them wraps.
    pub fn draw(
        &mut self,
        above: &[u8],
        picture: Picture<'_>,
        notes: &[String],
        status: Option<&str>,
        width: u16,
    ) -> Vec<u8> {
        let mut out = b"\x1b[?2026h".to_vec();
        if self.lines > 0 {
            let _ = write!(out, "\x1b[{}F", self.lines);
        }
        out.extend_from_slice(b"\x1b[J");
        out.extend_from_slice(above);

        let previous = self.image;
        match picture {
            Picture::New(frame) => {
                let id = kitty::random_id();
                let _ = render::write_inline(&mut out, frame, id);
                self.image = Some(Shown {
                    id,
                    cols: frame.cols,
                    rows: frame.rows,
                });
            }
            Picture::Keep => {
                if let Some(shown) = self.image {
                    render::write_cells(&mut out, shown.id, shown.cols, shown.rows);
                }
            }
            Picture::None => self.image = None,
        }
        let mut lines = self.image.map_or(0, |shown| shown.rows);
        for line in notes.iter().map(String::as_str).chain(status) {
            out.extend_from_slice(fit_line(line, width).as_bytes());
            out.push(b'\n');
            lines += 1;
        }
        if let Some(old) = previous
            && self.image.map(|shown| shown.id) != Some(old.id)
        {
            let _ = kitty::delete(&mut out, old.id);
        }
        self.lines = lines;
        out.extend_from_slice(b"\x1b[?2026l");
        out
    }

    /// Leaves the current content as ordinary output; the next draw starts below it.
    pub fn forget(&mut self) {
        self.lines = 0;
        self.image = None;
    }

    pub fn has_image(&self) -> bool {
        self.image.is_some()
    }

    /// Rows taken by the frame currently shown.
    pub fn lines_for_image(&self) -> u16 {
        self.image.map_or(0, |shown| shown.rows)
    }
}

/// Cuts a line to fewer than `width` columns, ignoring escape sequences, so it never wraps.
fn fit_line(line: &str, width: u16) -> String {
    let max = usize::from(width).saturating_sub(1).max(1);
    let mut visible = 0;
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            for next in chars.by_ref() {
                out.push(next);
                if next.is_ascii_alphabetic() || next == '~' {
                    break;
                }
            }
            continue;
        }
        if visible + 1 == max && chars.clone().any(|rest| rest != '\x1b') {
            out.push('…');
            out.push_str("\x1b[0m");
            return out;
        }
        out.push(c);
        visible += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn frame(cols: u16, rows: u16) -> Frame {
        Frame {
            rgba: vec![0; usize::from(cols) * usize::from(rows) * 4],
            pixels: (u32::from(cols), u32::from(rows)),
            cols,
            rows,
            elapsed: Duration::ZERO,
        }
    }

    fn text(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).into_owned()
    }

    #[test]
    fn regions_redraw_in_place() {
        let mut region = Region::default();
        let first = text(&region.draw(b"", Picture::New(&frame(3, 2)), &[], Some("status"), 80));
        assert!(first.starts_with("\x1b[?2026h\x1b[J\x1b_G"));
        assert!(first.ends_with("status\n\x1b[?2026l"));
        let shown = region.image.unwrap();

        let kept = text(&region.draw(b"", Picture::Keep, &["note".to_string()], Some("s2"), 80));
        assert!(kept.starts_with("\x1b[?2026h\x1b[3F\x1b[J"));
        assert!(!kept.contains("\x1b_G"), "the kept frame is not sent again");
        assert_eq!(region.image, Some(shown));
        assert_eq!(region.lines, 4);

        let replaced = text(&region.draw(b"", Picture::New(&frame(1, 1)), &[], None, 80));
        assert!(replaced.starts_with("\x1b[?2026h\x1b[4F\x1b[J"));
        assert!(replaced.contains(&format!("\x1b_Ga=d,d=I,i={},q=2", shown.id)));
        assert_eq!(region.lines, 1);
    }

    #[test]
    fn content_above_becomes_history() {
        let mut region = Region::default();
        region.draw(b"", Picture::None, &[], Some("waiting"), 80);
        let out = text(&region.draw(b"done\n", Picture::None, &[], Some("waiting"), 80));
        assert!(out.starts_with("\x1b[?2026h\x1b[1F\x1b[Jdone\nwaiting\n"));
        region.forget();
        let next = text(&region.draw(b"", Picture::None, &[], Some("x"), 80));
        assert!(next.starts_with("\x1b[?2026h\x1b[J"));
    }

    #[test]
    fn long_lines_are_cut_without_counting_escapes() {
        assert_eq!(fit_line("short", 80), "short");
        assert_eq!(fit_line("abcdefghij", 6), "abcd…\x1b[0m");
        assert_eq!(fit_line("\x1b[2mabcdef\x1b[22m", 6), "\x1b[2mabcd…\x1b[0m");
        assert_eq!(fit_line("\x1b[2mabcd\x1b[22m", 6), "\x1b[2mabcd\x1b[22m");
    }

    #[test]
    fn quitting_wins_over_redrawing() {
        assert_eq!(Outcome::Redraw.or(Outcome::Quit(0)), Outcome::Quit(0));
        assert_eq!(Outcome::Continue.or(Outcome::Redraw), Outcome::Redraw);
        assert_eq!(Outcome::Continue.or(Outcome::Continue), Outcome::Continue);
    }

    #[test]
    fn dropping_a_worker_cancels_its_job() {
        let (messages, inbox) = mpsc::channel();
        let (started, running) = mpsc::channel();
        let mut worker = Worker::spawn(messages, move |(), control: Control| {
            started.send(()).unwrap();
            while !control.is_cancelled() {
                thread::sleep(Duration::from_millis(1));
            }
            Msg::Changed
        });
        worker.submit(());
        running.recv().unwrap();
        drop(worker);
        assert!(matches!(inbox.try_recv(), Ok(Msg::Changed)));
    }
}
