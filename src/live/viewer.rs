//! `mer -i`: a full-screen viewer for panning and zooming around diagrams.
//!
//! Each diagram is laid out once; every change of view rasterizes only the visible viewport at
//! the current zoom, so the picture stays sharp at any size.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Result;
use resvg::{tiny_skia, usvg};
use serde_json::Value;

use super::{Mailbox, Msg, Outcome, Session, TICK, dim, fit_line, spawn_poller, write_stdout};
use crate::diag::{self, Diagnostic};
use crate::display::Setup;
use crate::engine::{Control, Engine, Failure};
use crate::input::{self, Diagram};
use crate::raster::{self, Rasterizer};
use crate::size::{self, Grid};
use crate::term::input::{Event, Key, Mouse, MouseKind};
use crate::term::{self, kitty};
use crate::theme::Rgb;

/// Zoom factor for one key press.
const ZOOM_STEP: f32 = 1.25;
/// Zoom factor for one scroll wheel step.
const SCROLL_STEP: f32 = 1.15;

/// What the viewer shows: device pixels per SVG unit, and the SVG point at the viewport center.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub scale: f32,
    pub center: (f32, f32),
}

/// A finished render sent back by the worker.
pub struct Viewed {
    index: usize,
    result: Result<Shot, Failure>,
}

struct Shot {
    rgba: Vec<u8>,
    pixels: (u32, u32),
    cells: (u16, u16),
    /// The diagram's size in SVG units.
    size: (f32, f32),
    view: View,
    elapsed: Duration,
}

struct Job {
    index: usize,
    source: String,
    /// `None` fits the whole diagram.
    view: Option<View>,
    grid: Grid,
    config: Option<Value>,
    /// The inputs were read again, so cached layouts are stale.
    reload: bool,
}

/// Shows `diagrams`. `paths` are read again on `r`, and whenever they change with `watch`.
pub fn run(
    setup: Setup,
    diagrams: Vec<Diagram>,
    paths: Vec<PathBuf>,
    watch: bool,
) -> Result<ExitCode> {
    let (messages, inbox) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    if watch {
        spawn_poller(paths.clone(), messages.clone(), Arc::clone(&stop));
    }
    let session = Session::start(&setup, messages.clone())?;
    let jobs = Mailbox::new();
    let worker = spawn_worker(setup.config.clone(), setup.background, jobs.clone(), messages);
    write_stdout(b"\x1b[?1049h\x1b[?1002h\x1b[?1006h\x1b[?1016h")?;

    let mut viewer = Viewer {
        session,
        jobs: jobs.clone(),
        paths,
        diagrams,
        index: 0,
        view: None,
        size: None,
        busy: false,
        stale: true,
        reload: false,
        drag: None,
        image: None,
        elapsed: Duration::ZERO,
        notes: Vec::new(),
    };
    let result = viewer.run(inbox);

    let mut out = Vec::new();
    if let Some(id) = viewer.image.take() {
        let _ = kitty::delete(&mut out, id);
    }
    out.extend_from_slice(b"\x1b]22;default\x1b\\\x1b[?1016l\x1b[?1006l\x1b[?1002l\x1b[?1049l");
    let _ = write_stdout(&out);
    jobs.close();
    let _ = worker.join();
    stop.store(true, Ordering::Relaxed);
    result
}

fn spawn_worker(
    config: Value,
    background: Option<Rgb>,
    jobs: Mailbox<Job>,
    messages: Sender<Msg>,
) -> JoinHandle<()> {
    thread::spawn(move || {
        let mut engine = Engine::new(config, None);
        let mut rasterizer = Rasterizer::new();
        let mut layouts: HashMap<usize, Result<usvg::Tree, Failure>> = HashMap::new();
        while let Some(job) = jobs.take() {
            let started = Instant::now();
            if let Some(config) = &job.config {
                engine = Engine::new(config.clone(), None);
                layouts.clear();
            }
            if job.reload {
                layouts.clear();
            }
            let layout = layouts.entry(job.index).or_insert_with(|| {
                let svg = engine.render_svg(&job.source, Control::new())?;
                rasterizer.parse(&svg).map_err(|err| {
                    Failure::Diagnostic(Diagnostic {
                        message: format!("{err:#}"),
                        span: None,
                    })
                })
            });
            let result = match layout {
                Ok(tree) => Ok(shoot(tree, &job, background, started)),
                Err(failure) => Err(failure.clone()),
            };
            let viewed = Viewed {
                index: job.index,
                result,
            };
            if messages.send(Msg::Viewed(Box::new(viewed))).is_err() {
                return;
            }
        }
    })
}

