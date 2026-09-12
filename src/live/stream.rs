//! `mer -` on a stream: drawn once when the input ends quickly, live while it keeps coming.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::process::ExitCode;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

use super::{Msg, Outcome, Picture, Session, TICK, dim};
use crate::display::{self, Setup};
use crate::engine::Failure;
use crate::input::markdown::{Fence, Scanner};
use crate::input::{self, Diagram, Format, sniff};
use crate::render::{self, Frame};
use crate::size::{Fit, Grid};
use crate::term::kitty;

/// A preview is rendered once the input pauses this long.
const QUIET: Duration = Duration::from_millis(80);
/// Without the end of input by then, output goes live.
const LIVE_AFTER: Duration = Duration::from_millis(400);

const STDIN: &str = "<stdin>";

pub fn run(setup: Setup, format: Option<Format>) -> Result<ExitCode> {
    let (messages, inbox) = mpsc::channel();
    spawn_reader(messages.clone());

    let started = Instant::now();
    let mut bytes = Vec::new();
    let mut last_data = started;
    loop {
        let deadline = if bytes.is_empty() {
            started + LIVE_AFTER
        } else {
            (last_data + QUIET).min(started + LIVE_AFTER)
        };
        match inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(Msg::Stdin(chunk)) => {
                bytes.extend_from_slice(&chunk);
                last_data = Instant::now();
            }
            Ok(Msg::StdinEnd) => {
                let text = String::from_utf8_lossy(&bytes);
                return display::show(&setup, &input::documents(&text, format));
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
        }
    }

    let session = Session::start(&setup, messages)?;
    let mut live = Live::new(setup, session, format);
    live.feed(&bytes);
    live.run(inbox)
}

fn spawn_reader(messages: Sender<Msg>) {
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = vec![0u8; 1 << 16];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if messages.send(Msg::Stdin(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        let _ = messages.send(Msg::StdinEnd);
    });
}

enum Busy {
    /// A finished diagram, printed for good once rendered.
    Final(Diagram),
    /// The Mermaid document still arriving, shown in the live region.
    Preview,
}

struct Live {
    setup: Setup,
    session: Session,
    /// The format given with `--stdin-format`, if any.
    fixed_format: Option<Format>,
    /// The format of the document being received, once known.
    format: Option<Format>,
    /// Bytes at the end of the input that don't form a whole UTF-8 character yet.
    partial: Vec<u8>,
    /// The document being received.
    text: String,
    /// How much of `text` the Markdown scanner has seen.
    scanned: usize,
    scanner: Scanner,
    /// Finished diagrams waiting to be printed, in order.
    queue: VecDeque<Diagram>,
    busy: Option<Busy>,
    dirty: bool,
    last_data: Instant,
    ended: bool,
    status: String,
    found: usize,
    failed: bool,
}

impl Live {
    fn new(setup: Setup, session: Session, format: Option<Format>) -> Live {
        Live {
            setup,
            session,
            fixed_format: format,
            format,
            partial: Vec::new(),
            text: String::new(),
            scanned: 0,
            scanner: Scanner::new(),
            queue: VecDeque::new(),
            busy: None,
            dirty: false,
            last_data: Instant::now(),
            ended: false,
            status: "waiting for input…".to_string(),
            found: 0,
            failed: false,
        }
    }

    fn run(mut self, inbox: Receiver<Msg>) -> Result<ExitCode> {
        self.draw_status()?;
        let quit = loop {
            self.pump();
            if self.ended && self.queue.is_empty() && self.busy.is_none() {
                break None;
            }
            let message = match inbox.recv_timeout(TICK) {
                Ok(message) => Some(message),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break None,
            };
            let mut outcome = self.session.tick();
            match message {
                Some(Msg::Stdin(bytes)) => self.feed(&bytes),
                Some(Msg::StdinEnd) => {
                    self.end_document();
                    self.ended = true;
                }
                Some(Msg::Input(event)) => outcome = outcome.or(self.session.handle(&event)),
                Some(Msg::Rendered(result)) => self.rendered(result)?,
                Some(Msg::Changed) | None => {}
            }
            match outcome {
                Outcome::Quit(code) => break Some(code),
                Outcome::Redraw => self.refresh()?,
                Outcome::Continue => {}
            }
        };
        self.finish(quit)
    }

    fn feed(&mut self, bytes: &[u8]) {
        for (index, part) in bytes.split(|&byte| byte == 0).enumerate() {
            if index > 0 {
                self.end_document();
            }
            self.push_bytes(part);
        }
    }

    fn push_bytes(&mut self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        self.partial.extend_from_slice(bytes);
        let whole = match std::str::from_utf8(&self.partial) {
            Err(err) if err.error_len().is_none() => err.valid_up_to(),
            _ => self.partial.len(),
        };
        let decoded = String::from_utf8_lossy(&self.partial[..whole]).into_owned();
        self.partial.drain(..whole);
        self.text.push_str(&decoded);
        self.dirty = true;
        self.last_data = Instant::now();
        self.scan();
    }

