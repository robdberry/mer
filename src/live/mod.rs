//! Live modes: output that keeps updating while the input changes.
//!
//! Terminal input, stdin or file changes, and a render worker each run on their own thread and
//! feed one channel. The main loop owns all terminal output.

pub mod stream;
pub mod viewer;
pub mod watch;

use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::Value;
use signal_hook::consts::{SIGHUP, SIGINT, SIGTERM, SIGWINCH};

use crate::display::Setup;
use crate::engine::{Control, Failure};
use crate::render::{self, Frame, Renderer};
use crate::size::{Fit, Grid};
use crate::term::input::{Decoder, Event, Key};
use crate::term::tty::{RawMode, Tty};
use crate::term::{self, kitty, probe};
use crate::theme::{Palette, Rgb};

/// How often the main loop wakes up when nothing arrives.
pub const TICK: Duration = Duration::from_millis(25);
/// After a light/dark switch, color replies are collected this long before re-rendering.
const RECOLOR_DELAY: Duration = Duration::from_millis(150);
/// How often watched files are checked for changes.
const POLL: Duration = Duration::from_millis(150);

pub enum Msg {
    Input(Event),
    Stdin(Vec<u8>),
    StdinEnd,
    Changed,
    Rendered(Result<Frame, Failure>),
    Viewed(Box<viewer::Viewed>),
}

pub struct Job {
    pub source: String,
    pub grid: Grid,
    pub scale: f32,
    pub fit: Fit,
    /// A new Mermaid configuration to switch to first, after a theme change.
    pub config: Option<Value>,
}

#[derive(Default)]
struct Slot {
    job: Option<Job>,
    running: Option<Control>,
    closed: bool,
}

/// Renders on its own thread, one job at a time. A new job replaces one that hasn't started.
pub struct Worker {
    shared: Arc<(Mutex<Slot>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Worker {
    pub fn spawn(config: Value, background: Option<Rgb>, messages: Sender<Msg>) -> Worker {
        let shared: Arc<(Mutex<Slot>, Condvar)> = Arc::default();
        let thread = thread::spawn({
            let shared = Arc::clone(&shared);
            move || {
                let mut renderer = Renderer::new(config, background);
                let (lock, ready) = &*shared;
                loop {
                    let (job, control) = {
                        let mut slot = lock.lock().unwrap_or_else(PoisonError::into_inner);
                        loop {
                            if slot.closed {
                                return;
                            }
                            if let Some(job) = slot.job.take() {
                                let control = Control::new();
                                slot.running = Some(control.clone());
                                break (job, control);
                            }
                            slot = ready.wait(slot).unwrap_or_else(PoisonError::into_inner);
                        }
                    };
                    if let Some(config) = job.config {
                        renderer = Renderer::new(config, background);
                    }
                    let result =
                        renderer.frame(&job.source, &job.grid, job.scale, job.fit, control);
                    lock.lock().unwrap_or_else(PoisonError::into_inner).running = None;
                    if messages.send(Msg::Rendered(result)).is_err() {
                        return;
                    }
                }
            }
        });
        Worker {
            shared,
            thread: Some(thread),
        }
    }

    pub fn submit(&self, mut job: Job) {
        let (lock, ready) = &*self.shared;
        let mut slot = lock.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(replaced) = slot.job.take() {
            job.config = job.config.or(replaced.config);
        }
        slot.job = Some(job);
        ready.notify_one();
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        let (lock, ready) = &*self.shared;
        {
            let mut slot = lock.lock().unwrap_or_else(PoisonError::into_inner);
            slot.closed = true;
            if let Some(control) = &slot.running {
                control.cancel();
            }
        }
        ready.notify_one();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// A one-job slot between the main loop and a worker thread. A job that hasn't been taken yet
/// is merged into the job that replaces it.
pub struct Mailbox<J> {
    shared: Arc<(Mutex<MailboxState<J>>, Condvar)>,
}

struct MailboxState<J> {
    job: Option<J>,
    closed: bool,
}

impl<J> Clone for Mailbox<J> {
    fn clone(&self) -> Self {
        Mailbox {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<J> Mailbox<J> {
    pub fn new() -> Mailbox<J> {
        let state = MailboxState {
            job: None,
            closed: false,
        };
        Mailbox {
            shared: Arc::new((Mutex::new(state), Condvar::new())),
        }
    }

    pub fn put_with(&self, job: J, merge: impl FnOnce(J, J) -> J) {
        let (lock, ready) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        let job = match state.job.take() {
            Some(waiting) => merge(job, waiting),
            None => job,
        };
        state.job = Some(job);
        ready.notify_one();
    }

    /// Waits for the next job. Returns `None` once the mailbox is closed.
    pub fn take(&self) -> Option<J> {
        let (lock, ready) = &*self.shared;
        let mut state = lock.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if state.closed {
                return None;
            }
            if let Some(job) = state.job.take() {
                return Some(job);
            }
            state = ready.wait(state).unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub fn close(&self) {
        let (lock, ready) = &*self.shared;
        lock.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
        ready.notify_all();
    }
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
    quit: Arc<AtomicBool>,
    resized: Arc<AtomicBool>,
    stop_input: Arc<AtomicBool>,
    input: Option<JoinHandle<()>>,
    _raw: RawMode,
}

impl Session {
    pub fn start(setup: &Setup, messages: Sender<Msg>) -> Result<Session> {
        let tty = setup.tty.clone().context("live output needs a terminal")?;
        let raw = tty.raw(true)?;
        let quit = Arc::new(AtomicBool::new(false));
        for signal in [SIGINT, SIGTERM, SIGHUP] {
            signal_hook::flag::register(signal, Arc::clone(&quit))?;
        }
        let resized = Arc::new(AtomicBool::new(false));
        signal_hook::flag::register(SIGWINCH, Arc::clone(&resized))?;
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
            quit,
            resized,
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

    /// A render job at the chosen scale, carrying the new configuration after a theme change.
    pub fn job(&mut self, source: String, grid: Grid, fit: Fit) -> Job {
        Job {
            source,
            grid,
            scale: self.setup.scale,
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

    /// Checks signals and timers. Call on every pass of the main loop.
    pub fn tick(&mut self) -> Outcome {
        if self.quit.load(Ordering::Relaxed) {
            return Outcome::Quit(130);
        }
        if self.resized.swap(false, Ordering::Relaxed)
            && let Ok(size) = self.tty.winsize()
        {
            return self.resize(size.ws_col, size.ws_row, size.ws_xpixel.into(), size.ws_ypixel.into());
        }
        if self.recolor_at.is_some_and(|at| Instant::now() >= at) {
            self.recolor_at = None;
            self.config = Some(self.setup.config_for(&self.palette));
            return Outcome::Redraw;
        }
        Outcome::Continue
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
    fn mailboxes_keep_the_newest_job() {
        let mailbox = Mailbox::new();
        mailbox.put_with(1, |new, _| new);
        mailbox.put_with(2, |new, waiting| new + waiting * 10);
        assert_eq!(mailbox.take(), Some(12));
        mailbox.close();
        assert_eq!(mailbox.take(), None);
    }
}