fn shoot(tree: &usvg::Tree, job: &Job, background: Option<Rgb>, started: Instant) -> Shot {
    let size = (tree.size().width(), tree.size().height());
    let pixels = (
        u32::from(job.grid.cols) * job.grid.cell_w,
        u32::from(job.grid.rows) * job.grid.cell_h,
    );
    let view = job
        .view
        .unwrap_or_else(|| fit_all(size, pixels, job.grid.cell_h));
    let transform = tiny_skia::Transform::from_row(
        view.scale,
        0.0,
        0.0,
        view.scale,
        pixels.0 as f32 / 2.0 - view.center.0 * view.scale,
        pixels.1 as f32 / 2.0 - view.center.1 * view.scale,
    );
    let pixmap = raster::render_transform(tree, transform, pixels, background);
    Shot {
        rgba: raster::straight_rgba(pixmap),
        pixels,
        cells: (job.grid.cols, job.grid.rows),
        size,
        view,
        elapsed: started.elapsed(),
    }
}

/// The whole diagram in view, but never larger than the terminal's own text size.
fn fit_all(size: (f32, f32), pixels: (u32, u32), cell_h: u32) -> View {
    let scale = (pixels.0 as f32 / size.0.max(1.0)).min(pixels.1 as f32 / size.1.max(1.0)) * 0.95;
    View {
        scale: scale.min(size::text_matched_scale(cell_h)),
        center: (size.0 / 2.0, size.1 / 2.0),
    }
}

/// Zooms by `factor` while keeping the diagram point under `point` in place.
fn zoom_at(view: &mut View, factor: f32, point: (f32, f32), viewport: (f32, f32), limits: (f32, f32)) {
    let offset = (point.0 - viewport.0 / 2.0, point.1 - viewport.1 / 2.0);
    let target = (
        view.center.0 + offset.0 / view.scale,
        view.center.1 + offset.1 / view.scale,
    );
    view.scale = (view.scale * factor).clamp(limits.0, limits.1);
    view.center = (
        target.0 - offset.0 / view.scale,
        target.1 - offset.1 / view.scale,
    );
}

/// Keeps some of the diagram in view.
fn clamp(view: &mut View, size: (f32, f32)) {
    view.center.0 = view.center.0.clamp(0.0, size.0);
    view.center.1 = view.center.1.clamp(0.0, size.1);
}

struct Drag {
    from: (f32, f32),
    center: (f32, f32),
}

struct Viewer {
    session: Session,
    jobs: Mailbox<Job>,
    paths: Vec<PathBuf>,
    diagrams: Vec<Diagram>,
    index: usize,
    view: Option<View>,
    size: Option<(f32, f32)>,
    busy: bool,
    /// The screen no longer matches the view, grid or colors.
    stale: bool,
    reload: bool,
    drag: Option<Drag>,
    image: Option<u32>,
    elapsed: Duration,
    /// Error lines shown instead of a diagram.
    notes: Vec<String>,
}

impl Viewer {
    fn run(&mut self, inbox: Receiver<Msg>) -> Result<ExitCode> {
        loop {
            self.pump()?;
            let message = match inbox.recv_timeout(TICK) {
                Ok(message) => Some(message),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return Ok(ExitCode::SUCCESS),
            };
            let mut outcome = self.session.tick();
            match message {
                Some(Msg::Input(event)) => {
                    outcome = outcome.or(self.session.handle(&event));
                    self.input(event);
                }
                Some(Msg::Viewed(viewed)) => self.viewed(*viewed)?,
                Some(Msg::Changed) => self.reload(),
                _ => {}
            }
            match outcome {
                Outcome::Quit(code) => return Ok(ExitCode::from(code)),
                Outcome::Redraw => self.stale = true,
                Outcome::Continue => {}
            }
        }
    }

