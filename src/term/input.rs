//! What the terminal sends mer: keys, mouse events, and replies to its queries.

use crate::theme::Rgb;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Char(char),
    /// Ctrl with a letter, such as `Ctrl('c')`.
    Ctrl(char),
    Enter,
    Tab,
    Backspace,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseKind {
    /// Button 0 (left), 1 (middle) or 2 (right) pressed.
    Press(u8),
    Release,
    Drag,
    Move,
    ScrollUp,
    ScrollDown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mouse {
    pub kind: MouseKind,
    /// Zero-based position: pixels with SGR-pixel reporting, otherwise cells.
    pub x: u32,
    pub y: u32,
    pub shift: bool,
    pub ctrl: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    Key(Key),
    Mouse(Mouse),
    /// The terminal's light or dark preference, reported on request or when it switches.
    ColorScheme { dark: bool },
    /// In-band resize report (mode 2048), with the text area in pixels.
    Resize {
        cols: u16,
        rows: u16,
        width: u32,
        height: u32,
    },
    /// A color query reply: 10 is the foreground, 11 the background, others palette indexes.
    Color { index: u16, rgb: Rgb },
    /// The cell size in pixels, in reply to `CSI 16 t`.
    CellSize { width: u32, height: u32 },
    /// The text area size in pixels, in reply to `CSI 14 t`.
    TextArea { width: u32, height: u32 },
    /// A kitty graphics protocol reply about image `id`.
    Graphics { id: u32, ok: bool },
    /// The reply to a primary device attributes query (DA1).
    DeviceAttributes,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    Ground,
    Escape,
    Csi,
    Ss3,
    Str(Kind),
    /// An ESC inside a string, which ends it when `\` follows.
    StrEscape(Kind),
}

/// Strings the terminal sends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Osc,
    Apc,
    /// DCS strings carry nothing mer reads.
    Dcs,
}

/// Incremental decoder for everything the terminal sends.
#[derive(Default)]
pub struct Decoder {
    state: State,
    params: String,
    utf8: Vec<u8>,
}

const MAX_PARAMS: usize = 4096;

impl Decoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        for &byte in bytes {
            self.byte(byte, &mut events);
        }
        events
    }

    /// Whether a lone ESC is waiting: it is the Escape key unless more bytes follow promptly.
    pub fn pending_escape(&self) -> bool {
        self.state == State::Escape
    }

    /// Resolves a waiting ESC as the Escape key, once no continuation has arrived.
    pub fn flush(&mut self) -> Option<Event> {
        self.pending_escape().then(|| {
            self.state = State::Ground;
            Event::Key(Key::Escape)
        })
    }

    fn byte(&mut self, byte: u8, events: &mut Vec<Event>) {
        match self.state {
            State::Ground => self.ground(byte, events),
            State::Escape => {
                self.params.clear();
                self.state = match byte {
                    b'[' => State::Csi,
                    b'O' => State::Ss3,
                    b']' => State::Str(Kind::Osc),
                    b'_' => State::Str(Kind::Apc),
                    b'P' => State::Str(Kind::Dcs),
                    0x1b => {
                        events.push(Event::Key(Key::Escape));
                        State::Escape
                    }
                    _ => {
                        // Alt+key: report the key itself.
                        self.state = State::Ground;
                        self.ground(byte, events);
                        return;
                    }
                };
            }
            State::Csi => match byte {
                0x40..=0x7e => {
                    self.state = State::Ground;
                    if let Some(event) = csi(&self.params, byte) {
                        events.push(event);
                    }
                }
                0x1b => self.state = State::Escape,
                _ => self.push(byte),
            },
            State::Ss3 => {
                self.state = State::Ground;
                let key = match byte {
                    b'A' => Key::Up,
                    b'B' => Key::Down,
                    b'C' => Key::Right,
                    b'D' => Key::Left,
                    b'H' => Key::Home,
                    b'F' => Key::End,
                    _ => return,
                };
                events.push(Event::Key(key));
            }
            State::Str(kind) => match byte {
                0x07 => {
                    self.state = State::Ground;
                    events.extend(string(kind, &self.params));
                }
                0x1b => self.state = State::StrEscape(kind),
                _ => self.push(byte),
            },
            State::StrEscape(kind) => {
                self.state = State::Ground;
                if byte == b'\\' {
                    events.extend(string(kind, &self.params));
                } else {
                    self.byte(0x1b, events);
                    self.byte(byte, events);
                }
            }
        }
    }

    fn ground(&mut self, byte: u8, events: &mut Vec<Event>) {
        if byte >= 0x80 {
            self.utf8.push(byte);
            match std::str::from_utf8(&self.utf8) {
                Ok(text) => {
                    events.extend(text.chars().map(|c| Event::Key(Key::Char(c))));
                    self.utf8.clear();
                }
                Err(err) if err.error_len().is_some() || self.utf8.len() >= 4 => self.utf8.clear(),
                Err(_) => {}
            }
            return;
        }
        self.utf8.clear();
        let key = match byte {
            0x1b => {
                self.state = State::Escape;
                return;
            }
            b'\r' | b'\n' => Key::Enter,
            b'\t' => Key::Tab,
            0x7f | 0x08 => Key::Backspace,
            0x01..=0x1a => Key::Ctrl(char::from(b'a' + byte - 1)),
            0x20..=0x7e => Key::Char(char::from(byte)),
            _ => return,
        };
        events.push(Event::Key(key));
    }

    fn push(&mut self, byte: u8) {
        if self.params.len() < MAX_PARAMS {
            self.params.push(char::from(byte));
        }
    }
}

