//! Terminal I/O: the controlling tty, capability probing and the kitty graphics protocol.

pub mod diacritics;
pub mod input;
pub mod kitty;
pub mod probe;
pub mod tty;

use std::borrow::Cow;
use std::sync::OnceLock;

/// Whether mer runs inside tmux, which forwards kitty graphics sequences to the terminal only
/// when they are wrapped in its passthrough escape, and only with `allow-passthrough` on.
pub fn inside_tmux() -> bool {
    static TMUX: OnceLock<bool> = OnceLock::new();
    *TMUX.get_or_init(|| std::env::var_os("TMUX").is_some_and(|value| !value.is_empty()))
}

/// Output prepared for the terminal: inside tmux, graphics sequences are wrapped for passthrough.
pub fn for_terminal(bytes: &[u8]) -> Cow<'_, [u8]> {
    if inside_tmux() {
        Cow::Owned(passthrough(bytes))
    } else {
        Cow::Borrowed(bytes)
    }
}

/// Wraps each `ESC _ G … ESC \` sequence in tmux's `ESC P tmux; … ESC \`, doubling its escapes.
pub fn passthrough(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 256 + 16);
    let mut rest = bytes;
    while let Some(start) = find(rest, b"\x1b_G") {
        let Some(length) = find(&rest[start..], b"\x1b\\") else {
            break;
        };
        let end = start + length + 2;
        out.extend_from_slice(&rest[..start]);
        out.extend_from_slice(b"\x1bPtmux;");
        for &byte in &rest[start..end] {
            if byte == 0x1b {
                out.push(0x1b);
            }
            out.push(byte);
        }
        out.extend_from_slice(b"\x1b\\");
        rest = &rest[end..];
    }
    out.extend_from_slice(rest);
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphics_sequences_are_wrapped_for_tmux() {
        let output = b"text\x1b_Ga=T,i=1;QUFB\x1b\\\x1b[38;5;1m\x1b_Gm=0;\x1b\\done";
        assert_eq!(
            passthrough(output),
            b"text\x1bPtmux;\x1b\x1b_Ga=T,i=1;QUFB\x1b\x1b\\\x1b\\\x1b[38;5;1m\
\x1bPtmux;\x1b\x1b_Gm=0;\x1b\x1b\\\x1b\\done"
                .to_vec()
        );
    }

    #[test]
    fn other_output_passes_unchanged() {
        let output = b"\x1b[?2026h plain \x1b]11;?\x1b\\ text";
        assert_eq!(passthrough(output), output.to_vec());
    }
}
