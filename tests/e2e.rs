//! End-to-end tests: `mer` runs on a pseudo-terminal that answers its capability probe the way
//! Ghostty does, and the tests decode what it writes back into images and placeholder grids.
//!
//! Set `MER_E2E_DUMP=1` to write each decoded image to `target/e2e/`, composited over the fake
//! terminal's background, for inspection.

use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::read::ZlibDecoder;
use resvg::tiny_skia::{FilterQuality, IntSize, Pixmap, PixmapPaint, Transform};

const MER: &str = env!("CARGO_BIN_EXE_mer");
const COLS: u16 = 120;
const ROWS: u16 = 40;
const CELL: (u32, u32) = (17, 34);
const BACKGROUND: [u8; 3] = [0x1e, 0x1e, 0x2e];
const PLACEHOLDER: char = '\u{10EEEE}';

/// What Ghostty sends back for mer's probe.
const GHOSTTY: &[u8] = b"\x1b_Gi=31;OK\x1b\\\x1b[6;34;17t\x1b[4;1360;2040t\
\x1b]10;rgb:cdcd/d6d6/f4f4\x1b\\\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\
\x1b]4;1;rgb:f3f3/8b8b/a8a8\x1b\\\x1b]4;2;rgb:a6a6/e3e3/a1a1\x1b\\\
\x1b]4;3;rgb:f9f9/e2e2/afaf\x1b\\\x1b]4;4;rgb:8989/b4b4/fafa\x1b\\\
\x1b]4;5;rgb:cbcb/a6a6/f7f7\x1b\\\x1b]4;6;rgb:9494/e2e2/d5d5\x1b\\\
\x1b[?997;1n\x1b[?62;22c";

/// A light terminal theme.
const LIGHT_TERMINAL: &[u8] = b"\x1b_Gi=31;OK\x1b\\\x1b[6;34;17t\x1b[4;1360;2040t\
\x1b]10;rgb:4c4c/4f4f/6969\x1b\\\x1b]11;rgb:efef/f1f1/f5f5\x1b\\\
\x1b]4;1;rgb:d2d2/0f0f/3939\x1b\\\x1b]4;2;rgb:4040/a0a0/2b2b\x1b\\\
\x1b]4;3;rgb:dfdf/8e8e/1d1d\x1b\\\x1b]4;4;rgb:1e1e/6666/f5f5\x1b\\\
\x1b]4;5;rgb:8888/3939/efef\x1b\\\x1b]4;6;rgb:1717/9292/9999\x1b\\\
\x1b[?997;2n\x1b[?62;22c";
const LIGHT_BACKGROUND: [u8; 3] = [0xef, 0xf1, 0xf5];

/// A terminal that answers DA1 and nothing else.
const PLAIN_TERMINAL: &[u8] = b"\x1b[?62;22c";

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
    unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) }
}

/// Runs mer with a pseudo-terminal as its controlling terminal and stdout. The terminal sends
/// `replies` once it sees the DA1 query that ends the probe. With `stdin`, input is piped.
fn run_in_terminal(args: &[&str], stdin: Option<Vec<u8>>, replies: &'static [u8]) -> Run {
    let (master, slave) = open_pty();
    let mut command = Command::new(MER);
    command
        .args(args)
        .env("MER_PROBE_TIMEOUT_MS", "300")
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::piped());
    if stdin.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::from(slave.try_clone().unwrap()));
    }
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(1, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn mer");
    drop(command);
    drop(slave);

    let mut master = File::from(master);
    let mut answer = master.try_clone().unwrap();
    let terminal = thread::spawn(move || {
        let mut output = Vec::new();
        let mut buf = [0u8; 1 << 16];
        let mut answered = false;
        while let Ok(n @ 1..) = master.read(&mut buf) {
            output.extend_from_slice(&buf[..n]);
            if !answered && output.windows(3).any(|w| w == b"\x1b[c") {
                answer.write_all(replies).unwrap();
                answered = true;
            }
        }
        output
    });
    if let Some(data) = stdin {
        let mut pipe = child.stdin.take().unwrap();
        thread::spawn(move || pipe.write_all(&data));
    }
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    let status = child.wait().unwrap().code().unwrap_or(-1);
    Run {
        status,
        output: terminal.join().unwrap(),
        stderr,
    }
}

fn run_plain(args: &[&str]) -> Run {
    let out = Command::new(MER)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
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
        if pending.is_none() && keys.get("a").is_some_and(|a| a == "q") {
            // The capability probe's query, written to the same terminal.
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
    let [_, r, g, b] = image.id.to_be_bytes();
    let color = format!("\x1b[38;2;{r};{g};{b}m");
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
    for (name, replies, background) in [
        ("dark", GHOSTTY, BACKGROUND),
        ("light", LIGHT_TERMINAL, LIGHT_BACKGROUND),
    ] {
        let run = run_in_terminal(&[&fixture("gallery.md")], None, replies);
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
fn terminals_without_graphics_get_a_clear_error() {
    let run = run_in_terminal(&[&fixture("flowchart.mmd")], None, PLAIN_TERMINAL);
    assert_eq!(run.status, 2);
    assert!(run.stderr.contains("kitty graphics protocol"), "{}", run.stderr);
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
