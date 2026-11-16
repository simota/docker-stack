//! UI settings at `$XDG_CONFIG_HOME/docker-stack/config.toml`. Docker's own
//! configuration is never written. Unknown keys are ignored.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ThemePref {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LanguagePref {
    /// Follow the OS language (Japanese if it starts with `ja`, else English).
    #[default]
    System,
    En,
    Ja,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Docker context name to connect to; `None` follows the CLI's default.
    pub context: Option<String>,
    /// Log lines kept per open log view.
    pub log_lines: u32,
    pub theme: ThemePref,
    pub show_stopped: bool,
    pub language: LanguagePref,
    /// Show a tray / menu bar icon and keep running when the window is closed.
    pub tray: bool,
    /// Draw control borders at 3:1 against the background (WCAG 1.4.11) instead of
    /// the platform's faint ones.
    pub increase_contrast: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            context: None,
            log_lines: 10_000,
            theme: ThemePref::System,
            show_stopped: true,
            language: LanguagePref::System,
            tray: false,
            increase_contrast: false,
        }
    }
}

pub const LOG_LINES_MIN: u32 = 100;
pub const LOG_LINES_MAX: u32 = 1_000_000;

pub fn resolve_path(xdg_config_home: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    let base = xdg_config_home
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .or_else(|| {
            home.filter(|p| !p.as_os_str().is_empty())
                .map(|h| h.join(".config"))
        })?;
    Some(base.join("docker-stack/config.toml"))
}

fn path() -> Option<PathBuf> {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    let home = std::env::var_os("HOME").map(PathBuf::from);
    resolve_path(xdg.as_deref(), home.as_deref())
}

impl Config {
    /// A missing or unreadable file yields defaults.
    pub fn load() -> Self {
        path().map(|p| Self::load_from(&p)).unwrap_or_default()
    }

    /// A file that does not parse is moved to `config.toml.invalid` (see
    /// [`crate::files::set_aside`]), so saving a setting does not overwrite it.
    pub fn load_from(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => Self::try_parse(&s).unwrap_or_else(|e| {
                crate::files::set_aside(path, &e);
                Self::default()
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "failed to read config.toml; using defaults");
                Self::default()
            }
        }
    }

    #[cfg(test)]
    pub fn parse(s: &str) -> Self {
        Self::try_parse(s).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "invalid config.toml; falling back to defaults");
            Self::default()
        })
    }

    fn try_parse(s: &str) -> Result<Self, toml::de::Error> {
        toml::from_str::<Config>(s).map(Self::normalized)
    }

    pub fn normalized(mut self) -> Self {
        self.log_lines = self.log_lines.clamp(LOG_LINES_MIN, LOG_LINES_MAX);
        self
    }

    /// Writes atomically (temp file + rename).
    pub fn save(&self) -> std::io::Result<()> {
        let Some(p) = path() else { return Ok(()) };
        self.save_to(&p)
    }

    pub fn save_to(&self, path: &Path) -> std::io::Result<()> {
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::files::write_atomic(path, &text)
    }

    /// The configured context if it still exists and is supported, else what the docker CLI would use.
    pub fn initial_target(&self) -> engine::ConnectTarget {
        self.context
            .as_deref()
            .and_then(engine::find_context)
            .filter(|c| c.supported)
            .map(|c| engine::ConnectTarget::from(&c))
            .unwrap_or_else(engine::resolve_default_target)
    }
}

/// Concurrent-safe UI config holder: `mem` is held only for in-memory reads/mutations
/// while `io` serializes atomic disk writes off the async/UI thread.
pub struct ConfigStore {
    mem: std::sync::Mutex<Config>,
    io: tokio::sync::Mutex<()>,
}

impl ConfigStore {
    pub fn new(initial: Config) -> Self {
        Self {
            mem: std::sync::Mutex::new(initial),
            io: tokio::sync::Mutex::new(()),
        }
    }

