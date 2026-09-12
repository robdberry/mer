//! The embedded Inter font, used for both label measurement and rasterization so that layout
//! boxes match the drawn glyphs on every machine.

use std::sync::Arc;

use resvg::usvg::fontdb;
use skrifa::MetadataProvider;

pub const FAMILY: &str = "Inter";

pub struct Face {
    pub data: &'static [u8],
    pub bold: bool,
    pub italic: bool,
}

pub static FACES: [Face; 4] = [
    Face {
        data: include_bytes!("../assets/fonts/Inter-Regular.ttf"),
        bold: false,
        italic: false,
    },
    Face {
        data: include_bytes!("../assets/fonts/Inter-Italic.ttf"),
        bold: false,
        italic: true,
    },
    Face {
        data: include_bytes!("../assets/fonts/Inter-Bold.ttf"),
        bold: true,
        italic: false,
    },
    Face {
        data: include_bytes!("../assets/fonts/Inter-BoldItalic.ttf"),
        bold: true,
        italic: true,
    },
];

/// A font database holding only the embedded faces, with Inter as the sans-serif family.
pub fn database() -> fontdb::Database {
    let mut db = fontdb::Database::new();
    for face in &FACES {
        db.load_font_source(fontdb::Source::Binary(Arc::new(face.data)));
    }
    db.set_sans_serif_family(FAMILY);
    db
}

/// The embedded face for a CSS `font-weight` and `font-style`.
pub fn face(weight: Option<&str>, style: Option<&str>) -> &'static Face {
    let bold = match weight.map(str::trim) {
        Some("bold" | "bolder") => true,
        Some(number) => number.parse::<u16>().is_ok_and(|w| w >= 600),
        None => false,
    };
    let italic = matches!(style.map(str::trim), Some("italic" | "oblique"));
    FACES
        .iter()
        .find(|face| face.bold == bold && face.italic == italic)
        .expect("all four faces are embedded")
}

/// Whether `text` has characters Inter cannot draw, so system fonts are needed as fallback.
pub fn needs_fallback(text: &str) -> bool {
    let Ok(font) = skrifa::FontRef::new(FACES[0].data) else {
        return true;
    };
    let charmap = font.charmap();
    text.chars()
        .filter(|c| !c.is_ascii() && !c.is_whitespace() && !c.is_control())
        .any(|c| charmap.map(c).is_none())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faces_follow_css_weight_and_style() {
        assert!(!face(None, None).bold);
        assert!(face(Some("bold"), None).bold);
        assert!(face(Some("600"), None).bold);
        assert!(!face(Some("500"), None).bold);
        assert!(face(Some("normal"), Some("italic")).italic);
    }

    #[test]
    fn inter_covers_latin_greek_and_cyrillic() {
        assert!(!needs_fallback("Straße, café, Ωmega, Привет → done"));
        assert!(needs_fallback("図"));
        assert!(needs_fallback("🦀"));
    }

    #[test]
    fn database_resolves_inter_for_sans_serif() {
        let db = database();
        assert_eq!(db.len(), 4);
        let query = fontdb::Query {
            families: &[fontdb::Family::SansSerif],
            weight: fontdb::Weight::BOLD,
            ..fontdb::Query::default()
        };
        let id = db.query(&query).expect("bold sans-serif");
        let info = db.face(id).unwrap();
        assert_eq!(info.families[0].0, FAMILY);
        assert_eq!(info.weight, fontdb::Weight::BOLD);
    }
}