    /// The image area: the whole terminal but the status line.
    fn viewport(&self) -> Grid {
        Grid {
            rows: self.session.grid.rows.saturating_sub(1).max(1),
            ..self.session.grid
        }
    }

    fn viewport_pixels(&self) -> (f32, f32) {
        let grid = self.viewport();
        (
            (u32::from(grid.cols) * grid.cell_w) as f32,
            (u32::from(grid.rows) * grid.cell_h) as f32,
        )
    }

    fn actual_size(&self) -> f32 {
        size::text_matched_scale(self.session.grid.cell_h)
    }

    fn zoom_limits(&self) -> (f32, f32) {
        (self.actual_size() / 20.0, self.actual_size() * 20.0)
    }

    fn pump(&mut self) -> io::Result<()> {
        if self.busy || !self.stale {
            return Ok(());
        }
        self.stale = false;
        let Some(diagram) = self.diagrams.get(self.index) else {
            self.notes = vec!["no Mermaid diagram found".to_string()];
            return self.draw_notes();
        };
        let job = Job {
            index: self.index,
            source: diagram.text.clone(),
            view: self.view,
            grid: self.viewport(),
            config: self.session.take_config(),
            reload: std::mem::take(&mut self.reload),
        };
        self.jobs.put_with(job, |new, old| Job {
            config: new.config.or(old.config),
            reload: new.reload || old.reload,
            ..new
        });
        self.busy = true;
        Ok(())
    }

    fn viewed(&mut self, viewed: Viewed) -> io::Result<()> {
        self.busy = false;
        if viewed.index != self.index {
            self.stale = true;
            return Ok(());
        }
        match viewed.result {
            Ok(shot) => {
                self.view.get_or_insert(shot.view);
                self.size = Some(shot.size);
                self.elapsed = shot.elapsed;
                self.notes.clear();
                if shot.cells != (self.viewport().cols, self.viewport().rows) {
                    self.stale = true;
                }
                self.draw(&shot)
            }
            Err(Failure::Cancelled) => Ok(()),
            Err(failure) => {
                let diagnostic = match failure {
                    Failure::Diagnostic(diagnostic) => diagnostic,
                    _ => Diagnostic {
                        message: "no Mermaid diagram found".to_string(),
                        span: None,
                    },
                };
                self.notes = diag::format(&diagnostic, &self.diagrams[self.index], true)
                    .lines()
                    .map(str::to_string)
                    .collect();
                self.draw_notes()
            }
        }
    }

    fn input(&mut self, event: Event) {
        match event {
            Event::Key(key) => self.key(key),
            Event::Mouse(mouse) => self.mouse(mouse),
            _ => {}
        }
    }