    pub fn get(&self) -> Config {
        self.mem.lock().expect("config lock").clone()
    }

    pub fn log_lines(&self) -> u32 {
        self.mem.lock().expect("config lock").log_lines
    }

    pub async fn update_with_save<F>(
        &self,
        mutate: impl FnOnce(&mut Config),
        save_fn: F,
    ) -> std::io::Result<Config>
    where
        F: FnOnce(Config) -> std::io::Result<()> + Send + 'static,
    {
        let _io_guard = self.io.lock().await;
        let (prev, next) = {
            let mut c = self.mem.lock().expect("config lock");
            let prev = c.clone();
            mutate(&mut c);
            (prev, c.clone())
        };
        let to_write = next.clone();
        match tokio::task::spawn_blocking(move || save_fn(to_write)).await {
            Ok(Ok(())) => Ok(next),
            Ok(Err(e)) => {
                *self.mem.lock().expect("config lock") = prev;
                Err(e)
            }
            Err(e) => {
                *self.mem.lock().expect("config lock") = prev;
                Err(std::io::Error::other(e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_defaults() {
        let c = Config {
            context: Some("remote".into()),
            theme: ThemePref::Dark,
            ..Default::default()
        };
        assert_eq!(Config::parse(&toml::to_string_pretty(&c).unwrap()), c);
        assert_eq!(Config::parse(""), Config::default());
    }

    #[test]
    fn ignores_unknown_keys_clamps_and_logs_warning_on_corrupt_toml() {
        // A-008 / R-05
        let c = Config::parse("future_key = 3\nlog_lines = 5\ntheme = \"light\"\n");
        assert_eq!(c.log_lines, LOG_LINES_MIN);
        assert_eq!(c.theme, ThemePref::Light);
        // A file written before a setting existed keeps the setting's default.
        assert!(!c.increase_contrast);

        let (parsed, events) = crate::logging::capture(|| Config::parse("not = = toml"));
        assert_eq!(parsed, Config::default());
        assert!(
            events
                .iter()
                .any(|(lvl, line)| *lvl == tracing::Level::WARN
                    && line.contains("invalid config.toml")),
            "{events:?}"
        );
    }

    #[test]
    fn save_to_and_load_from_roundtrip_and_xdg_path_precedence() {
        // A-009 / R-07
        assert_eq!(
            resolve_path(Some(Path::new("/xdg")), Some(Path::new("/home/u"))),
            Some(PathBuf::from("/xdg/docker-stack/config.toml"))
        );
        assert_eq!(
            resolve_path(None, Some(Path::new("/home/u"))),
            Some(PathBuf::from("/home/u/.config/docker-stack/config.toml"))
        );

        let tmp = std::env::temp_dir().join(format!("ds-cfg-{}", std::process::id()));
        let file = tmp.join("docker-stack/config.toml");
        let saved = Config {
            context: Some("ctx-a".into()),
            log_lines: 25_000,
            theme: ThemePref::Dark,
            show_stopped: false,
            language: LanguagePref::Ja,
            tray: true,
            increase_contrast: true,
        };
        saved.save_to(&file).unwrap();
        assert_eq!(Config::load_from(&file), saved);
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn unknown_context_falls_back_to_cli_default() {
        let c = Config {
            context: Some("no-such-context-xyz".into()),
            ..Default::default()
        };
        assert_eq!(c.initial_target(), engine::resolve_default_target());
    }

    #[test]
    fn a_config_that_does_not_parse_is_kept_and_not_saved_over() {
        let dir = std::env::temp_dir().join(format!("ds-config-bad-{}", std::process::id()));
        let file = dir.join("config.toml");
        crate::files::write_atomic(&file, "theme = \"dark\"\nlog_lines = = 5\n").unwrap();
        assert_eq!(Config::load_from(&file), Config::default());
        Config::default().save_to(&file).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml.invalid")).unwrap(),
            "theme = \"dark\"\nlog_lines = = 5\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
