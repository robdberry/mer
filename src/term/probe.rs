//! One-round-trip terminal capability probe.
//!
//! Every query is written at once, followed by a DA1 request. Terminals answer in order and
//! all of them answer DA1, so its reply marks the end: supported features have replied by then
//! and no individual query has to time out.

use std::io;
use std::time::{Duration, Instant};

use super::input::{Decoder, Event};
use super::tty::Tty;
use crate::theme::Palette;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Caps {
    pub cols: u16,
    pub rows: u16,
    /// Cell size in device pixels, as (width, height).
    pub cell: Option<(u32, u32)>,
    /// Text area size in device pixels, as (width, height).
    pub text_area: Option<(u32, u32)>,
    pub kitty_graphics: bool,
    pub palette: Palette,
    /// The terminal's light/dark preference, when it reports one.
    pub dark: Option<bool>,
    /// Whether the terminal answered DA1 before the timeout.
    pub responded: bool,
}

const KITTY_QUERY_ID: u32 = 31;

/// How long to wait for graphics replies after tmux has answered DA1 itself: tmux replies at
/// once, while passthrough replies travel from the terminal behind it.
const TMUX_GRACE: Duration = Duration::from_millis(250);

pub fn queries(tmux: bool) -> Vec<u8> {
    let kitty = format!("\x1b_Gi={KITTY_QUERY_ID},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    let mut q = if tmux {
        super::passthrough(kitty.as_bytes())
    } else {
        kitty.into_bytes()
    };
    q.extend_from_slice(b"\x1b[16t\x1b[14t");
    q.extend_from_slice(color_queries().as_bytes());
    q.extend_from_slice(b"\x1b[?996n\x1b[c");
    q
}

/// Queries for the colors the `terminal` theme is derived from.
pub fn color_queries() -> String {
    let mut q = String::from("\x1b]10;?\x1b\\\x1b]11;?\x1b\\");
    for index in 1..=6 {
        q.push_str(&format!("\x1b]4;{index};?\x1b\\"));
    }
    q
}

pub fn probe(tty: &Tty, timeout: Duration) -> io::Result<Caps> {
    let size = tty.winsize()?;
    let mut caps = Caps {
        cols: size.ws_col,
        rows: size.ws_row,
        ..Caps::default()
    };
    {
        let tmux = super::inside_tmux();
        let _raw = tty.raw(false)?;
        tty.write_all(&queries(tmux))?;
        let mut deadline = Instant::now() + timeout;
        let mut decoder = Decoder::default();
        let mut buf = [0u8; 4096];
        'read: while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            let n = match tty.read_timeout(&mut buf, left)? {
                None => continue,
                Some(0) => break,
                Some(n) => n,
            };
            for event in decoder.feed(&buf[..n]) {
                if apply(&mut caps, event) && !tmux {
                    break 'read;
                }
                if caps.responded {
                    if caps.kitty_graphics {
                        break 'read;
                    }
                    deadline = deadline.min(Instant::now() + TMUX_GRACE);
                }
            }
        }
    }
    if caps.cell.is_none() {
        let pixels = match (size.ws_xpixel, size.ws_ypixel) {
            (0, _) | (_, 0) => caps.text_area,
            (w, h) => Some((u32::from(w), u32::from(h))),
        };
        caps.cell = cell_size(pixels, caps.cols, caps.rows);
    }
    Ok(caps)
}

fn cell_size(pixels: Option<(u32, u32)>, cols: u16, rows: u16) -> Option<(u32, u32)> {
    let (w, h) = pixels?;
    if cols == 0 || rows == 0 {
        return None;
    }
    let cell = (w / u32::from(cols), h / u32::from(rows));
    (cell.0 > 0 && cell.1 > 0).then_some(cell)
}

