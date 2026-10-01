//! Which Whisper model file Offline Captions load.
//!
//! LCRT is installed with a multilingual model, so Offline Captions work
//! without any setup. A custom model is optional and chosen in Settings.

use std::{
    fs,
    path::{Path, PathBuf},
};

/// File name of the built-in model: Whisper base, multilingual.
const BUILT_IN_FILE: &str = "ggml-base.bin";
/// The built-in model's exact size. Comparing it catches a truncated or
/// replaced file without hashing 148 MB on every Start; the package build
/// verifies the pinned SHA-256 (see `packaging/models.json`).
const BUILT_IN_BYTES: u64 = 147_951_465;

/// Where the package installs the built-in model, found from the executable
/// so that an unpacked or relocated installation works too:
/// `<prefix>/bin/lcrt` uses `<prefix>/share/lcrt/models/ggml-base.bin`.
pub(crate) fn built_in_model(executable: &Path) -> Option<PathBuf> {
    Some(
        executable
            .parent()?
            .parent()?
            .join("share/lcrt/models")
            .join(BUILT_IN_FILE),
    )
}

/// Why Offline Captions can't load a model.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ModelProblem {
    BuiltInMissing,
    BuiltInDamaged,
    CustomMissing,
    RunOverrideMissing,
}

impl ModelProblem {
    /// What to tell the user.
    pub(crate) fn message(&self) -> &'static str {
        match self {
            Self::BuiltInMissing => {
                "LCRT's built-in speech model is missing. Reinstall LCRT to use Offline Captions."
            }
            Self::BuiltInDamaged => {
                "LCRT's built-in speech model is damaged. Reinstall LCRT to use Offline Captions."
            }
            Self::CustomMissing => {
                "The custom speech model can't be found. Choose it again in Settings, or turn off \
                 the custom model to use the built-in one."
            }
            Self::RunOverrideMissing => {
                "The model given by --model or LCRT_MODEL_PATH can't be found."
            }
        }
    }

    /// Whether Settings can fix it.
    pub(crate) fn needs_settings(&self) -> bool {
        matches!(self, Self::CustomMissing)
    }
}

/// The model file to load: a model given for this run, else the user's
/// custom model, else the built-in one.
pub(crate) fn offline_model(
    run_override: Option<&Path>,
    custom: Option<&Path>,
    built_in: Option<&Path>,
) -> Result<PathBuf, ModelProblem> {
    if let Some(path) = run_override {
        return path
            .is_file()
            .then(|| path.to_owned())
            .ok_or(ModelProblem::RunOverrideMissing);
    }
    if let Some(path) = custom {
        return path
            .is_file()
            .then(|| path.to_owned())
            .ok_or(ModelProblem::CustomMissing);
    }
    let path = built_in.ok_or(ModelProblem::BuiltInMissing)?;
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() && metadata.len() == BUILT_IN_BYTES => {
            Ok(path.to_owned())
        }
        Ok(metadata) if metadata.is_file() => Err(ModelProblem::BuiltInDamaged),
        _ => Err(ModelProblem::BuiltInMissing),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use super::{BUILT_IN_BYTES, BUILT_IN_FILE, ModelProblem, built_in_model, offline_model};

    fn scratch(name: &str) -> PathBuf {
        let directory =
            std::env::temp_dir().join(format!("lcrt-models-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&directory);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    #[test]
    fn the_built_in_model_is_found_beside_the_installed_executable() {
        assert_eq!(
            built_in_model(Path::new("/usr/bin/lcrt")),
            Some(PathBuf::from("/usr/share/lcrt/models/ggml-base.bin"))
        );
        assert_eq!(
            built_in_model(Path::new("/opt/lcrt/bin/lcrt")),
            Some(PathBuf::from("/opt/lcrt/share/lcrt/models/ggml-base.bin"))
        );
        assert_eq!(built_in_model(Path::new("lcrt")), None);
    }

    #[test]
    fn the_built_in_model_is_used_unless_another_is_chosen() {
        let directory = scratch("choice");
        let built_in = directory.join(BUILT_IN_FILE);
        fs::File::create(&built_in)
            .unwrap()
            .set_len(BUILT_IN_BYTES)
            .unwrap();
        let custom = directory.join("custom.bin");
        fs::write(&custom, b"model").unwrap();

        assert_eq!(
            offline_model(None, None, Some(&built_in)),
            Ok(built_in.clone())
        );
        assert_eq!(
            offline_model(None, Some(&custom), Some(&built_in)),
            Ok(custom.clone())
        );
        // A model given for this run wins over both.
        assert_eq!(
            offline_model(Some(&custom), None, Some(&built_in)),
            Ok(custom.clone())
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn a_missing_or_damaged_model_is_reported_not_replaced() {
        let directory = scratch("problems");
        let built_in = directory.join(BUILT_IN_FILE);
        let missing = directory.join("missing.bin");
        assert_eq!(
            offline_model(None, None, Some(&built_in)),
            Err(ModelProblem::BuiltInMissing)
        );
        assert_eq!(
            offline_model(None, None, None),
            Err(ModelProblem::BuiltInMissing)
        );
        // A truncated download or a different file is not the built-in model.
        fs::write(&built_in, b"not a model").unwrap();
        assert_eq!(
            offline_model(None, None, Some(&built_in)),
            Err(ModelProblem::BuiltInDamaged)
        );
        // A missing custom model does not silently fall back to another one.
        let custom = offline_model(None, Some(&missing), Some(&built_in));
        assert_eq!(custom, Err(ModelProblem::CustomMissing));
        assert!(ModelProblem::CustomMissing.needs_settings());
        assert!(!ModelProblem::BuiltInMissing.needs_settings());
        assert_eq!(
            offline_model(Some(&missing), None, Some(&built_in)),
            Err(ModelProblem::RunOverrideMissing)
        );
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn the_built_in_model_matches_the_packaging_manifest() {
        let manifest: serde_json::Value =
            serde_json::from_str(include_str!("../../../packaging/models.json")).unwrap();
        let whisper = manifest["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == "whisper-base-multilingual")
            .unwrap();
        assert_eq!(whisper["file"], BUILT_IN_FILE);
        assert_eq!(whisper["size"], BUILT_IN_BYTES);
        assert_eq!(
            whisper["destination"],
            format!("usr/share/lcrt/models/{BUILT_IN_FILE}")
        );
    }
}
