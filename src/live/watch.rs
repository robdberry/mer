//! `mer -w FILE…`: the diagram is drawn again whenever a file changes.

use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

use anyhow::Result;

use super::{Msg, Outcome, Picture, Session, TICK, Worker, dim, spawn_poller};
use crate::diag::{self, Diagnostic};
use crate::display::Setup;
use crate::engine::Failure;
use crate::input::{self, Diagram};
use crate::render::Frame;
use crate::size::{Fit, Grid};
use crate::term::input::{Event, Key};

pub fn run(setup: Setup, paths: Vec<PathBuf>, selected: Option<usize>) -> Result<ExitCode> {
    let (messages, inbox) = mpsc::channel();
    let stop = Arc::new(AtomicBool::new(false));
    spawn_poller(paths.clone(), messages.clone(), Arc::clone(&stop));
    let worker = Worker::spawn(setup.config.clone(), setup.background, messages.clone());
    let session = Session::start(&setup, messages)?;
    let mut watch = Watch {
        session,
        worker,
        paths,
        diagrams: Vec::new(),
        index: selected.map_or(0, |n| n.saturating_sub(1)),
        busy: false,
        stale: true,
        notes: Vec::new(),
        status: String::new(),
        problem: None,
    };
    watch.reload();
    let result = watch.run(inbox);
    stop.store(true, Ordering::Relaxed);
    result
}

struct Watch {
    session: Session,
    worker: Worker,
    paths: Vec<PathBuf>,
    diagrams: Vec<Diagram>,
    index: usize,
    busy: bool,
    /// The shown diagram no longer matches the input, grid or colors.
    stale: bool,
    /// Lines under the diagram: the latest error.
    notes: Vec<String>,
    status: String,
    /// A file that could not be read.
    problem: Option<String>,
}

impl Watch {
    fn run(mut self, inbox: Receiver<Msg>) -> Result<ExitCode> {
        let code = loop {
            self.pump()?;
            let message = match inbox.recv_timeout(TICK) {
                Ok(message) => Some(message),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break 0,
            };
            let mut outcome = self.session.tick();
            match message {
                Some(Msg::Changed) => self.reload(),
                Some(Msg::Input(event)) => {
                    outcome = outcome.or(self.session.handle(&event));
                    self.key(&event);
                }
                Some(Msg::Rendered(result)) => self.rendered(result)?,
                Some(Msg::Stdin(_) | Msg::StdinEnd | Msg::Viewed(_)) | None => {}
            }
            match outcome {
                Outcome::Quit(code) => break code,
                Outcome::Redraw => self.stale = true,
                Outcome::Continue => {}
            }
        };
        let notes = self.notes.clone();
        self.session.release(&notes)?;
        Ok(ExitCode::from(code))
    }

    fn reload(&mut self) {
        let mut diagrams = Vec::new();
        for path in &self.paths {
            match fs::read_to_string(path) {
                Ok(text) => {
                    let origin = path.display().to_string();
                    let format = input::format_from_extension(path);
                    diagrams.extend(input::diagrams(&text, &origin, format));
                }
                Err(err) => {
                    // Often a save in progress; the next change reloads.
                    self.problem = Some(format!("cannot read {}: {err}", path.display()));
                    self.stale = true;
                    return;
                }
            }
        }
        self.problem = None;
        self.diagrams = diagrams;
        self.index = self.index.min(self.diagrams.len().saturating_sub(1));
        self.stale = true;
    }

    fn key(&mut self, event: &Event) {
        let count = self.diagrams.len();
        let Event::Key(key) = event else {
            return;
        };
        match key {
            Key::Char('r') => self.reload(),
            Key::Char('n' | 'j') | Key::Right | Key::Down | Key::PageDown if count > 1 => {
                self.index = (self.index + 1) % count;
                self.stale = true;
            }
            Key::Char('p' | 'k') | Key::Left | Key::Up | Key::PageUp if count > 1 => {
                self.index = (self.index + count - 1) % count;
                self.stale = true;
            }
            _ => {}
        }
    }

    fn pump(&mut self) -> Result<()> {
        if self.busy || !self.stale {
            return Ok(());
        }
        self.stale = false;
        let Some(diagram) = self.diagrams.get(self.index) else {
            self.notes = vec![
                self.problem
                    .clone()
                    .unwrap_or_else(|| "no Mermaid diagram found".to_string()),
            ];
            self.status = self.paths[0].display().to_string();
            return self.draw(Picture::None);
        };
        let reserved = 1 + self.notes.len() as u16;
        let grid = Grid {
            rows: self.session.grid.rows.saturating_sub(reserved + 1).max(1),
            ..self.session.grid
        };
        let job = self.session.job(diagram.text.clone(), grid, Fit::Contain);
        self.worker.submit(job);
        self.busy = true;
        Ok(())
    }

    fn rendered(&mut self, result: Result<Frame, Failure>) -> Result<()> {
        self.busy = false;
        let Some(diagram) = self.diagrams.get(self.index) else {
            return Ok(());
        };
        let position = if self.diagrams.len() > 1 {
            format!("{} [{}/{}]", diagram.origin, self.index + 1, self.diagrams.len())
        } else {
            diagram.origin.clone()
        };
        match result {
            Ok(frame) => {
                self.notes = self.problem.iter().cloned().collect();
                self.status = format!("{position} · {} ms", frame.elapsed.as_millis());
                self.draw(Picture::New(&frame))
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
                // Room for the error below whatever is still shown.
                let shown = self.session.region.lines_for_image();
                let room = usize::from(self.session.grid.rows.saturating_sub(shown + 2)).max(1);
                self.notes = diag::format(&diagnostic, diagram, true)
                    .lines()
                    .take(room)
                    .map(str::to_string)
                    .collect();
                self.status = format!("{position} · error");
                self.draw(Picture::Keep)
            }
        }
    }

    fn draw(&mut self, picture: Picture<'_>) -> Result<()> {
        let status = dim(&format!("{} · watching · q to quit", self.status));
        let notes = self.notes.clone();
        self.session.draw(&[], picture, &notes, Some(&status))?;
        Ok(())
    }
}
