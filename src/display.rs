//! Drawing diagrams in the terminal: capability checks, theme, and one-shot inline output.

use std::io::{self, IsTerminal, Write};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cli::Protocol;
use crate::diag::{self, NO_DIAGRAM};
use crate::engine::{Control, Engine, Failure};
use crate::fonts;
use crate::input::Diagram;
use crate::render::{self, Renderer};
use crate::settings::{self, Settings};
use crate::size::{self, Fit, Grid};
use crate::term::probe::{self, Caps};
use crate::term::tty::Tty;
use crate::term::{self, kitty};
use crate::theme::{self, Palette, Rgb};

/// Everything needed to draw diagrams in this terminal.
#[derive(Clone)]
pub struct Setup {
    pub tty: Option<Tty>,
    pub caps: Caps,
    pub grid: Grid,
    pub fit: Fit,
    pub scale: f32,
    pub theme: String,
    pub user_config: Option<Value>,
    pub config: Value,
    pub background: Option<Rgb>,
    /// Diagrams are drawn with box-drawing characters instead of images.
    pub text: bool,
    /// Whether diagnostics on stderr are colored.
    pub color: bool,
}

impl Setup {
    /// Probes the terminal and settles size, theme and background.
    pub fn new(settings: &Settings) -> Result<Setup> {
        let theme = settings.theme.clone().unwrap_or_else(|| "terminal".to_string());
        let (tty, caps, text) = match settings.protocol {
            Protocol::Text => {
                let tty = Tty::open().ok();
                let cols = tty
                    .as_ref()
                    .and_then(|tty| tty.winsize().ok())
                    .map_or(0, |size| size.ws_col);
                let caps = Caps {
                    cols,
                    ..Caps::default()
                };
                (tty, caps, true)
            }
            Protocol::Kitty | Protocol::Auto => {
                let forced = settings.protocol == Protocol::Kitty;
                if !forced && !io::stdout().is_terminal() {
                    bail!(
                        "stdout is not a terminal; write an image with -o FILE, draw text with \
                         --protocol text, or force graphics with --protocol kitty"
                    );
                }
                let (tty, caps) = match Tty::open() {
                    Ok(tty) => {
                        let caps = probe::probe(&tty, probe_timeout())?;
                        (Some(tty), caps)
                    }
                    Err(_) if forced => (None, Caps::default()),
                    Err(err) => return Err(err).context("cannot open the terminal (/dev/tty)"),
                };
                let text = !forced && !caps.kitty_graphics;
                if text {
                    let hint = if term::inside_tmux() {
                        " (inside tmux, add `set -g allow-passthrough on` to tmux.conf)"
                    } else {
                        ""
                    };
                    eprintln!(
                        "mer: this terminal doesn't support the kitty graphics protocol{hint}, \
                         so diagrams are drawn as text"
                    );
                }
                (tty, caps, text)
            }
        };
        let (cell_w, cell_h) = caps.cell.unwrap_or((10, 20));
        let grid = Grid {
            cols: if caps.cols == 0 { 80 } else { caps.cols },
            rows: if caps.rows == 0 { 24 } else { caps.rows },
            cell_w,
            cell_h,
        };
        let user_config = settings.mermaid.clone();
        let config = site_config(&theme, Some(&caps), user_config.as_ref());
        let background = background(settings.background.as_deref(), &theme, Some(&caps.palette))?;
        Ok(Setup {
            tty,
            caps,
            grid,
            fit: settings.fit,
            scale: settings.scale,
            theme,
            user_config,
            config,
            background,
            text,
            color: io::stderr().is_terminal(),
        })
    }

    /// The configuration after the terminal reported new colors.
    pub fn config_for(&self, palette: &Palette) -> Value {
        let caps = Caps {
            palette: palette.clone(),
            ..self.caps.clone()
        };
        site_config(&self.theme, Some(&caps), self.user_config.as_ref())
    }
}

