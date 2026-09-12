//! Settings from the command line and the configuration file.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::Value;

use crate::cli::{Cli, FitArg, Protocol};
use crate::display;

/// Settings in effect: flags override the configuration file, which overrides the defaults.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub theme: Option<String>,
    pub background: Option<String>,
    pub scale: f32,
    pub fit: FitArg,
    pub protocol: Protocol,
    /// Mermaid configuration: the file's `[mermaid]` table with `-c` merged over it.
    pub mermaid: Option<Value>,
}

/// The configuration file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    theme: Option<String>,
    background: Option<String>,
    scale: Option<f32>,
    fit: Option<FitArg>,
    protocol: Option<Protocol>,
    mermaid: Option<toml::Table>,
}

impl Settings {
    pub fn load(cli: &Cli) -> Result<Settings> {
        let file = match config_path() {
            Some((path, _)) if path.exists() => read_file(&path)?,
            Some((path, true)) => bail!("cannot read {}: no such file", path.display()),
            _ => File::default(),
        };
        let json = match &cli.config {
            Some(path) => Some(read_json(path)?),
            None => None,
        };
        Settings::resolve(cli, file, json)
    }

    fn resolve(cli: &Cli, file: File, json: Option<Value>) -> Result<Settings> {
        let scale = cli.scale.or(file.scale).unwrap_or(1.0);
        if !(scale.is_finite() && scale > 0.0) {
            bail!("scale must be a positive number, not {scale}");
        }
        let mut mermaid = file
            .mermaid
            .map(serde_json::to_value)
            .transpose()
            .context("the [mermaid] table cannot be used as Mermaid configuration")?;
        if let Some(json) = json {
            match &mut mermaid {
                Some(base) => display::merge(base, json),
                None => mermaid = Some(json),
            }
        }
        Ok(Settings {
            theme: cli.theme.clone().or(file.theme),
            background: cli.background.clone().or(file.background),
            scale,
            fit: cli.fit.or(file.fit).unwrap_or(FitArg::Width),
            protocol: cli.protocol.or(file.protocol).unwrap_or(Protocol::Auto),
            mermaid,
        })
    }
}

/// The configuration file's path, and whether it was given explicitly with `MER_CONFIG`.
/// Otherwise it is `$XDG_CONFIG_HOME/mer/config.toml` or `~/.config/mer/config.toml`.
pub fn config_path() -> Option<(PathBuf, bool)> {
    if let Some(path) = std::env::var_os("MER_CONFIG") {
        return Some((PathBuf::from(path), true));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
    Some((base.join("mer").join("config.toml"), false))
}

fn read_file(path: &Path) -> Result<File> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("invalid configuration in {}", path.display()))
}

fn read_json(path: &Path) -> Result<Value> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    use serde_json::json;

    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("mer").chain(args.iter().copied())).unwrap()
    }

    fn file(toml: &str) -> File {
        toml::from_str(toml).unwrap()
    }

    #[test]
    fn defaults_without_flags_or_file() {
        let settings = Settings::resolve(&cli(&[]), File::default(), None).unwrap();
        assert_eq!(settings.scale, 1.0);
        assert_eq!(settings.fit, FitArg::Width);
        assert_eq!(settings.protocol, Protocol::Auto);
        assert_eq!(settings.theme, None);
        assert_eq!(settings.mermaid, None);
    }

    #[test]
    fn flags_override_the_file() {
        let config = file("theme = \"dark\"\nscale = 1.5\nfit = \"contain\"\nprotocol = \"text\"\n");
        let from_file = Settings::resolve(&cli(&[]), config, None).unwrap();
        assert_eq!(from_file.theme.as_deref(), Some("dark"));
        assert_eq!(from_file.scale, 1.5);
        assert_eq!(from_file.fit, FitArg::Contain);
        assert_eq!(from_file.protocol, Protocol::Text);

        let config = file("theme = \"dark\"\nscale = 1.5\n");
        let flags = Settings::resolve(&cli(&["-t", "forest", "-s", "2"]), config, None).unwrap();
        assert_eq!(flags.theme.as_deref(), Some("forest"));
        assert_eq!(flags.scale, 2.0);
    }

    #[test]
    fn mermaid_configuration_merges_file_then_flag() {
        let config = file("[mermaid]\ntheme = \"base\"\nflowchart.curve = \"basis\"\n");
        let json = json!({ "flowchart": { "curve": "linear" }, "fontSize": 18 });
        let settings = Settings::resolve(&cli(&[]), config, Some(json)).unwrap();
        assert_eq!(
            settings.mermaid,
            Some(json!({ "theme": "base", "flowchart": { "curve": "linear" }, "fontSize": 18 }))
        );
    }

    #[test]
    fn invalid_settings_are_rejected() {
        assert!(toml::from_str::<File>("colour = \"red\"\n").is_err());
        assert!(toml::from_str::<File>("fit = \"stretch\"\n").is_err());
        assert!(Settings::resolve(&cli(&[]), file("scale = -1.0\n"), None).is_err());
    }
}
