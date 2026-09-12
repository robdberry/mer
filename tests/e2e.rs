//! End-to-end tests: `mer` runs on a pseudo-terminal that answers its queries the way Ghostty
//! does, and the tests decode what it writes back into images and placeholder grids.
//!
//! Set `MER_E2E_DUMP=1` to write decoded images to `target/tmp/e2e/`, composited over the fake
//! terminal's background, for inspection.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::read::ZlibDecoder;
use resvg::tiny_skia::{FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};

const MER: &str = env!("CARGO_BIN_EXE_mer");
const COLS: u16 = 120;
const ROWS: u16 = 40;
const CELL: (u32, u32) = (17, 34);
const BACKGROUND: [u8; 3] = [0x1e, 0x1e, 0x2e];
const LIGHT_BACKGROUND: [u8; 3] = [0xef, 0xf1, 0xf5];
const PLACEHOLDER: char = '\u{10EEEE}';
const TRANSMIT: &[u8] = b"a=T,U=1";

/// How a fake terminal answers queries.
#[derive(Clone, Copy)]
struct Profile {
    kitty: bool,
    /// Whether it reports sizes, colors and its color scheme.
    reports: bool,
    dark: bool,
    fg: &'static str,
    bg: &'static str,
    /// ANSI colors 1 to 6.
    ansi: [&'static str; 6],
}

const GHOSTTY: Profile = Profile {
    kitty: true,
    reports: true,
    dark: true,
    fg: "cdcd/d6d6/f4f4",
    bg: "1e1e/1e1e/2e2e",
    ansi: [
        "f3f3/8b8b/a8a8",
        "a6a6/e3e3/a1a1",
        "f9f9/e2e2/afaf",
        "8989/b4b4/fafa",
        "cbcb/a6a6/f7f7",
        "9494/e2e2/d5d5",
    ],
};

const LIGHT_TERMINAL: Profile = Profile {
    dark: false,
    fg: "4c4c/4f4f/6969",
    bg: "efef/f1f1/f5f5",
    ansi: [
        "d2d2/0f0f/3939",
        "4040/a0a0/2b2b",
        "dfdf/8e8e/1d1d",
        "1e1e/6666/f5f5",
        "8888/3939/efef",
        "1717/9292/9999",
    ],
    ..GHOSTTY
};

/// A terminal without graphics that answers device attribute queries and nothing else.
const PLAIN_TERMINAL: Profile = Profile {
    kitty: false,
    reports: false,
    ..GHOSTTY
};

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum ParseState {
    #[default]
    Ground,
    Escape,
    Csi,
    Osc,
    OscEscape,
    Apc,
    ApcEscape,
    Dcs,
    DcsEscape,
}

/// Follows a program's output and collects answers to its queries, in order.
#[derive(Default)]
struct Responder {
    state: ParseState,
    body: Vec<u8>,
}

impl Responder {
    fn feed(&mut self, bytes: &[u8], profile: &Profile, replies: &mut Vec<u8>) {
        use ParseState::*;
        for &byte in bytes {
            self.state = match (self.state, byte) {
                (Ground, 0x1b) => Escape,
                (Ground, _) => Ground,
                (Escape, b'[' | b']' | b'_' | b'P') => {
                    self.body.clear();
                    match byte {
                        b'[' => Csi,
                        b']' => Osc,
                        b'_' => Apc,
                        _ => Dcs,
                    }
                }
                (Escape, 0x1b) => Escape,
                (Escape, _) => Ground,
                (Csi, 0x40..=0x7e) => {
                    self.body.push(byte);
                    answer_csi(&self.body, profile, replies);
                    Ground
                }
                (Csi, _) => {
                    self.body.push(byte);
                    Csi
                }
                (Osc, 0x07) => {
                    answer_osc(&self.body, profile, replies);
                    Ground
                }
                (Osc, 0x1b) => OscEscape,
                (Osc, _) => {
                    self.body.push(byte);
                    Osc
                }
                (OscEscape, b'\\') => {
                    answer_osc(&self.body, profile, replies);
                    Ground
                }
                (OscEscape, _) => Ground,
                (Apc, 0x1b) => ApcEscape,
                (Apc, _) => {
                    // The control keys come first; payloads can be large.
                    if self.body.len() < 256 {
                        self.body.push(byte);
                    }
                    Apc
                }
                (ApcEscape, b'\\') => {
                    answer_apc(&self.body, profile, replies);
                    Ground
                }
                (ApcEscape, _) => Ground,
                (Dcs, 0x1b) => DcsEscape,
                (Dcs, _) => Dcs,
                (DcsEscape, b'\\') => Ground,
                (DcsEscape, _) => Dcs,
            };
        }
    }
}

fn answer_csi(body: &[u8], profile: &Profile, replies: &mut Vec<u8>) {
    let reply = match body {
        b"c" | b"0c" => b"\x1b[?62;22c".to_vec(),
        b">c" | b">0c" => b"\x1b[>1;10;0c".to_vec(),
        b">q" | b">0q" => b"\x1bP>|ghostty 1.3.1\x1b\\".to_vec(),
        b"16t" if profile.reports => format!("\x1b[6;{};{}t", CELL.1, CELL.0).into_bytes(),
        b"14t" if profile.reports => {
            let (w, h) = (u32::from(COLS) * CELL.0, u32::from(ROWS) * CELL.1);
            format!("\x1b[4;{h};{w}t").into_bytes()
        }
        b"?996n" if profile.reports => {
            format!("\x1b[?997;{}n", if profile.dark { 1 } else { 2 }).into_bytes()
        }
        _ => return,
    };
    replies.extend_from_slice(&reply);
}

fn answer_osc(body: &[u8], profile: &Profile, replies: &mut Vec<u8>) {
    if !profile.reports {
        return;
    }
    let body = String::from_utf8_lossy(body);
    let color = match body.as_ref() {
        "10;?" => format!("10;rgb:{}", profile.fg),
        "11;?" => format!("11;rgb:{}", profile.bg),
        other => match other.strip_prefix("4;").and_then(|rest| rest.strip_suffix(";?")) {
            Some(index @ ("1" | "2" | "3" | "4" | "5" | "6")) => {
                let n: usize = index.parse().unwrap();
                format!("4;{index};rgb:{}", profile.ansi[n - 1])
            }
            _ => return,
        },
    };
    replies.extend_from_slice(format!("\x1b]{color}\x1b\\").as_bytes());
}

fn answer_apc(body: &[u8], profile: &Profile, replies: &mut Vec<u8>) {
    let body = String::from_utf8_lossy(body);
    let Some(control) = body.strip_prefix('G') else {
        return;
    };
    let keys = control.split(';').next().unwrap_or("");
    if !profile.kitty || !keys.split(',').any(|kv| kv == "a=q") {
        return;
    }
    let id = keys.split(',').find(|kv| kv.starts_with("i=")).unwrap_or("i=0");
    replies.extend_from_slice(format!("\x1b_G{id};OK\x1b\\").as_bytes());
}

struct Run {
    status: i32,
    output: Vec<u8>,
    stderr: String,
}

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

fn scratch_file(name: &str, contents: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("scratch");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    fs::write(&path, contents).unwrap();
    path
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|window| window == needle)
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack.windows(needle.len()).filter(|window| *window == needle).count()
}