    /// Settles the format once a meaningful line has arrived, and scans complete Markdown lines.
    fn scan(&mut self) {
        let complete = self.text.rfind('\n').map_or(0, |i| i + 1);
        if self.format.is_none() {
            self.format = sniff::sniff(&self.text[..complete], false);
            if let Some(format) = self.format {
                self.status = match format {
                    Format::Mermaid => "receiving a diagram…",
                    Format::Markdown => "receiving Markdown…",
                }
                .to_string();
            }
        }
        if self.format != Some(Format::Markdown) {
            return;
        }
        while let Some(end) = self.text[self.scanned..complete].find('\n') {
            let line = &self.text[self.scanned..self.scanned + end];
            if let Some(fence) = self.scanner.push_line(line) {
                self.queue.push_back(fence_diagram(fence));
                self.found += 1;
            }
            self.scanned += end + 1;
        }
    }

    /// Queues what is left of the current document and starts a new one.
    fn end_document(&mut self) {
        if !self.partial.is_empty() {
            let rest = String::from_utf8_lossy(&self.partial).into_owned();
            self.text.push_str(&rest);
            self.partial.clear();
        }
        let text = std::mem::take(&mut self.text);
        match self.format.or_else(|| sniff::sniff(&text, true)) {
            Some(Format::Mermaid) if !text.trim().is_empty() => {
                self.queue.push_back(Diagram {
                    text,
                    origin: STDIN.to_string(),
                    first_line: 0,
                    caption: None,
                });
                self.found += 1;
            }
            Some(Format::Markdown) => {
                let mut scanner = std::mem::take(&mut self.scanner);
                for line in text[self.scanned..].split_inclusive('\n') {
                    if let Some(fence) = scanner.push_line(line.strip_suffix('\n').unwrap_or(line)) {
                        self.queue.push_back(fence_diagram(fence));
                        self.found += 1;
                    }
                }
                if let Some(fence) = scanner.finish() {
                    self.queue.push_back(fence_diagram(fence));
                    self.found += 1;
                }
            }
            _ => {}
        }
        self.scanner = Scanner::new();
        self.scanned = 0;
        self.format = self.fixed_format;
        self.dirty = false;
    }

    /// Starts the next render when the worker is free: finished diagrams first, in order,
    /// then a preview of the Mermaid document still arriving.
    fn pump(&mut self) {
        if self.busy.is_some() {
            return;
        }
        if let Some(diagram) = self.queue.pop_front() {
            self.session.submit(diagram.text.clone(), self.session.grid, self.setup.fit);
            self.busy = Some(Busy::Final(diagram));
            return;
        }
        if self.format != Some(Format::Mermaid) || !self.dirty || self.last_data.elapsed() < QUIET {
            return;
        }
        let complete = self.text.rfind('\n').map_or(0, |i| i + 1);
        self.dirty = false;
        if self.text[..complete].trim().is_empty() {
            return;
        }
        let grid = Grid {
            rows: self.session.grid.rows.saturating_sub(2).max(1),
            ..self.session.grid
        };
        self.session.submit(self.text[..complete].to_string(), grid, Fit::Contain);
        self.busy = Some(Busy::Preview);
    }

    fn rendered(&mut self, result: Result<Frame, Failure>) -> Result<()> {
        match (self.busy.take(), result) {
            (Some(Busy::Final(diagram)), Ok(frame)) => {
                let mut above = Vec::new();
                if let Some(caption) = &diagram.caption {
                    above.extend_from_slice(format!("{}\n", dim(caption)).as_bytes());
                }
                render::write_inline(&mut above, &frame, kitty::random_id())?;
                let status = self.status_line();
                self.session.draw(&above, Picture::None, &[], Some(&status))?;
            }
            (Some(Busy::Final(diagram)), Err(failure)) => {
                self.failed = true;
                // A preview that was showing stays, followed by the error.
                self.session.release(&[])?;
                display::report(&failure, &diagram, self.setup.color);
                self.draw_status()?;
            }
            (Some(Busy::Preview), Ok(frame)) => {
                self.status = format!("{STDIN} · {} ms · receiving…", frame.elapsed.as_millis());
                let status = self.status_line();
                self.session.draw(&[], Picture::New(&frame), &[], Some(&status))?;
            }
            (Some(Busy::Preview), Err(_)) => {
                self.status = "waiting for more input…".to_string();
                self.draw_status()?;
            }
            (None, _) => {}
        }
        Ok(())
    }

    /// Renders the preview again after a resize or a color change.
    fn refresh(&mut self) -> Result<()> {
        if self.session.region.has_image() {
            self.dirty = true;
            self.last_data = Instant::now() - QUIET;
        }
        self.draw_status()
    }

    fn status_line(&self) -> String {
        dim(&format!("{} · q to stop", self.status))
    }

    fn draw_status(&mut self) -> Result<()> {
        let status = self.status_line();
        self.session.draw(&[], Picture::Keep, &[], Some(&status))?;
        Ok(())
    }

    fn finish(mut self, quit: Option<u8>) -> Result<ExitCode> {
        self.session.release(&[])?;
        drop(self.session);
        if let Some(code) = quit {
            return Ok(ExitCode::from(code));
        }
        if self.found == 0 {
            eprintln!("mer: no Mermaid diagram found");
            return Ok(ExitCode::from(1));
        }
        Ok(if self.failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
    }
}

fn fence_diagram(fence: Fence) -> Diagram {
    Diagram {
        text: fence.text,
        origin: STDIN.to_string(),
        first_line: fence.first_line,
        caption: fence.caption,
    }
}
