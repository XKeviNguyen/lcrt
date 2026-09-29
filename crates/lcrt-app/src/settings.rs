//! Non-secret preferences on disk: `$XDG_CONFIG_HOME/lcrt/preferences.json`.
//!
//! Writes are atomic (temporary file, fsync, rename) and readable only by the
//! user. A missing or corrupt file yields defaults; a corrupt file is kept
//! aside for inspection instead of being overwritten.

use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

use lcrt_core::Preferences;
use tracing::{info, warn};

const FILE_NAME: &str = "preferences.json";
const MAX_FILE_BYTES: u64 = 64 * 1024;

/// Location of the preferences file.
#[derive(Clone, Debug)]
pub(crate) struct SettingsStore {
    path: PathBuf,
}

impl SettingsStore {
    /// The store in the XDG config directory, if a home can be determined.
    pub(crate) fn from_environment() -> Option<Self> {
        let base = env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;
        Some(Self::at(base.join("lcrt")))
    }

    pub(crate) fn at(directory: PathBuf) -> Self {
        Self {
            path: directory.join(FILE_NAME),
        }
    }

    /// Loads preferences, falling back to defaults on any problem.
    pub(crate) fn load(&self) -> Preferences {
        match self.read() {
            Ok(Some(preferences)) => preferences.normalized(),
            Ok(None) => Preferences::default().normalized(),
            Err(error) => {
                warn!(%error, "preferences unreadable; using defaults");
                let aside = self.path.with_extension("json.corrupt");
                if fs::rename(&self.path, &aside).is_ok() {
                    info!("kept the unreadable preferences file aside");
                }
                Preferences::default().normalized()
            }
        }
    }

    fn read(&self) -> io::Result<Option<Preferences>> {
        let file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let mut text = String::new();
        file.take(MAX_FILE_BYTES + 1).read_to_string(&mut text)?;
        if text.len() as u64 > MAX_FILE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "preferences file too large",
            ));
        }
        serde_json::from_str(&text)
            .map(Some)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }

    /// Saves preferences atomically with owner-only permissions.
    pub(crate) fn save(&self, preferences: &Preferences) -> io::Result<()> {
        let directory = self.path.parent().unwrap_or(Path::new("."));
        fs::create_dir_all(directory)?;
        let text = serde_json::to_string_pretty(&preferences.clone().normalized())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let temporary = directory.join(format!(".{FILE_NAME}.{}.tmp", std::process::id()));
        let result = (|| {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &self.path)
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use lcrt_core::{Language, Preferences, ProcessingMode, Rgb};

    use super::SettingsStore;

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("lcrt-settings-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        directory
    }

    #[test]
    fn missing_file_gives_defaults() {
        let store = SettingsStore::at(scratch("missing"));
        assert_eq!(store.load(), Preferences::default().normalized());
    }

    #[test]
    fn preferences_round_trip_including_unicode_font_names_and_colors() {
        let directory = scratch("round-trip");
        let store = SettingsStore::at(directory.clone());
        let mut preferences = Preferences::default();
        preferences.general.default_mode = ProcessingMode::Translation;
        preferences.general.translation_target = Language::Vietnamese;
        preferences.general.model_path = Some(PathBuf::from("/models/ggml-base.bin"));
        preferences.appearance.font_family = Some("Noto Sans CJK JP".to_owned());
        preferences.appearance.text_color = Rgb::new(250, 240, 10);
        preferences.appearance.background_opacity = 0.0;
        preferences.vocabulary.explanation_language = Language::Japanese;
        store.save(&preferences).unwrap();
        assert_eq!(store.load(), preferences.normalized());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupt_file_yields_defaults_and_is_kept_aside() {
        let directory = scratch("corrupt");
        fs::create_dir_all(&directory).unwrap();
        fs::write(directory.join("preferences.json"), "{ not json").unwrap();
        let store = SettingsStore::at(directory.clone());
        assert_eq!(store.load(), Preferences::default().normalized());
        assert!(directory.join("preferences.json.corrupt").exists());
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn unknown_future_fields_are_ignored_and_values_clamped() {
        let directory = scratch("future");
        fs::create_dir_all(&directory).unwrap();
        fs::write(
            directory.join("preferences.json"),
            r#"{"version": 99, "future_setting": true,
                "appearance": {"font_size_points": 500, "width": -5, "new_field": 1}}"#,
        )
        .unwrap();
        let loaded = SettingsStore::at(directory.clone()).load();
        assert_eq!(loaded.appearance.font_size_points, 64.0);
        assert_eq!(loaded.appearance.width, 320);
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn saved_file_contains_no_credential_and_is_private() {
        let directory = scratch("private");
        let store = SettingsStore::at(directory.clone());
        store.save(&Preferences::default()).unwrap();
        let text = fs::read_to_string(directory.join("preferences.json")).unwrap();
        let lowered = text.to_lowercase();
        for forbidden in ["api_key", "apikey", "secret", "token", "password", "sk-"] {
            assert!(
                !lowered.contains(forbidden),
                "{forbidden} leaked into preferences"
            );
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(directory.join("preferences.json"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o077, 0);
        }
        fs::remove_dir_all(directory).unwrap();
    }
}