/// Draws each diagram inline, in order, and leaves it in the scrollback.
pub fn show(setup: &Setup, diagrams: &[Diagram]) -> Result<ExitCode> {
    if diagrams.is_empty() {
        return Ok(no_diagrams());
    }
    if setup.text {
        return show_text(setup, diagrams);
    }
    let mut renderer = Renderer::new(setup.config.clone(), setup.background);
    let mut stdout = io::stdout().lock();
    let mut failed = false;
    for diagram in diagrams {
        match renderer.frame(&diagram.text, &setup.grid, setup.scale, setup.fit, Control::new()) {
            Ok(frame) => {
                let mut out = b"\x1b[?2026h".to_vec();
                if let Some(caption) = &diagram.caption {
                    writeln!(out, "\x1b[2m{caption}\x1b[22m")?;
                }
                render::write_inline(&mut out, &frame, kitty::random_id())?;
                out.extend_from_slice(b"\x1b[?2026l");
                stdout.write_all(&term::for_terminal(&out))?;
                stdout.flush()?;
            }
            Err(failure) => {
                stdout.flush()?;
                report(&failure, diagram, setup.color);
                failed = true;
            }
        }
    }
    Ok(if failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

fn show_text(setup: &Setup, diagrams: &[Diagram]) -> Result<ExitCode> {
    let engine = Engine::new(setup.config.clone(), None);
    let terminal = io::stdout().is_terminal();
    // A terminal wraps lines wider than itself, garbling the drawing; pipes and files don't.
    let max_width = (terminal && setup.caps.cols > 0).then_some(usize::from(setup.caps.cols));
    let mut stdout = io::stdout().lock();
    let mut failed = false;
    for diagram in diagrams {
        match engine.render_text(&diagram.text, max_width) {
            Ok(text) => {
                match (&diagram.caption, terminal) {
                    (Some(caption), true) => writeln!(stdout, "\x1b[2m{caption}\x1b[22m")?,
                    (Some(caption), false) => writeln!(stdout, "{caption}")?,
                    (None, _) => {}
                }
                stdout.write_all(text.as_bytes())?;
                if !text.ends_with('\n') {
                    writeln!(stdout)?;
                }
                stdout.flush()?;
            }
            Err(failure) => {
                stdout.flush()?;
                report(&failure, diagram, setup.color);
                failed = true;
            }
        }
    }
    Ok(if failed { ExitCode::from(1) } else { ExitCode::SUCCESS })
}

pub fn doctor() -> Result<ExitCode> {
    let tty = Tty::open().context("cannot open the terminal (/dev/tty)")?;
    let caps = probe::probe(&tty, probe_timeout())?;
    let color = |rgb: Option<Rgb>| rgb.map_or_else(|| "not reported".to_string(), Rgb::hex);
    let yes = |value: bool| if value { "yes" } else { "no" };
    let program = std::env::var("TERM_PROGRAM").unwrap_or_else(|_| "unknown".to_string());
    let version = std::env::var("TERM_PROGRAM_VERSION").unwrap_or_default();
    println!("terminal          {program} {version}");
    println!("inside tmux       {}", yes(term::inside_tmux()));
    println!("responded         {}", yes(caps.responded));
    println!("kitty graphics    {}", yes(caps.kitty_graphics));
    println!("grid              {} × {} cells", caps.cols, caps.rows);
    match caps.cell {
        Some((w, h)) => println!(
            "cell size         {w} × {h} px (text-matched scale {:.2})",
            size::text_matched_scale(h)
        ),
        None => println!("cell size         not reported"),
    }
    println!("foreground        {}", color(caps.palette.fg));
    println!("background        {}", color(caps.palette.bg));
    let scheme = match caps.dark {
        Some(true) => "dark",
        Some(false) => "light",
        None => "not reported",
    };
    println!("color scheme      {scheme}");
    match settings::config_path() {
        Some((path, _)) if path.exists() => println!("config file       {}", path.display()),
        Some((path, _)) => println!("config file       {} (not found)", path.display()),
        None => println!("config file       none"),
    }
    Ok(ExitCode::SUCCESS)
}

pub fn probe_timeout() -> Duration {
    let millis = std::env::var("MER_PROBE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(500);
    Duration::from_millis(millis)
}

/// Mermaid site configuration: the theme, the embedded font, then the user's configuration.
pub fn site_config(theme: &str, caps: Option<&Caps>, user: Option<&Value>) -> Value {
    let mut config = match theme {
        "terminal" => caps
            .and_then(|caps| theme::terminal_theme(&caps.palette))
            .unwrap_or_else(|| {
                let dark = caps.and_then(|caps| caps.dark).unwrap_or(false);
                json!({ "theme": if dark { "dark" } else { "default" } })
            }),
        name => json!({ "theme": name }),
    };
    config["fontFamily"] = json!(fonts::FAMILY);
    if let Some(user) = user {
        merge(&mut config, user.clone());
    }
    config
}

/// Merges `overlay` into `base`, recursing into objects that both have.
pub fn merge(base: &mut Value, overlay: Value) {
    match (base, overlay) {
        (Value::Object(base), Value::Object(overlay)) => {
            for (key, value) in overlay {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// The frame background. The `terminal` theme draws over the terminal's own background;
/// Mermaid themes get the background they were designed for, unless `-b` says otherwise.
pub fn background(spec: Option<&str>, theme: &str, palette: Option<&Palette>) -> Result<Option<Rgb>> {
    let spec = match spec {
        Some(spec) => spec,
        None if theme == "terminal" => "transparent",
        None if theme.contains("dark") => "#333333",
        None => "white",
    };
    Ok(match spec {
        "transparent" | "none" => None,
        "terminal" => Some(
            palette
                .and_then(|palette| palette.bg)
                .context("the terminal did not report its background color")?,
        ),
        "white" => Some(Rgb(255, 255, 255)),
        "black" => Some(Rgb(0, 0, 0)),
        other => Some(Rgb::parse_x11(other).with_context(|| {
            format!("unknown background {other:?}; use transparent, terminal, or a #rrggbb color")
        })?),
    })
}

pub fn report(failure: &Failure, diagram: &Diagram, color: bool) {
    if let Some(diagnostic) = failure.diagnostic() {
        eprint!("{}", diag::format(&diagnostic, diagram, color));
    }
}

/// Says that the input holds no diagram, and exits 1.
pub fn no_diagrams() -> ExitCode {
    eprintln!("mer: {NO_DIAGRAM}");
    ExitCode::from(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_overlays_nested_objects() {
        let mut base = json!({ "theme": "base", "themeVariables": { "a": 1, "b": 2 } });
        merge(&mut base, json!({ "themeVariables": { "b": 3 }, "flowchart": { "curve": "basis" } }));
        assert_eq!(
            base,
            json!({ "theme": "base", "themeVariables": { "a": 1, "b": 3 }, "flowchart": { "curve": "basis" } })
        );
    }

    #[test]
    fn user_configuration_wins_over_the_theme() {
        let config = site_config("dark", None, Some(&json!({ "theme": "forest" })));
        assert_eq!(config["theme"], "forest");
        assert_eq!(config["fontFamily"], fonts::FAMILY);
    }

    #[test]
    fn terminal_theme_falls_back_to_the_reported_scheme() {
        let caps = Caps {
            dark: Some(true),
            ..Caps::default()
        };
        assert_eq!(site_config("terminal", Some(&caps), None)["theme"], "dark");
        assert_eq!(site_config("terminal", None, None)["theme"], "default");
    }

    #[test]
    fn backgrounds() {
        assert_eq!(background(None, "terminal", None).unwrap(), None);
        assert_eq!(background(None, "default", None).unwrap(), Some(Rgb(255, 255, 255)));
        assert_eq!(background(None, "neo-dark", None).unwrap(), Some(Rgb(0x33, 0x33, 0x33)));
        assert_eq!(background(Some("#010203"), "default", None).unwrap(), Some(Rgb(1, 2, 3)));
        assert!(background(Some("terminal"), "default", None).is_err());
        assert!(background(Some("mauve"), "default", None).is_err());
    }
}
