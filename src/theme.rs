//! Terminal colors and the Mermaid theme derived from them.

use serde_json::{Map, Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    /// Parses a color the way terminals report it: `rgb:r/g/b` with one to four hex digits
    /// per channel, or `#rgb` / `#rrggbb`.
    pub fn parse_x11(spec: &str) -> Option<Rgb> {
        let spec = spec.trim();
        if let Some(channels) = spec.strip_prefix("rgb:") {
            let mut parts = channels.split('/').map(scaled_channel);
            let color = Rgb(parts.next()??, parts.next()??, parts.next()??);
            return parts.next().is_none().then_some(color);
        }
        let hex = spec.strip_prefix('#')?;
        let channel = |start: usize, len: usize| scaled_channel(hex.get(start..start + len)?);
        match hex.len() {
            3 => Some(Rgb(channel(0, 1)?, channel(1, 1)?, channel(2, 1)?)),
            6 => Some(Rgb(channel(0, 2)?, channel(2, 2)?, channel(4, 2)?)),
            _ => None,
        }
    }

    /// Interpolates towards `other`; `t` runs from 0 (self) to 1 (other).
    pub fn mix(self, other: Rgb, t: f32) -> Rgb {
        let lerp = |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u8;
        Rgb(lerp(self.0, other.0), lerp(self.1, other.1), lerp(self.2, other.2))
    }

    /// WCAG relative luminance, 0 for black to 1 for white.
    pub fn luminance(self) -> f32 {
        let linear = |c: u8| {
            let c = f32::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(self.0) + 0.7152 * linear(self.1) + 0.0722 * linear(self.2)
    }

    pub fn is_dark(self) -> bool {
        self.luminance() < 0.2
    }

    pub fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
    }
}

