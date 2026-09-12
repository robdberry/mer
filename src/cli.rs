use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use serde::Deserialize;

use crate::size::Fit;

/// Render Mermaid diagrams as graphics in the terminal.
#[derive(Debug, Parser)]
#[command(name = "mer", version)]
pub struct Cli {
    /// Mermaid (.mmd, .mermaid) or Markdown files; "-" reads stdin, showing input that keeps
    /// arriving as it comes
    #[arg(value_name = "INPUT")]
    pub inputs: Vec<PathBuf>,

    /// Keep running and redraw whenever an input file changes
    #[arg(short = 'w', long)]
    pub watch: bool,

    /// Full-screen viewer: pan with the arrow keys or mouse, zoom with +/- or the scroll wheel
    #[arg(short = 'i', long)]
    pub interactive: bool,

    /// Parse and lay out every diagram without displaying; exit 1 on errors
    #[arg(long)]
    pub check: bool,

    /// Only the Nth mermaid block of each Markdown input, counting from 1
    #[arg(short = 'n', long = "diagram", value_name = "N")]
    pub diagram: Option<usize>,

    /// How to interpret stdin
    #[arg(long, value_enum, default_value_t = StdinFormat::Auto)]
    pub stdin_format: StdinFormat,

    /// Theme: terminal (match the terminal's colors), or a Mermaid theme such as default,
    /// dark, forest, neutral, base, neo, neo-dark
    #[arg(short = 't', long)]
    pub theme: Option<String>,

    /// Background: transparent, terminal, or a color such as white or #1e1e2e
    #[arg(short = 'b', long)]
    pub background: Option<String>,

    /// Mermaid configuration file (JSON)
    #[arg(short = 'c', long, value_name = "FILE")]
    pub config: Option<PathBuf>,

    /// Size relative to the terminal's text size (for -o, relative to 2x) [default: 1]
    #[arg(short = 's', long)]
    pub scale: Option<f32>,

    /// How diagrams are fitted to the terminal [default: width]
    #[arg(long, value_enum)]
    pub fit: Option<Fit>,

    /// Write PNG or SVG files instead of displaying ("-" for stdout)
    #[arg(short = 'o', long, value_name = "FILE")]
    pub output: Option<PathBuf>,

    /// Output format [default: from the -o extension]
    #[arg(long, value_enum)]
    pub format: Option<OutputFormat>,

    /// How diagrams are drawn [default: auto]
    #[arg(long, value_enum)]
    pub protocol: Option<Protocol>,

    /// Print the detected terminal capabilities and exit
    #[arg(long)]
    pub doctor: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum StdinFormat {
    Auto,
    Mermaid,
    Markdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    Png,
    Svg,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    /// Kitty graphics when the terminal supports them, text otherwise
    Auto,
    /// Kitty graphics, even when stdout is not a terminal
    Kitty,
    /// Box-drawing characters, for any terminal or pipe
    Text,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn stdin_dash_is_an_input() {
        let cli = Cli::try_parse_from(["mer", "-", "-t", "dark"]).unwrap();
        assert_eq!(cli.inputs, [PathBuf::from("-")]);
        assert_eq!(cli.theme.as_deref(), Some("dark"));
    }
}
