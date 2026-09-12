//! Kitty graphics protocol encoding.
//!
//! Images are shown through virtual placements and Unicode placeholders: the pixels are
//! transmitted with `U=1`, then a grid of `U+10EEEE` cells marks where the image appears. The
//! grid is ordinary text to the terminal, so the image scrolls with the output, survives
//! redraws, and is replaced in place when the same image id is sent again.

use std::fmt::Write as _;
use std::io::{self, Write};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use flate2::Compression;
use flate2::write::ZlibEncoder;

use super::diacritics::DIACRITICS;

pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Rows and columns past this cannot be addressed with placeholder diacritics.
pub const MAX_CELLS: u16 = DIACRITICS.len() as u16;

/// Largest payload per escape sequence allowed by the protocol.
const CHUNK_BYTES: usize = 4096;

/// Transmits straight (non-premultiplied) RGBA pixels as image `id` and creates a virtual
/// placement spanning `cols`×`rows` cells.
pub fn transmit_virtual(
    out: &mut impl Write,
    id: u32,
    rgba: &[u8],
    (width, height): (u32, u32),
    (cols, rows): (u16, u16),
) -> io::Result<()> {
    debug_assert_eq!(rgba.len(), width as usize * height as usize * 4);
    let mut zlib = ZlibEncoder::new(Vec::with_capacity(rgba.len() / 16), Compression::fast());
    zlib.write_all(rgba)?;
    let payload = STANDARD.encode(zlib.finish()?);
    let control =
        format!("a=T,U=1,i={id},f=32,s={width},v={height},c={cols},r={rows},t=d,o=z,q=2");
    write_chunked(out, &control, payload.as_bytes())
}

fn write_chunked(out: &mut impl Write, control: &str, payload: &[u8]) -> io::Result<()> {
    let mut chunks = payload.chunks(CHUNK_BYTES).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        if first {
            write!(out, "\x1b_G{control},m={more};")?;
            first = false;
        } else {
            write!(out, "\x1b_Gm={more},q=2;")?;
        }
        out.write_all(chunk)?;
        out.write_all(b"\x1b\\")?;
    }
    Ok(())
}

/// Deletes image `id` and frees its data.
pub fn delete(out: &mut impl Write, id: u32) -> io::Result<()> {
    write!(out, "\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

/// Appends one row of placeholder cells for image `id`, wrapped in the foreground color that
/// encodes the id. The caller adds the line break.
pub fn placeholder_row(out: &mut String, id: u32, row: u16, cols: u16) {
    if id <= 0xff {
        // A palette index survives multiplexers that downsample truecolor.
        let _ = write!(out, "\x1b[38;5;{id}m");
    } else {
        let [_, r, g, b] = id.to_be_bytes();
        let _ = write!(out, "\x1b[38;2;{r};{g};{b}m");
    }
    let row_mark = DIACRITICS[usize::from(row)];
    for col in 0..cols {
        out.push(PLACEHOLDER);
        out.push(row_mark);
        out.push(DIACRITICS[usize::from(col)]);
    }
    out.push_str("\x1b[39m");
}

/// A random image id. Ids have to fit the foreground color that marks placeholder cells:
/// 24 bits normally, 8 bits inside tmux, which may turn truecolor into palette colors.
/// Randomness keeps them from colliding with images other programs have put on the screen.
pub fn random_id() -> u32 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u32(std::process::id());
    let mask = if super::inside_tmux() { 0xff } else { 0xff_ffff };
    ((hasher.finish() & mask) as u32).max(1)
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use flate2::read::ZlibDecoder;

    use super::*;

    /// Splits output into (control, payload) pairs, one per graphics escape sequence.
    fn sequences(bytes: &[u8]) -> Vec<(String, String)> {
        std::str::from_utf8(bytes)
            .unwrap()
            .split_terminator("\x1b\\")
            .map(|seq| {
                let body = seq.strip_prefix("\x1b_G").expect("graphics escape");
                let (control, payload) = body.split_once(';').unwrap();
                (control.to_string(), payload.to_string())
            })
            .collect()
    }

    fn decode(seqs: &[(String, String)]) -> Vec<u8> {
        let joined: String = seqs.iter().map(|(_, payload)| payload.as_str()).collect();
        let compressed = STANDARD.decode(joined).unwrap();
        let mut rgba = Vec::new();
        ZlibDecoder::new(&compressed[..]).read_to_end(&mut rgba).unwrap();
        rgba
    }

    #[test]
    fn large_images_are_chunked_and_round_trip() {
        // Noise does not compress, so the payload needs several chunks.
        let mut state: u32 = 0x1234_5678;
        let rgba: Vec<u8> = (0..64 * 64 * 4)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect();
        let mut out = Vec::new();
        transmit_virtual(&mut out, 0xabcdef, &rgba, (64, 64), (4, 2)).unwrap();

        let seqs = sequences(&out);
        assert!(seqs.len() > 2);
        assert_eq!(
            seqs[0].0,
            "a=T,U=1,i=11259375,f=32,s=64,v=64,c=4,r=2,t=d,o=z,q=2,m=1"
        );
        for (control, _) in &seqs[1..seqs.len() - 1] {
            assert_eq!(control, "m=1,q=2");
        }
        assert_eq!(seqs.last().unwrap().0, "m=0,q=2");
        assert!(seqs.iter().all(|(_, payload)| payload.len() <= CHUNK_BYTES));
        assert_eq!(decode(&seqs), rgba);
    }

    #[test]
    fn small_images_fit_one_sequence() {
        let rgba = [0u8; 2 * 2 * 4];
        let mut out = Vec::new();
        transmit_virtual(&mut out, 7, &rgba, (2, 2), (1, 1)).unwrap();
        let seqs = sequences(&out);
        assert_eq!(seqs.len(), 1);
        assert!(seqs[0].0.ends_with(",m=0"));
        assert_eq!(decode(&seqs), rgba);
    }

    #[test]
    fn placeholder_cells_encode_id_row_and_column() {
        let mut row = String::new();
        placeholder_row(&mut row, 0x123456, 1, 3);
        let p = PLACEHOLDER;
        let expected = format!(
            "\x1b[38;2;18;52;86m{p}\u{30D}\u{305}{p}\u{30D}\u{30D}{p}\u{30D}\u{30E}\x1b[39m"
        );
        assert_eq!(row, expected);
    }

    #[test]
    fn small_ids_use_palette_colors() {
        let mut row = String::new();
        placeholder_row(&mut row, 200, 0, 1);
        assert!(row.starts_with("\x1b[38;5;200m"));
    }

    #[test]
    fn delete_frees_image_data() {
        let mut out = Vec::new();
        delete(&mut out, 42).unwrap();
        assert_eq!(out, b"\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
    }

    #[test]
    fn ids_fit_in_24_bits() {
        for _ in 0..100 {
            let id = random_id();
            assert!((1..=0xff_ffff).contains(&id));
        }
    }
}