fn scaled_channel(digits: &str) -> Option<u8> {
    if digits.is_empty() || digits.len() > 4 {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let max = (1u32 << (4 * digits.len())) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

/// Colors reported by the terminal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Palette {
    pub fg: Option<Rgb>,
    pub bg: Option<Rgb>,
    /// ANSI colors 1 to 6: red, green, yellow, blue, magenta, cyan.
    pub ansi: [Option<Rgb>; 6],
}

impl Palette {
    /// Records a color query reply: 10 is the foreground, 11 the background, 1 to 6 are ANSI.
    pub fn set(&mut self, index: u16, color: Rgb) {
        match index {
            10 => self.fg = Some(color),
            11 => self.bg = Some(color),
            1..=6 => self.ansi[usize::from(index - 1)] = Some(color),
            _ => {}
        }
    }
}

/// ANSI colors 1 to 6 for terminals that don't report theirs (One Dark and One Light).
const DARK_ANSI: [Rgb; 6] = [
    Rgb(0xe0, 0x6c, 0x75),
    Rgb(0x98, 0xc3, 0x79),
    Rgb(0xe5, 0xc0, 0x7b),
    Rgb(0x61, 0xaf, 0xef),
    Rgb(0xc6, 0x78, 0xdd),
    Rgb(0x56, 0xb6, 0xc2),
];
const LIGHT_ANSI: [Rgb; 6] = [
    Rgb(0xe4, 0x56, 0x49),
    Rgb(0x50, 0xa1, 0x4f),
    Rgb(0xc1, 0x84, 0x01),
    Rgb(0x40, 0x78, 0xf2),
    Rgb(0xa6, 0x26, 0xa4),
    Rgb(0x01, 0x84, 0xbc),
];

/// Mermaid configuration for the `terminal` theme: Mermaid's `base` theme with every color
/// derived from the terminal's own. `None` unless the terminal reported fg and bg.
///
/// Variables are set explicitly rather than left to the base theme, which falls back to its
/// light default colors for several of them (state backgrounds, gantt sections, pie text).
pub fn terminal_theme(palette: &Palette) -> Option<Value> {
    let (fg, bg) = (palette.fg?, palette.bg?);
    let dark = bg.is_dark();
    let fallback = if dark { DARK_ANSI } else { LIGHT_ANSI };
    let ansi: [Rgb; 6] = std::array::from_fn(|i| palette.ansi[i].unwrap_or(fallback[i]));
    let [red, green, yellow, blue, magenta, cyan] = ansi;

    let tint = |color: Rgb, amount: f32| bg.mix(color, amount).hex();
    let ink = fg.hex();
    let paper = bg.hex();
    let line = tint(fg, 0.65);
    let node = tint(blue, 0.20);
    let border = tint(blue, 0.70);
    let subtle = tint(fg, 0.08);

    let colors: Vec<(&str, String)> = vec![
        ("background", paper.clone()),
        ("textColor", ink.clone()),
        ("titleColor", ink.clone()),
        ("lineColor", line.clone()),
        ("arrowheadColor", line.clone()),
        ("primaryColor", node.clone()),
        ("primaryBorderColor", border.clone()),
        ("primaryTextColor", ink.clone()),
        ("secondaryColor", tint(magenta, 0.20)),
        ("secondaryBorderColor", tint(magenta, 0.70)),
        ("secondaryTextColor", ink.clone()),
        ("tertiaryColor", subtle.clone()),
        ("tertiaryBorderColor", tint(fg, 0.30)),
        ("tertiaryTextColor", ink.clone()),
        ("mainBkg", node.clone()),
        ("nodeBorder", border.clone()),
        ("nodeTextColor", ink.clone()),
        ("clusterBkg", tint(fg, 0.05)),
        ("clusterBorder", tint(fg, 0.30)),
        ("edgeLabelBackground", paper.clone()),
        ("defaultLinkColor", line.clone()),
        ("errorBkgColor", tint(red, 0.30)),
        ("errorTextColor", ink.clone()),
        // Sequence diagrams.
        ("noteBkgColor", tint(yellow, 0.25)),
        ("noteBorderColor", tint(yellow, 0.60)),
        ("noteTextColor", ink.clone()),
        ("actorBkg", node.clone()),
        ("actorBorder", border.clone()),
        ("actorTextColor", ink.clone()),
        ("actorLineColor", tint(fg, 0.45)),
        ("signalColor", ink.clone()),
        ("signalTextColor", ink.clone()),
        ("labelBoxBkgColor", node.clone()),
        ("labelBoxBorderColor", border.clone()),
        ("labelTextColor", ink.clone()),
        ("loopTextColor", ink.clone()),
        ("activationBkgColor", tint(fg, 0.15)),
        ("activationBorderColor", line.clone()),
        ("sequenceNumberColor", paper.clone()),
        // Class and state diagrams.
        ("classText", ink.clone()),
        ("stateBkg", node.clone()),
        ("stateBorder", border.clone()),
        ("stateLabelColor", ink.clone()),
        ("labelBackgroundColor", node.clone()),
        ("compositeBackground", paper.clone()),
        ("compositeTitleBackground", node.clone()),
        ("compositeBorder", border.clone()),
        ("altBackground", subtle.clone()),
        ("innerEndBackground", border.clone()),
        ("specialStateColor", line.clone()),
        ("transitionColor", line.clone()),
        ("transitionLabelColor", ink.clone()),
        // Entity relationship diagrams.
        ("attributeBackgroundColorOdd", tint(fg, 0.04)),
        ("attributeBackgroundColorEven", tint(fg, 0.10)),
        // Gantt charts.
        ("sectionBkgColor", tint(blue, 0.10)),
        ("altSectionBkgColor", paper.clone()),
        ("sectionBkgColor2", tint(magenta, 0.10)),
        ("excludeBkgColor", tint(fg, 0.05)),
        ("gridColor", tint(fg, 0.20)),
        ("taskBkgColor", tint(blue, 0.45)),
        ("taskBorderColor", tint(blue, 0.80)),
        ("taskTextColor", ink.clone()),
        ("taskTextLightColor", ink.clone()),
        ("taskTextDarkColor", ink.clone()),
        ("taskTextOutsideColor", ink.clone()),
        ("taskTextClickableColor", blue.hex()),
        ("activeTaskBkgColor", tint(blue, 0.25)),
        ("activeTaskBorderColor", tint(blue, 0.80)),
        ("doneTaskBkgColor", tint(fg, 0.20)),
        ("doneTaskBorderColor", tint(fg, 0.45)),
        ("critBkgColor", tint(red, 0.45)),
        ("critBorderColor", tint(red, 0.80)),
        ("todayLineColor", red.hex()),
        ("vertLineColor", cyan.hex()),
        // Pie charts, journeys and git graphs.
        ("pieTitleTextColor", ink.clone()),
        ("pieSectionTextColor", paper.clone()),
        ("pieLegendTextColor", ink.clone()),
        ("pieStrokeColor", paper.clone()),
        ("pieOuterStrokeColor", line.clone()),
        ("faceColor", tint(yellow, 0.70)),
        ("commitLabelColor", ink.clone()),
        ("commitLabelBackground", subtle.clone()),
        ("tagLabelColor", ink.clone()),
        ("tagLabelBackground", tint(yellow, 0.30)),
        ("tagLabelBorder", tint(yellow, 0.70)),
        // Quadrant charts.
        ("quadrant1Fill", tint(blue, 0.18)),
        ("quadrant2Fill", tint(magenta, 0.14)),
        ("quadrant3Fill", tint(fg, 0.06)),
        ("quadrant4Fill", tint(green, 0.14)),
        ("quadrant1TextFill", ink.clone()),
        ("quadrant2TextFill", ink.clone()),
        ("quadrant3TextFill", ink.clone()),
        ("quadrant4TextFill", ink.clone()),
        ("quadrantPointFill", blue.hex()),
        ("quadrantPointTextFill", ink.clone()),
        ("quadrantXAxisTextFill", ink.clone()),
        ("quadrantYAxisTextFill", ink.clone()),
        ("quadrantTitleFill", ink.clone()),
        ("quadrantInternalBorderStrokeFill", tint(fg, 0.30)),
        ("quadrantExternalBorderStrokeFill", line.clone()),
        // Requirement and architecture diagrams.
        ("requirementBackground", node.clone()),
        ("requirementBorderColor", border.clone()),
        ("requirementTextColor", ink.clone()),
        ("relationColor", line.clone()),
        ("relationLabelBackground", paper.clone()),
        ("relationLabelColor", ink.clone()),
        ("archEdgeColor", line.clone()),
        ("archEdgeArrowColor", line.clone()),
        ("archGroupBorderColor", tint(fg, 0.30)),
    ];

    let mut vars: Map<String, Value> = colors
        .into_iter()
        .map(|(name, color)| (name.to_string(), Value::String(color)))
        .collect();
    vars.insert("darkMode".to_string(), Value::Bool(dark));
    vars.insert("fontFamily".to_string(), json!(crate::fonts::FAMILY));

    // Categorical colors follow the terminal's ANSI palette.
    let scale = [blue, magenta, green, yellow, cyan, red];
    for (i, color) in scale.iter().cycle().take(12).enumerate() {
        let strength = if i < 6 { 1.0 } else { 0.6 };
        vars.insert(format!("cScale{i}"), json!(tint(*color, 0.35 * strength)));
        vars.insert(format!("cScaleLabel{i}"), json!(ink));
        vars.insert(format!("pie{}", i + 1), json!(tint(*color, 0.75 * strength)));
    }
    for (i, color) in scale.iter().cycle().take(8).enumerate() {
        vars.insert(format!("git{i}"), json!(tint(*color, 0.80)));
        vars.insert(format!("gitBranchLabel{i}"), json!(paper));
        vars.insert(format!("fillType{i}"), json!(tint(*color, 0.30)));
    }
    let plot_palette = scale.iter().map(|c| c.hex()).collect::<Vec<_>>().join(", ");
    vars.insert(
        "xyChart".to_string(),
        json!({
            "backgroundColor": paper,
            "titleColor": ink,
            "xAxisLabelColor": ink,
            "xAxisTitleColor": ink,
            "xAxisTickColor": line,
            "xAxisLineColor": line,
            "yAxisLabelColor": ink,
            "yAxisTitleColor": ink,
            "yAxisTickColor": line,
            "yAxisLineColor": line,
            "plotColorPalette": plot_palette,
        }),
    );
    vars.insert(
        "radar".to_string(),
        json!({ "axisColor": line, "graticuleColor": tint(fg, 0.25) }),
    );
    // Packet diagrams take their colors from the diagram configuration, not theme variables.
    let packet = json!({
        "startByteColor": ink,
        "endByteColor": ink,
        "labelColor": ink,
        "titleColor": ink,
        "blockStrokeColor": border,
        "blockFillColor": node,
    });

    Some(json!({ "theme": "base", "themeVariables": vars, "packet": packet }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_terminal_color_reports() {
        assert_eq!(Rgb::parse_x11("rgb:1e1e/1e1e/2e2e"), Some(Rgb(0x1e, 0x1e, 0x2e)));
        assert_eq!(Rgb::parse_x11("rgb:ff/80/00"), Some(Rgb(255, 128, 0)));
        assert_eq!(Rgb::parse_x11("rgb:f/0/f"), Some(Rgb(255, 0, 255)));
        assert_eq!(Rgb::parse_x11("#0a0b0c"), Some(Rgb(10, 11, 12)));
        assert_eq!(Rgb::parse_x11("#fff"), Some(Rgb(255, 255, 255)));
        assert_eq!(Rgb::parse_x11("rgb:1/2"), None);
        assert_eq!(Rgb::parse_x11("rgb:1/2/3/4"), None);
        assert_eq!(Rgb::parse_x11("cornflowerblue"), None);
    }

    #[test]
    fn mixing_and_luminance() {
        let black = Rgb(0, 0, 0);
        let white = Rgb(255, 255, 255);
        assert_eq!(black.mix(white, 0.5), Rgb(128, 128, 128));
        assert_eq!(black.mix(white, 0.0), black);
        assert_eq!(black.mix(white, 1.0), white);
        assert!(black.is_dark() && !white.is_dark());
        assert!(Rgb(0x1e, 0x1e, 0x2e).is_dark());
        assert!(!Rgb(0xef, 0xf1, 0xf5).is_dark());
        assert_eq!(Rgb(1, 171, 255).hex(), "#01abff");
    }

    #[test]
    fn palette_records_replies_by_index() {
        let mut palette = Palette::default();
        palette.set(10, Rgb(1, 1, 1));
        palette.set(11, Rgb(2, 2, 2));
        palette.set(4, Rgb(3, 3, 3));
        palette.set(7, Rgb(4, 4, 4));
        assert_eq!(palette.fg, Some(Rgb(1, 1, 1)));
        assert_eq!(palette.bg, Some(Rgb(2, 2, 2)));
        assert_eq!(palette.ansi, [None, None, None, Some(Rgb(3, 3, 3)), None, None]);
    }

    #[test]
    fn terminal_theme_needs_fg_and_bg() {
        let palette = Palette {
            bg: Some(Rgb(0, 0, 0)),
            ..Palette::default()
        };
        assert_eq!(terminal_theme(&palette), None);
    }

    #[test]
    fn terminal_theme_follows_the_terminal() {
        let mut dark = Palette {
            fg: Some(Rgb(0xcd, 0xd6, 0xf4)),
            bg: Some(Rgb(0x1e, 0x1e, 0x2e)),
            ..Palette::default()
        };
        dark.set(4, Rgb(0x89, 0xb4, 0xfa));
        let config = terminal_theme(&dark).unwrap();
        let vars = &config["themeVariables"];
        assert_eq!(config["theme"], "base");
        assert_eq!(vars["darkMode"], true);
        assert_eq!(vars["primaryTextColor"], "#cdd6f4");
        assert_eq!(vars["edgeLabelBackground"], "#1e1e2e");
        let node = Rgb(0x1e, 0x1e, 0x2e).mix(Rgb(0x89, 0xb4, 0xfa), 0.2).hex();
        assert_eq!(vars["primaryColor"], node);
        assert_eq!(vars["mainBkg"], node);
        assert_eq!(vars["stateBkg"], node);
        assert!(vars["cScale11"].is_string() && vars["pie12"].is_string());
        assert!(vars["git7"].is_string() && vars["fillType7"].is_string());
        assert!(vars["xyChart"]["plotColorPalette"].as_str().unwrap().starts_with("#89b4fa"));

        let light = Palette {
            fg: Some(Rgb(0x4c, 0x4f, 0x69)),
            bg: Some(Rgb(0xef, 0xf1, 0xf5)),
            ..Palette::default()
        };
        assert_eq!(terminal_theme(&light).unwrap()["themeVariables"]["darkMode"], false);
    }

    #[test]
    fn every_color_is_a_hex_string() {
        let palette = Palette {
            fg: Some(Rgb(0xcd, 0xd6, 0xf4)),
            bg: Some(Rgb(0x1e, 0x1e, 0x2e)),
            ..Palette::default()
        };
        let config = terminal_theme(&palette).unwrap();
        for (name, value) in config["themeVariables"].as_object().unwrap() {
            match value {
                Value::String(s) if name != "fontFamily" => {
                    assert!(s.starts_with('#') && s.len() == 7, "{name} = {s}");
                }
                Value::String(_) | Value::Bool(_) | Value::Object(_) => {}
                other => panic!("{name} = {other}"),
            }
        }
    }
}