/// Records one reply. Returns true for the DA1 reply that ends the probe.
fn apply(caps: &mut Caps, event: Event) -> bool {
    match event {
        Event::DeviceAttributes => {
            caps.responded = true;
            return true;
        }
        Event::Graphics { id, ok } if id == KITTY_QUERY_ID => caps.kitty_graphics = ok,
        Event::CellSize { width, height } => caps.cell = Some((width, height)),
        Event::TextArea { width, height } => caps.text_area = Some((width, height)),
        Event::Color { index, rgb } => caps.palette.set(index, rgb),
        Event::ColorScheme { dark } => caps.dark = Some(dark),
        _ => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Rgb;

    const GHOSTTY_REPLIES: &[u8] = b"\x1b_Gi=31;OK\x1b\\\x1b[6;34;17t\x1b[4;1360;2890t\
\x1b]10;rgb:d8d8/dada/dede\x1b\\\x1b]11;rgb:1e1e/1e1e/2e2e\x1b\\\
\x1b]4;3;rgb:f9f9/e2e2/afaf\x1b\\\x1b]4;4;rgb:8989/b4b4/fafa\x1b\\\
\x1b]4;5;rgb:cbcb/a6a6/f7f7\x1b\\\x1b[?997;1n\x1b[?62;22c";

    fn run(chunks: &[&[u8]]) -> (Caps, bool) {
        let mut caps = Caps::default();
        let mut decoder = Decoder::default();
        let mut done = false;
        for chunk in chunks {
            for event in decoder.feed(chunk) {
                done |= apply(&mut caps, event);
            }
        }
        (caps, done)
    }

    #[test]
    fn parses_a_full_ghostty_reply() {
        let (caps, done) = run(&[GHOSTTY_REPLIES]);
        assert!(done && caps.responded);
        assert!(caps.kitty_graphics);
        assert_eq!(caps.cell, Some((17, 34)));
        assert_eq!(caps.text_area, Some((2890, 1360)));
        assert_eq!(caps.palette.fg, Some(Rgb(0xd8, 0xda, 0xde)));
        assert_eq!(caps.palette.bg, Some(Rgb(0x1e, 0x1e, 0x2e)));
        assert_eq!(caps.palette.ansi[3], Some(Rgb(0x89, 0xb4, 0xfa)));
        assert_eq!(caps.dark, Some(true));
    }

    #[test]
    fn replies_split_across_reads() {
        let (a, b) = GHOSTTY_REPLIES.split_at(37);
        let (b, c) = b.split_at(5);
        assert_eq!(run(&[a, b, c]), run(&[GHOSTTY_REPLIES]));
    }

    #[test]
    fn terminal_without_graphics() {
        let (caps, done) = run(&[b"\x1b_Gi=31;ENOTSUPPORTED:no\x1b\\\x1b[?1;2c"]);
        assert!(done);
        assert!(!caps.kitty_graphics);
        let (caps, done) = run(&[b"typed ahead\x1b[?64;4c"]);
        assert!(done && !caps.kitty_graphics && caps.cell.is_none());
    }

    #[test]
    fn inside_tmux_the_graphics_query_is_wrapped() {
        let wrapped = queries(true);
        assert!(wrapped.starts_with(b"\x1bPtmux;\x1b\x1b_Gi=31"));
        assert!(wrapped.ends_with(b"\x1b[c"));
        assert!(queries(false).starts_with(b"\x1b_Gi=31"));
    }

    #[test]
    fn osc_terminated_by_bel() {
        let (caps, _) = run(&[b"\x1b]11;rgb:ffff/ffff/ffff\x07"]);
        assert_eq!(caps.palette.bg, Some(Rgb(255, 255, 255)));
    }

    #[test]
    fn cell_size_from_pixels() {
        assert_eq!(cell_size(Some((1700, 1020)), 100, 30), Some((17, 34)));
        assert_eq!(cell_size(None, 100, 30), None);
        assert_eq!(cell_size(Some((50, 50)), 100, 30), None);
    }
}