/// `openpty` is not thread-safe on macOS, and a child spawned by another test while fresh pty
/// descriptors are still inheritable would hold them open. Terminals and children are created
/// one at a time under this lock.
static SPAWN: Mutex<()> = Mutex::new(());

/// Opens a pseudo-terminal pair. Call with `SPAWN` held.
fn open_pty() -> (OwnedFd, OwnedFd) {
    let (mut master, mut slave) = (0, 0);
    let mut size = libc::winsize {
        ws_row: ROWS,
        ws_col: COLS,
        ws_xpixel: COLS * CELL.0 as u16,
        ws_ypixel: ROWS * CELL.1 as u16,
    };
    let status = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    };
    assert_eq!(status, 0, "openpty: {}", std::io::Error::last_os_error());
    for fd in [master, slave] {
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    }
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

enum Stdin {
    /// stdin is the terminal itself.
    Terminal,
    /// stdin is a pipe the test writes to.
    Pipe,
}

/// A program running with a pseudo-terminal as its controlling terminal and stdout, which
/// answers its queries according to a profile.
struct Terminal {
    child: Child,
    keyboard: File,
    stdin: Option<ChildStdin>,
    output: Arc<Mutex<Vec<u8>>>,
    screen: Option<JoinHandle<()>>,
    stderr: Option<JoinHandle<String>>,
}