fn csi(params: &str, final_byte: u8) -> Option<Event> {
    let numbers = || params.split(';').map(|p| p.parse::<u32>().ok());
    let key = match (final_byte, params) {
        (b'A', _) => Key::Up,
        (b'B', _) => Key::Down,
        (b'C', _) => Key::Right,
        (b'D', _) => Key::Left,
        (b'H', _) => Key::Home,
        (b'F', _) => Key::End,
        (b'~', _) => match numbers().next().flatten()? {
            1 | 7 => Key::Home,
            4 | 8 => Key::End,
            5 => Key::PageUp,
            6 => Key::PageDown,
            _ => return None,
        },
        (b'M' | b'm', _) if params.starts_with('<') => return mouse(&params[1..], final_byte),
        (b'c', _) if params.starts_with('?') => return Some(Event::DeviceAttributes),
        (b'n', "?997;1") => return Some(Event::ColorScheme { dark: true }),
        (b'n', "?997;2") => return Some(Event::ColorScheme { dark: false }),
        (b't', _) => {
            let values: Vec<u32> = numbers().collect::<Option<_>>()?;
            return match values[..] {
                [48, rows, cols, height, width] => Some(Event::Resize {
                    cols: u16::try_from(cols).ok()?,
                    rows: u16::try_from(rows).ok()?,
                    width,
                    height,
                }),
                [6, height, width] if width > 0 && height > 0 => {
                    Some(Event::CellSize { width, height })
                }
                [4, height, width] if width > 0 && height > 0 => {
                    Some(Event::TextArea { width, height })
                }
                _ => None,
            };
        }
        _ => return None,
    };
    Some(Event::Key(key))
}

fn mouse(params: &str, final_byte: u8) -> Option<Event> {
    let mut values = params.split(';').map(|p| p.parse::<u32>().ok());
    let (code, x, y) = (values.next()??, values.next()??, values.next()??);
    let kind = if code & 64 != 0 {
        if code & 1 == 0 {
            MouseKind::ScrollUp
        } else {
            MouseKind::ScrollDown
        }
    } else if code & 32 != 0 {
        if code & 3 == 3 {
            MouseKind::Move
        } else {
            MouseKind::Drag
        }
    } else if final_byte == b'm' {
        MouseKind::Release
    } else {
        MouseKind::Press((code & 3) as u8)
    };
    Some(Event::Mouse(Mouse {
        kind,
        x: x.saturating_sub(1),
        y: y.saturating_sub(1),
        shift: code & 4 != 0,
        ctrl: code & 16 != 0,
    }))
}

fn osc(body: &str) -> Option<Event> {
    let mut parts = body.splitn(3, ';');
    let (index, spec) = match (parts.next()?, parts.next()?, parts.next()) {
        ("4", index, Some(spec)) => (index.parse().ok()?, spec),
        (index @ ("10" | "11"), spec, None) => (index.parse().ok()?, spec),
        _ => return None,
    };
    Some(Event::Color {
        index,
        rgb: Rgb::parse_x11(spec)?,
    })
}

/// A kitty graphics reply such as `Gi=31;OK`.
fn apc(body: &str) -> Option<Event> {
    let (keys, message) = body.strip_prefix('G')?.split_once(';')?;
    let id = keys.split(',').find_map(|key| key.strip_prefix("i="))?;
    Some(Event::Graphics {
        id: id.parse().ok()?,
        ok: message == "OK",
    })
}