    fn key(&mut self, key: Key) {
        let count = self.diagrams.len();
        match key {
            Key::Char('n') | Key::PageDown | Key::Tab if count > 1 => {
                return self.select((self.index + 1) % count);
            }
            Key::Char('p') | Key::PageUp if count > 1 => {
                return self.select((self.index + count - 1) % count);
            }
            Key::Home if count > 1 => return self.select(0),
            Key::End if count > 1 => return self.select(count - 1),
            Key::Char('r') => return self.reload(),
            Key::Char('0') => {
                // The worker fits the diagram again.
                self.view = None;
                self.stale = true;
                return;
            }
            _ => {}
        }

        let viewport = self.viewport_pixels();
        let limits = self.zoom_limits();
        let actual = self.actual_size();
        let (Some(view), Some(size)) = (self.view.as_mut(), self.size) else {
            return;
        };
        let pan = |fraction: f32| viewport.0.min(viewport.1) * fraction / view.scale;
        let (small, large) = (pan(0.1), pan(0.5));
        let middle = (viewport.0 / 2.0, viewport.1 / 2.0);
        match key {
            Key::Char('h') | Key::Left => view.center.0 -= small,
            Key::Char('l') | Key::Right => view.center.0 += small,
            Key::Char('k') | Key::Up => view.center.1 -= small,
            Key::Char('j') | Key::Down => view.center.1 += small,
            Key::Char('H') => view.center.0 -= large,
            Key::Char('L') => view.center.0 += large,
            Key::Char('K') => view.center.1 -= large,
            Key::Char('J') => view.center.1 += large,
            Key::Char('+' | '=') => zoom_at(view, ZOOM_STEP, middle, viewport, limits),
            Key::Char('-' | '_') => zoom_at(view, 1.0 / ZOOM_STEP, middle, viewport, limits),
            Key::Char('1') => view.scale = actual,
            Key::Char('w') => {
                view.scale = (viewport.0 / size.0.max(1.0) * 0.98).clamp(limits.0, limits.1);
                view.center.0 = size.0 / 2.0;
            }
            _ => return,
        }
        clamp(view, size);
        self.stale = true;
    }

    /// The pointer in viewport pixels. Inside tmux, which reports cells, the cell's center
    /// stands in for it.
    fn pointer(&self, mouse: Mouse) -> (f32, f32) {
        if term::inside_tmux() {
            let grid = self.session.grid;
            (
                (mouse.x as f32 + 0.5) * grid.cell_w as f32,
                (mouse.y as f32 + 0.5) * grid.cell_h as f32,
            )
        } else {
            (mouse.x as f32, mouse.y as f32)
        }
    }

    fn mouse(&mut self, mouse: Mouse) {
        let viewport = self.viewport_pixels();
        let limits = self.zoom_limits();
        let point = self.pointer(mouse);
        let (Some(view), Some(size)) = (self.view.as_mut(), self.size) else {
            return;
        };
        match mouse.kind {
            MouseKind::Press(0) => {
                self.drag = Some(Drag {
                    from: point,
                    center: view.center,
                });
                let _ = write_stdout(b"\x1b]22;grabbing\x1b\\");
                return;
            }
            MouseKind::Drag => {
                let Some(drag) = &self.drag else {
                    return;
                };
                view.center = (
                    drag.center.0 - (point.0 - drag.from.0) / view.scale,
                    drag.center.1 - (point.1 - drag.from.1) / view.scale,
                );
            }
            MouseKind::Release => {
                if self.drag.take().is_some() {
                    let _ = write_stdout(b"\x1b]22;default\x1b\\");
                }
                return;
            }
            MouseKind::ScrollUp => zoom_at(view, SCROLL_STEP, point, viewport, limits),
            MouseKind::ScrollDown => zoom_at(view, 1.0 / SCROLL_STEP, point, viewport, limits),
            _ => return,
        }
        clamp(view, size);
        self.stale = true;
    }

    fn select(&mut self, index: usize) {
        self.index = index;
        self.view = None;
        self.size = None;
        self.stale = true;
    }

    fn reload(&mut self) {
        let mut diagrams = Vec::new();
        for path in &self.paths {
            let Ok(text) = fs::read_to_string(path) else {
                // Often a save in progress; the next change reloads.
                return;
            };
            let format = input::format_from_extension(path);
            diagrams.extend(input::diagrams(&text, &path.display().to_string(), format));
        }
        if self.paths.is_empty() {
            return;
        }
        self.diagrams = diagrams;
        self.index = self.index.min(self.diagrams.len().saturating_sub(1));
        self.reload = true;
        self.stale = true;
    }