impl Terminal {
    fn spawn(args: &[&str], stdin: Stdin, profile: Profile) -> Terminal {
        Terminal::spawn_program(MER, args, stdin, profile, &[])
    }

    fn spawn_program(
        program: &str,
        args: &[&str],
        stdin: Stdin,
        profile: Profile,
        env: &[(&str, &str)],
    ) -> Terminal {
        let spawning = SPAWN.lock().unwrap_or_else(PoisonError::into_inner);
        let (master, slave) = open_pty();
        let mut command = Command::new(program);
        command
            .args(args)
            .env("MER_PROBE_TIMEOUT_MS", "300")
            .env("MER_CONFIG", "/dev/null")
            .envs(env.iter().copied())
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::piped());
        match stdin {
            Stdin::Pipe => command.stdin(Stdio::piped()),
            Stdin::Terminal => command.stdin(Stdio::from(slave.try_clone().unwrap())),
        };
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 || libc::ioctl(1, libc::TIOCSCTTY as _, 0) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().expect("spawn the program");
        drop(command);
        drop(slave);
        drop(spawning);

        let mut master = File::from(master);
        let mut answer = master.try_clone().unwrap();
        let keyboard = master.try_clone().unwrap();
        let output = Arc::new(Mutex::new(Vec::new()));
        let screen = thread::spawn({
            let output = Arc::clone(&output);
            move || {
                let mut responder = Responder::default();
                let mut replies = Vec::new();
                let mut buf = [0u8; 1 << 16];
                while let Ok(n @ 1..) = master.read(&mut buf) {
                    responder.feed(&buf[..n], &profile, &mut replies);
                    output.lock().unwrap().extend_from_slice(&buf[..n]);
                    if !replies.is_empty() {
                        let _ = answer.write_all(&replies);
                        replies.clear();
                    }
                }
            }
        });
        let mut stderr_pipe = child.stderr.take().unwrap();
        let stderr = thread::spawn(move || {
            let mut text = String::new();
            let _ = stderr_pipe.read_to_string(&mut text);
            text
        });
        Terminal {
            stdin: child.stdin.take(),
            child,
            keyboard,
            output,
            screen: Some(screen),
            stderr: Some(stderr),
        }
    }

    /// Waits until the output so far satisfies `condition`. On timeout, the program is stopped
    /// and its stderr is reported with the end of its output.
    fn wait_for(&mut self, what: &str, condition: impl Fn(&[u8]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if condition(&self.output.lock().unwrap()) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        let exited = self.child.try_wait().unwrap();
        let _ = self.child.kill();
        let stderr = self.stderr.take().map(|h| h.join().unwrap()).unwrap_or_default();
        let output = self.output.lock().unwrap();
        let tail = String::from_utf8_lossy(&output[output.len().saturating_sub(600)..]).into_owned();
        panic!("timed out waiting for {what} (exit: {exited:?})\nstderr: {stderr}\noutput ends with {tail:?}");
    }

    fn type_keys(&mut self, keys: &[u8]) {
        self.keyboard.write_all(keys).unwrap();
    }

    fn write_stdin(&mut self, bytes: &[u8]) {
        let pipe = self.stdin.as_mut().expect("stdin is a pipe");
        pipe.write_all(bytes).unwrap();
        pipe.flush().unwrap();
    }

    fn close_stdin(&mut self) {
        self.stdin.take();
    }

    /// Waits for the program to exit and collects what it wrote.
    fn finish(mut self) -> Run {
        let deadline = Instant::now() + Duration::from_secs(20);
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status.code().unwrap_or(-1);
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!("the program did not exit");
            }
            thread::sleep(Duration::from_millis(10));
        };
        self.stdin.take();
        let stderr = self.stderr.take().unwrap().join().unwrap();
        self.screen.take().unwrap().join().unwrap();
        let output = std::mem::take(&mut *self.output.lock().unwrap());
        Run {
            status,
            output,
            stderr,
        }
    }
}

