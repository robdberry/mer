//! SVG to pixels with resvg, drawing text with the embedded fonts.

use std::sync::Arc;

use anyhow::Context;
use resvg::{tiny_skia, usvg};

use crate::fonts;
use crate::theme::Rgb;

pub struct Rasterizer {
    options: usvg::Options<'static>,
    system_fonts: bool,
}

impl Rasterizer {
    pub fn new() -> Rasterizer {
        let options = usvg::Options {
            font_family: fonts::FAMILY.to_string(),
            fontdb: Arc::new(fonts::database()),
            ..usvg::Options::default()
        };
        Rasterizer {
            options,
            system_fonts: false,
        }
    }

    /// Parses an SVG document. System fonts are loaded on first need, for characters the
    /// embedded font lacks.
    pub fn parse(&mut self, svg: &str) -> anyhow::Result<usvg::Tree> {
        if !self.system_fonts && fonts::needs_fallback(svg) {
            Arc::make_mut(&mut self.options.fontdb).load_system_fonts();
            self.system_fonts = true;
        }
        usvg::Tree::from_str(svg, &self.options).context("rendered SVG could not be parsed")
    }
}

/// Renders `tree` at `scale` into a frame of `size` pixels, anchored at the top left.
pub fn render(
    tree: &usvg::Tree,
    scale: f32,
    size: (u32, u32),
    background: Option<Rgb>,
) -> tiny_skia::Pixmap {
    let mut pixmap = tiny_skia::Pixmap::new(size.0.max(1), size.1.max(1)).expect("frame fits");
    if let Some(Rgb(r, g, b)) = background {
        pixmap.fill(tiny_skia::Color::from_rgba8(r, g, b, 255));
    }
    resvg::render(
        tree,
        tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
}

/// Pixel data as straight (non-premultiplied) RGBA, which the kitty protocol expects.
pub fn straight_rgba(pixmap: &tiny_skia::Pixmap) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixmap.data().len());
    for pixel in pixmap.pixels() {
        let color = pixel.demultiply();
        rgba.extend_from_slice(&[color.red(), color.green(), color.blue(), color.alpha()]);
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="20">
        <rect x="0" y="0" width="20" height="20" fill="#ff0000" fill-opacity="0.5"/>
        <text x="22" y="15" font-size="12">Hi</text>
    </svg>"##;

    #[test]
    fn renders_at_scale_with_transparent_background() {
        let mut rasterizer = Rasterizer::new();
        let tree = rasterizer.parse(SVG).unwrap();
        assert_eq!((tree.size().width(), tree.size().height()), (40.0, 20.0));
        let pixmap = render(&tree, 2.0, (100, 50), None);
        let rgba = straight_rgba(&pixmap);
        assert_eq!(rgba.len(), 100 * 50 * 4);
        let at = |x: usize, y: usize| &rgba[(y * 100 + x) * 4..(y * 100 + x) * 4 + 4];
        assert_eq!(at(10, 10)[0], 255);
        assert!((126..=129).contains(&at(10, 10)[3]));
        assert_eq!(at(95, 45), [0, 0, 0, 0]);
        let text_pixels = (0..40)
            .flat_map(|y| (44..80).map(move |x| (x, y)))
            .filter(|&(x, y)| at(x, y)[3] > 0)
            .count();
        assert!(text_pixels > 20, "text was drawn with the embedded font");
    }

    #[test]
    fn background_fills_the_frame() {
        let mut rasterizer = Rasterizer::new();
        let tree = rasterizer.parse(SVG).unwrap();
        let rgba = straight_rgba(&render(&tree, 1.0, (60, 30), Some(Rgb(1, 2, 3))));
        assert_eq!(&rgba[rgba.len() - 4..], [1, 2, 3, 255]);
    }
}