/// What an OSC or APC string reports, if it is something mer reads.
fn string(kind: Kind, body: &str) -> Option<Event> {
    match kind {
        Kind::Osc => osc(body),
        Kind::Apc => apc(body),
        Kind::Dcs => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(bytes: &[u8]) -> Vec<Event> {
        Decoder::default().feed(bytes)
    }

    #[test]
    fn plain_and_control_keys() {
        assert_eq!(
            keys(b"q\x03\r\x7f\t"),
            [
                Event::Key(Key::Char('q')),
                Event::Key(Key::Ctrl('c')),
                Event::Key(Key::Enter),
                Event::Key(Key::Backspace),
                Event::Key(Key::Tab),
            ]
        );
        assert_eq!(keys("é".as_bytes()), [Event::Key(Key::Char('é'))]);
    }

    #[test]
    fn utf8_split_across_reads() {
        let mut decoder = Decoder::default();
        let bytes = "→".as_bytes();
        assert!(decoder.feed(&bytes[..1]).is_empty());
        assert_eq!(decoder.feed(&bytes[1..]), [Event::Key(Key::Char('→'))]);
    }

    #[test]
    fn navigation_keys() {
        assert_eq!(
            keys(b"\x1b[A\x1bOB\x1b[1;2C\x1b[D\x1b[5~\x1b[6~\x1b[H\x1b[4~"),
            [
                Event::Key(Key::Up),
                Event::Key(Key::Down),
                Event::Key(Key::Right),
                Event::Key(Key::Left),
                Event::Key(Key::PageUp),
                Event::Key(Key::PageDown),
                Event::Key(Key::Home),
                Event::Key(Key::End),
            ]
        );
    }

    #[test]
    fn escape_waits_for_a_continuation() {
        let mut decoder = Decoder::default();
        assert!(decoder.feed(b"\x1b").is_empty());
        assert!(decoder.pending_escape());
        assert_eq!(decoder.flush(), Some(Event::Key(Key::Escape)));
        assert_eq!(decoder.flush(), None);
        assert!(decoder.feed(b"\x1b").is_empty());
        assert_eq!(decoder.feed(b"[B"), [Event::Key(Key::Down)]);
    }

    #[test]
    fn sgr_mouse_events() {
        let events = keys(b"\x1b[<0;101;51M\x1b[<32;120;60M\x1b[<0;120;60m\x1b[<64;5;5M\x1b[<81;5;5M");
        let kinds: Vec<_> = events
            .iter()
            .map(|e| match e {
                Event::Mouse(m) => m.kind,
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                MouseKind::Press(0),
                MouseKind::Drag,
                MouseKind::Release,
                MouseKind::ScrollUp,
                MouseKind::ScrollDown,
            ]
        );
        let Event::Mouse(first) = events[0] else { unreachable!() };
        assert_eq!((first.x, first.y), (100, 50));
        let Event::Mouse(last) = events[4] else { unreachable!() };
        assert!(last.ctrl && !last.shift);
    }

    #[test]
    fn terminal_reports() {
        assert_eq!(
            keys(b"\x1b[?997;2n\x1b[48;40;120;1360;2040t\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b]4;4;rgb:00/00/ff\x07"),
            [
                Event::ColorScheme { dark: false },
                Event::Resize {
                    cols: 120,
                    rows: 40,
                    width: 2040,
                    height: 1360
                },
                Event::Color {
                    index: 11,
                    rgb: Rgb(255, 255, 255)
                },
                Event::Color {
                    index: 4,
                    rgb: Rgb(0, 0, 255)
                },
            ]
        );
    }

    #[test]
    fn probe_replies() {
        assert_eq!(
            keys(b"\x1b_Gi=31;OK\x1b\\\x1b_Gi=31;ENOTSUPPORTED:no\x07\x1b[6;34;17t\x1b[4;1360;2890t\x1b[?62;22c"),
            [
                Event::Graphics { id: 31, ok: true },
                Event::Graphics { id: 31, ok: false },
                Event::CellSize {
                    width: 17,
                    height: 34
                },
                Event::TextArea {
                    width: 2890,
                    height: 1360
                },
                Event::DeviceAttributes,
            ]
        );
    }

    #[test]
    fn unknown_sequences_are_skipped() {
        assert_eq!(
            keys(b"\x1b_Xnot graphics\x1b\\\x1bP1$r0m\x1b\\\x1b[>1;10;0c\x1b[5nx"),
            [Event::Key(Key::Char('x'))]
        );
    }
}