/// Runs mer to completion on a terminal, with `stdin` piped in when given.
fn run_in_terminal(args: &[&str], stdin: Option<Vec<u8>>, profile: Profile) -> Run {
    match stdin {
        Some(data) => {
            let mut terminal = Terminal::spawn(args, Stdin::Pipe, profile);
            terminal.write_stdin(&data);
            terminal.close_stdin();
            terminal.finish()
        }
        None => Terminal::spawn(args, Stdin::Terminal, profile).finish(),
    }
}

fn run_plain(args: &[&str]) -> Run {
    run_plain_with(args, &[])
}

/// Runs mer without a terminal. An empty configuration file stands in for the user's own
/// unless `env` sets `MER_CONFIG`.
fn run_plain_with(args: &[&str], env: &[(&str, &str)]) -> Run {
    let spawning = SPAWN.lock().unwrap_or_else(PoisonError::into_inner);
    let child = Command::new(MER)
        .args(args)
        .env("MER_CONFIG", "/dev/null")
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(spawning);
    let out = child.wait_with_output().unwrap();
    Run {
        status: out.status.code().unwrap_or(-1),
        output: out.stdout,
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[derive(Debug)]
struct Image {
    id: u32,
    size: (u32, u32),
    cells: (u16, u16),
    rgba: Vec<u8>,
}

/// Decodes every image transmitted in `output`.
fn images(output: &[u8]) -> Vec<Image> {
    let text = String::from_utf8_lossy(output);
    let mut images = Vec::new();
    let mut pending: Option<(HashMap<String, String>, String)> = None;
    for sequence in text.split("\x1b_G").skip(1) {
        let body = &sequence[..sequence.find("\x1b\\").expect("terminated graphics escape")];
        let (control, payload) = body.split_once(';').unwrap_or((body, ""));
        let keys: HashMap<String, String> = control
            .split(',')
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        if pending.is_none() && keys.get("a").is_some_and(|a| a == "q" || a == "d") {
            // The capability probe's query, or a deletion.
            continue;
        }
        let more = keys.get("m").is_some_and(|m| m == "1");
        match &mut pending {
            Some((_, data)) => data.push_str(payload),
            None => pending = Some((keys, payload.to_string())),
        }
        if more {
            continue;
        }
        let (keys, data) = pending.take().unwrap();
        let number = |key: &str| keys[key].parse::<u32>().unwrap();
        assert_eq!(keys["a"], "T");
        assert_eq!(keys["U"], "1");
        assert_eq!(keys["f"], "32");
        assert_eq!(keys["o"], "z");
        let mut rgba = Vec::new();
        ZlibDecoder::new(&STANDARD.decode(data).unwrap()[..])
            .read_to_end(&mut rgba)
            .unwrap();
        let image = Image {
            id: number("i"),
            size: (number("s"), number("v")),
            cells: (number("c") as u16, number("r") as u16),
            rgba,
        };
        assert_eq!(image.rgba.len(), (image.size.0 * image.size.1 * 4) as usize);
        images.push(image);
    }
    images
}

/// Checks that the placeholder grid for `image` has one line per row, each one cell per column.
fn assert_grid(output: &[u8], image: &Image) {
    let text = String::from_utf8_lossy(output);
    let color = if image.id <= 0xff {
        format!("\x1b[38;5;{}m", image.id)
    } else {
        let [_, r, g, b] = image.id.to_be_bytes();
        format!("\x1b[38;2;{r};{g};{b}m")
    };
    let rows: Vec<&str> = text
        .split('\n')
        .filter_map(|line| line.split_once(&color).map(|(_, rest)| rest))
        .collect();
    assert_eq!(rows.len(), usize::from(image.cells.1), "placeholder rows");
    for row in rows {
        let cells = row.chars().filter(|&c| c == PLACEHOLDER).count();
        assert_eq!(cells, usize::from(image.cells.0), "placeholder columns");
    }
}

fn visible_pixels(image: &Image) -> usize {
    image.rgba.chunks(4).filter(|px| px[3] > 0).count()
}

fn premultiplied(image: &Image) -> Pixmap {
    let mut data = image.rgba.clone();
    for px in data.chunks_mut(4) {
        let alpha = u32::from(px[3]);
        for channel in &mut px[..3] {
            *channel = ((u32::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
    let size = IntSize::from_wh(image.size.0, image.size.1).unwrap();
    Pixmap::from_vec(data, size).unwrap()
}

/// Writes images side by side over a terminal background, scaled down to fit their tiles.
fn contact_sheet(path: &Path, images: &[&Image], background: [u8; 3], tile: (u32, u32)) {
    let columns = images.len().clamp(1, 4) as u32;
    let rows = (images.len() as u32).div_ceil(columns).max(1);
    let mut sheet = Pixmap::new(columns * tile.0, rows * tile.1).unwrap();
    let [r, g, b] = background;
    sheet.fill(resvg::tiny_skia::Color::from_rgba8(r, g, b, 255));
    for (index, image) in images.iter().enumerate() {
        let (x, y) = (index as u32 % columns * tile.0, index as u32 / columns * tile.1);
        let scale = (tile.0 as f32 / image.size.0 as f32)
            .min(tile.1 as f32 / image.size.1 as f32)
            .min(1.0);
        sheet.draw_pixmap(
            0,
            0,
            premultiplied(image).as_ref(),
            &PixmapPaint {
                quality: FilterQuality::Bicubic,
                ..PixmapPaint::default()
            },
            Transform::from_row(scale, 0.0, 0.0, scale, x as f32 + 4.0, y as f32 + 4.0),
            None,
        );
    }
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    sheet.save_png(path).unwrap();
}

fn dump(name: &str, image: &Image) {
    if std::env::var_os("MER_E2E_DUMP").is_none() {
        return;
    }
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("e2e/{name}.png"));
    contact_sheet(&path, &[image], BACKGROUND, image.size);
}

/// Renders every diagram in `tests/fixtures/gallery.md` on a dark and a light terminal and
/// writes contact sheets to `target/tmp/gallery/`, for checking themes by eye:
/// `cargo test --test e2e gallery -- --ignored`.
#[test]
#[ignore]
fn gallery() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("gallery");
    for (name, profile, background) in [
        ("dark", GHOSTTY, BACKGROUND),
        ("light", LIGHT_TERMINAL, LIGHT_BACKGROUND),
    ] {
        let run = run_in_terminal(&[&fixture("gallery.md")], None, profile);
        if run.status != 0 {
            eprintln!("{name}: exit {}\n{}", run.status, run.stderr);
        }
        let images = images(&run.output);
        for (page, chunk) in images.chunks(12).enumerate() {
            let chunk: Vec<&Image> = chunk.iter().collect();
            let path = dir.join(format!("{name}-{}.png", page + 1));
            contact_sheet(&path, &chunk, background, (720, 560));
        }
    }
}

#[test]
fn renders_a_file_inline() {
    let run = run_in_terminal(&[&fixture("flowchart.mmd")], None, GHOSTTY);
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    let images = images(&run.output);
    assert_eq!(images.len(), 1);
    let image = &images[0];
    assert_eq!(image.size.0, u32::from(image.cells.0) * CELL.0);
    assert_eq!(image.size.1, u32::from(image.cells.1) * CELL.1);
    assert!(image.cells.0 <= COLS);
    assert_grid(&run.output, image);
    assert!(visible_pixels(image) > 5_000);
    dump("flowchart", image);
}

#[test]
fn stdin_renders_exactly_like_the_file() {
    let source = fs::read(fixture("sequence.mmd")).unwrap();
    let piped = run_in_terminal(&["-"], Some(source), GHOSTTY);
    assert_eq!(piped.status, 0, "stderr: {}", piped.stderr);
    let from_file = run_in_terminal(&[&fixture("sequence.mmd")], None, GHOSTTY);
    let (piped, from_file) = (images(&piped.output), images(&from_file.output));
    assert_eq!(piped.len(), 1);
    assert_eq!(piped[0].size, from_file[0].size);
    assert!(piped[0].rgba == from_file[0].rgba, "stdin and file output differ");
    dump("sequence", &piped[0]);
}

#[test]
fn markdown_shows_every_fence_with_its_heading() {
    let run = run_in_terminal(&[&fixture("doc.md")], None, GHOSTTY);
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    let images = images(&run.output);
    assert_eq!(images.len(), 2);
    let text = String::from_utf8_lossy(&run.output);
    let first = text.find("Request flow").expect("first caption");
    let second = text.find("States").expect("second caption");
    assert!(first < second);
    for image in &images {
        assert_grid(&run.output, image);
    }
    dump("doc-2", &images[1]);
}

#[test]
fn markdown_on_stdin_is_detected() {
    let doc = fs::read(fixture("doc.md")).unwrap();
    let run = run_in_terminal(&["-"], Some(doc), GHOSTTY);
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    assert_eq!(images(&run.output).len(), 2);
}

#[test]
fn syntax_errors_show_a_code_frame() {
    let run = run_in_terminal(&[&fixture("broken.mmd")], None, GHOSTTY);
    assert_eq!(run.status, 1);
    assert!(run.stderr.contains("broken.mmd:3:15"), "{}", run.stderr);
    assert!(run.stderr.contains("Unterminated node label"), "{}", run.stderr);
    assert!(images(&run.output).is_empty());
}

#[test]
fn terminals_without_graphics_get_text_diagrams() {
    let path = scratch_file("plain.mmd", "flowchart LR\n  A[Parse] --> B[Layout] --> C[Rasterize]\n");
    let run = run_in_terminal(&[path.to_str().unwrap()], None, PLAIN_TERMINAL);
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    assert!(run.stderr.contains("drawn as text"), "{}", run.stderr);
    assert!(images(&run.output).is_empty());
    let text = String::from_utf8_lossy(&run.output);
    assert!(text.contains("Rasterize") && text.contains('─'), "{text}");
}

#[test]
fn text_protocol_works_without_a_terminal() {
    let path = scratch_file("text.mmd", "flowchart LR\n  A[Parse] --> B[Layout] --> C[Rasterize]\n");
    let run = run_plain(&["--protocol", "text", path.to_str().unwrap()]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    let text = String::from_utf8_lossy(&run.output);
    assert!(text.contains("Layout") && !text.contains('\x1b'), "{text}");
}

#[test]
fn the_configuration_file_sets_defaults() {
    let config = scratch_file("config.toml", "protocol = \"text\"\n");
    let path = scratch_file("configured.mmd", "flowchart LR\n  A[Parse] --> B[Layout]\n");
    let env = [("MER_CONFIG", config.to_str().unwrap())];
    let run = run_plain_with(&[path.to_str().unwrap()], &env);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert!(String::from_utf8_lossy(&run.output).contains("Layout"));

    let flags_win = run_plain_with(&["--protocol", "kitty", "--check", path.to_str().unwrap()], &env);
    assert_eq!(flags_win.status, 0, "{}", flags_win.stderr);

    let invalid = scratch_file("invalid.toml", "protocol = \"sixel\"\n");
    let run = run_plain_with(&[path.to_str().unwrap()], &[("MER_CONFIG", invalid.to_str().unwrap())]);
    assert_eq!(run.status, 2);
    assert!(run.stderr.contains("invalid configuration"), "{}", run.stderr);
}

#[test]
fn stdout_must_be_a_terminal() {
    let run = run_plain(&[&fixture("flowchart.mmd")]);
    assert_eq!(run.status, 2);
    assert!(run.stderr.contains("not a terminal"), "{}", run.stderr);
}

#[test]
fn check_reports_errors_without_displaying() {
    let ok = run_plain(&["--check", &fixture("flowchart.mmd"), &fixture("doc.md")]);
    assert_eq!(ok.status, 0, "{}", ok.stderr);
    assert!(ok.output.is_empty());
    let bad = run_plain(&["--check", &fixture("broken.mmd")]);
    assert_eq!(bad.status, 1);
    assert!(bad.stderr.contains("broken.mmd:3:15"), "{}", bad.stderr);
}

#[test]
fn exports_png_and_svg() {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("export");
    fs::create_dir_all(&dir).unwrap();
    let png = dir.join("flow.png");
    let run = run_plain(&[&fixture("flowchart.mmd"), "-o", png.to_str().unwrap()]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert!(fs::read(&png).unwrap().starts_with(b"\x89PNG\r\n\x1a\n"));

    let svg = dir.join("doc.svg");
    let run = run_plain(&[&fixture("doc.md"), "-o", svg.to_str().unwrap()]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    for n in 1..=2 {
        let text = fs::read_to_string(dir.join(format!("doc-{n}.svg"))).unwrap();
        assert!(text.starts_with("<svg"));
    }

    let run = run_plain(&[&fixture("flowchart.mmd"), "-o", "-", "--format", "svg"]);
    assert_eq!(run.status, 0, "{}", run.stderr);
    assert!(run.output.starts_with(b"<svg"));
}

#[test]
fn slow_streams_go_live_and_end_like_one_shot_output() {
    let source = fs::read_to_string(fixture("flowchart.mmd")).unwrap();
    let cut = source.find("    P --> L").expect("a line to cut at");
    let mut terminal = Terminal::spawn(&["-"], Stdin::Pipe, GHOSTTY);
    terminal.write_stdin(&source.as_bytes()[..cut]);
    terminal.wait_for("a live preview", |out| contains(out, TRANSMIT));
    terminal.wait_for("the status line", |out| contains(out, b"q to stop"));
    terminal.write_stdin(&source.as_bytes()[cut..]);
    terminal.close_stdin();
    let live = terminal.finish();
    assert_eq!(live.status, 0, "stderr: {}", live.stderr);
    assert!(contains(&live.output, b"\x1b[?25h"), "the cursor is shown again");

    let one_shot = run_in_terminal(&[&fixture("flowchart.mmd")], None, GHOSTTY);
    let live_images = images(&live.output);
    let final_image = live_images.last().expect("final image");
    let expected = &images(&one_shot.output)[0];
    assert!(live_images.len() >= 2, "a preview, then the final image");
    assert_eq!(final_image.size, expected.size);
    assert!(final_image.rgba == expected.rgba, "the final image matches one-shot output");
}

#[test]
fn quitting_a_stream_leaves_the_terminal_usable() {
    let mut terminal = Terminal::spawn(&["-"], Stdin::Pipe, GHOSTTY);
    terminal.write_stdin(b"flowchart LR\n  A --> B\n");
    terminal.wait_for("a live preview", |out| contains(out, TRANSMIT));
    terminal.type_keys(b"q");
    let run = terminal.finish();
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    for restore in [&b"\x1b[?25h"[..], b"\x1b[?2048l", b"\x1b[?2031l"] {
        assert!(contains(&run.output, restore), "{:?} restored", String::from_utf8_lossy(restore));
    }
}

#[test]
fn markdown_streams_print_each_diagram_as_its_fence_closes() {
    let doc = fs::read_to_string(fixture("doc.md")).unwrap();
    let split = doc.find("## States").unwrap();
    let mut terminal = Terminal::spawn(&["-"], Stdin::Pipe, GHOSTTY);
    terminal.write_stdin(&doc.as_bytes()[..split]);
    terminal.wait_for("the first diagram", |out| contains(out, TRANSMIT));
    terminal.write_stdin(&doc.as_bytes()[split..]);
    terminal.close_stdin();
    let run = terminal.finish();
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    assert_eq!(images(&run.output).len(), 2);
    assert!(String::from_utf8_lossy(&run.output).contains("States"));
}

#[test]
fn stream_errors_are_reported_when_the_input_ends() {
    let mut terminal = Terminal::spawn(&["-"], Stdin::Pipe, GHOSTTY);
    terminal.write_stdin(b"flowchart TD\n  A[Start] --> B{Choice}\n");
    terminal.wait_for("a live preview", |out| contains(out, TRANSMIT));
    terminal.write_stdin(b"  B -->|yes| C[Done\n");
    terminal.close_stdin();
    let run = terminal.finish();
    assert_eq!(run.status, 1);
    assert!(run.stderr.contains("<stdin>:3:15"), "{}", run.stderr);
}

#[test]
fn watch_redraws_when_the_file_changes() {
    let path = scratch_file("watched.mmd", "flowchart LR\n  A --> B\n");
    let mut terminal =
        Terminal::spawn(&["-w", path.to_str().unwrap()], Stdin::Terminal, GHOSTTY);
    terminal.wait_for("the first frame", |out| count(out, TRANSMIT) >= 1);
    fs::write(&path, "flowchart LR\n  A --> B --> C --> D --> E\n").unwrap();
    terminal.wait_for("a second frame", |out| count(out, TRANSMIT) >= 2);
    fs::write(&path, "flowchart LR\n  A --> B[\n").unwrap();
    terminal.wait_for("the error", |out| contains(out, b"watched.mmd:2:"));
    terminal.type_keys(b"q");
    let run = terminal.finish();
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    let images = images(&run.output);
    assert!(images.len() >= 2);
    assert!(images[1].size.0 > images[0].size.0, "the longer chain is wider");
}

#[test]
fn viewer_pans_zooms_and_quits() {
    let mut terminal =
        Terminal::spawn(&["-i", &fixture("flowchart.mmd")], Stdin::Terminal, GHOSTTY);
    terminal.wait_for("the first frame", |out| {
        contains(out, b"\x1b[?1049h") && count(out, TRANSMIT) >= 1
    });
    let steps: [(&[u8], usize); 3] = [(b"+", 2), (b"l", 3), (b"\x1b[<64;600;400M", 4)];
    for (keys, frames) in steps {
        terminal.type_keys(keys);
        terminal.wait_for("another frame", |out| count(out, TRANSMIT) >= frames);
    }
    terminal.type_keys(b"q");
    let run = terminal.finish();
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    assert!(contains(&run.output, b"\x1b[?1049l"), "left the alternate screen");
    let images = images(&run.output);
    let viewport = (u32::from(COLS) * CELL.0, u32::from(ROWS - 1) * CELL.1);
    assert!(images.iter().all(|image| image.size == viewport));
    assert!(images[0].rgba != images[1].rgba, "zooming changed the picture");
    assert!(images[1].rgba != images[2].rgba, "panning changed the picture");
    assert!(images[2].rgba != images[3].rgba, "scrolling zoomed the picture");
}

#[test]
fn viewer_moves_between_diagrams() {
    let mut terminal = Terminal::spawn(&["-i", &fixture("doc.md")], Stdin::Terminal, GHOSTTY);
    terminal.wait_for("the first diagram", |out| contains(out, " · 1/2 · ".as_bytes()));
    terminal.type_keys(b"n");
    terminal.wait_for("the second diagram", |out| contains(out, " · 2/2 · ".as_bytes()));
    terminal.type_keys(b"q");
    let run = terminal.finish();
    assert_eq!(run.status, 0, "stderr: {}", run.stderr);
    assert!(images(&run.output).len() >= 2);
}

/// Stops a private tmux server when dropped.
struct TmuxServer(String);

impl Drop for TmuxServer {
    fn drop(&mut self) {
        let _ = Command::new("tmux").args(["-L", &self.0, "kill-server"]).output();
    }
}

#[test]
fn images_pass_through_tmux() {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("tmux is not installed; skipping");
        return;
    }
    let server = TmuxServer(format!("mer-e2e-{}", std::process::id()));
    let config = scratch_file("tmux.conf", "set -g allow-passthrough on\nset -g status off\n");
    let exit_file = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("scratch/tmux-exit");
    let _ = fs::remove_file(&exit_file);
    let command = format!(
        "{MER} {}; echo $? > {}; sleep 5",
        fixture("flowchart.mmd"),
        exit_file.display()
    );
    let args = [
        "-L",
        &server.0,
        "-f",
        config.to_str().unwrap(),
        "new-session",
        "-x",
        "120",
        "-y",
        "40",
        &command,
    ];
    let env = [("TERM", "xterm-256color")];
    let mut terminal = Terminal::spawn_program("tmux", &args, Stdin::Terminal, GHOSTTY, &env);
    terminal.wait_for("an image passed through tmux", |out| contains(out, TRANSMIT));
    let deadline = Instant::now() + Duration::from_secs(10);
    while !exit_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(50));
    }
    drop(server);
    let run = terminal.finish();
    let exit = fs::read_to_string(&exit_file).unwrap_or_default();
    assert_eq!(exit.trim(), "0", "mer's exit status inside tmux");
    let images = images(&run.output);
    assert_eq!(images.len(), 1);
    assert!(images[0].id <= 0xff, "image ids fit a palette color inside tmux");
    assert!(visible_pixels(&images[0]) > 5_000);
}

#[test]
fn watch_rejects_stdin() {
    let run = run_plain(&["-w", "-"]);
    assert_eq!(run.status, 2);
    assert!(run.stderr.contains("--watch needs files"), "{}", run.stderr);
}