    fn status(&self) -> String {
        let Some(diagram) = self.diagrams.get(self.index) else {
            return dim("q to quit");
        };
        let mut parts = vec![match &diagram.caption {
            Some(caption) => format!("{} › {caption}", diagram.origin),
            None => diagram.origin.clone(),
        }];
        if self.diagrams.len() > 1 {
            parts.push(format!("{}/{}", self.index + 1, self.diagrams.len()));
        }
        if let Some(view) = self.view {
            parts.push(format!("{:.0}%", view.scale / self.actual_size() * 100.0));
        }
        parts.push(format!("{} ms", self.elapsed.as_millis()));
        parts.push("arrows pan · +/- zoom · 0 fit · 1 actual size".to_string());
        if self.diagrams.len() > 1 {
            parts.push("n/p diagram".to_string());
        }
        parts.push("q quit".to_string());
        dim(&parts.join(" · "))
    }

    fn write_status(&self, out: &mut Vec<u8>) {
        let grid = self.session.grid;
        let _ = write!(out, "\x1b[{};1H\x1b[2K{}", grid.rows, fit_line(&self.status(), grid.cols));
    }

    fn draw(&mut self, shot: &Shot) -> io::Result<()> {
        let mut out = Vec::with_capacity(shot.rgba.len() / 8);
        out.extend_from_slice(b"\x1b[?2026h\x1b[H\x1b[2J");
        let id = kitty::random_id();
        kitty::transmit_virtual(&mut out, id, &shot.rgba, shot.pixels, shot.cells)?;
        let mut cells = String::new();
        for row in 0..shot.cells.1 {
            kitty::placeholder_row(&mut cells, id, row, shot.cells.0);
            cells.push_str("\r\n");
        }
        out.extend_from_slice(cells.as_bytes());
        self.write_status(&mut out);
        if let Some(old) = self.image.replace(id) {
            kitty::delete(&mut out, old)?;
        }
        out.extend_from_slice(b"\x1b[?2026l");
        write_stdout(&out)
    }

    fn draw_notes(&mut self) -> io::Result<()> {
        let mut out = b"\x1b[?2026h\x1b[H\x1b[2J".to_vec();
        let grid = self.session.grid;
        for note in self.notes.iter().take(usize::from(grid.rows.saturating_sub(1))) {
            out.extend_from_slice(fit_line(note, grid.cols).as_bytes());
            out.extend_from_slice(b"\r\n");
        }
        self.write_status(&mut out);
        if let Some(old) = self.image.take() {
            kitty::delete(&mut out, old)?;
        }
        out.extend_from_slice(b"\x1b[?2026l");
        write_stdout(&out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fitting_never_enlarges_past_the_text_size() {
        let small = fit_all((100.0, 50.0), (2000, 1000), 34);
        assert_eq!(small.scale, size::text_matched_scale(34));
        assert_eq!(small.center, (50.0, 25.0));
        let large = fit_all((4000.0, 1000.0), (2000, 1000), 34);
        assert!((large.scale - 0.475).abs() < 1e-4);
    }

    #[test]
    fn zooming_keeps_the_pointed_spot_in_place() {
        let mut view = View {
            scale: 1.0,
            center: (500.0, 500.0),
        };
        let viewport = (1000.0, 800.0);
        let point = (900.0, 100.0);
        let under = |v: &View| {
            (
                v.center.0 + (point.0 - viewport.0 / 2.0) / v.scale,
                v.center.1 + (point.1 - viewport.1 / 2.0) / v.scale,
            )
        };
        let before = under(&view);
        zoom_at(&mut view, 2.0, point, viewport, (0.1, 10.0));
        assert_eq!(view.scale, 2.0);
        let after = under(&view);
        assert!((before.0 - after.0).abs() < 1e-3 && (before.1 - after.1).abs() < 1e-3);
    }

    #[test]
    fn zoom_and_pan_stay_within_limits() {
        let mut view = View {
            scale: 1.0,
            center: (0.0, 0.0),
        };
        zoom_at(&mut view, 1000.0, (0.0, 0.0), (100.0, 100.0), (0.5, 4.0));
        assert_eq!(view.scale, 4.0);
        view.center = (-50.0, 900.0);
        clamp(&mut view, (300.0, 200.0));
        assert_eq!(view.center, (0.0, 200.0));
    }
}
