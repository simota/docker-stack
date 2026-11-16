//! The app's own files under `$XDG_CONFIG_HOME/docker-stack/` (`config.toml`):
//! atomic writes, and keeping a file that does not parse.

use std::path::{Path, PathBuf};

/// Writes `text` to `path` atomically (temp file + rename), creating the directory.
pub fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, path)
}

/// Where [`set_aside`] moves `path`: `config.toml` → `config.toml.invalid`.
pub fn invalid_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_owned();
    name.push(".invalid");
    path.with_file_name(name)
}

/// Moves a file that does not parse out of the way, so the defaults the app falls
/// back to are not saved over a hand edit with a typo. Replaces an older copy.
pub fn set_aside(path: &Path, error: &dyn std::fmt::Display) {
    let to = invalid_path(path);
    match std::fs::rename(path, &to) {
        Ok(()) => tracing::warn!(
            path = %path.display(), kept = %to.display(), %error,
            "file does not parse; starting from defaults and keeping it"
        ),
        Err(e) => tracing::warn!(
            path = %path.display(), %error, rename_error = %e,
            "file does not parse; starting from defaults (could not keep it)"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_aside_keeps_the_bad_file_next_to_where_it_was() {
        let dir = std::env::temp_dir().join(format!("ds-files-{}", std::process::id()));
        let file = dir.join("config.toml");
        write_atomic(&file, "theme = = \"dark\"\n").unwrap();
        assert_eq!(invalid_path(&file), dir.join("config.toml.invalid"));

        set_aside(&file, &"expected a value");
        assert!(!file.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml.invalid")).unwrap(),
            "theme = = \"dark\"\n"
        );
        // A later bad file replaces the copy.
        write_atomic(&file, "x = = 2\n").unwrap();
        set_aside(&file, &"again");
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml.invalid")).unwrap(),
            "x = = 2\n"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
