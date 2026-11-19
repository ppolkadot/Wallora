use std::fs;
use std::path::{Path, PathBuf};

use directories::BaseDirs;
use thiserror::Error;

const APP_DIR_NAME: &str = "Wallora";

/// Resolved application directories under Windows `%APPDATA%` and `%LOCALAPPDATA%`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub local_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub data_dir: PathBuf,
    pub logs_dir: PathBuf,
}

#[derive(Debug, Error)]
pub enum PathError {
    #[error("unable to determine standard user directories")]
    MissingBaseDirs,
    #[error("failed to create application directory {path}: {source}")]
    CreateDir {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl AppPaths {
    /// Resolves standard directories using `directories::BaseDirs`:
    /// - Config: `%APPDATA%\Wallora`
    /// - Cache/Data/Logs: `%LOCALAPPDATA%\Wallora\{cache,data,logs}`
    pub fn resolve() -> Result<Self, PathError> {
        let base = BaseDirs::new().ok_or(PathError::MissingBaseDirs)?;
        Ok(Self::from_roots(
            base.config_dir().join(APP_DIR_NAME),
            base.data_local_dir().join(APP_DIR_NAME),
        ))
    }

    /// Constructs paths from explicit config and local roots (useful for tests).
    pub fn from_roots(config_dir: PathBuf, local_dir: PathBuf) -> Self {
        let cache_dir = local_dir.join("cache");
        let data_dir = local_dir.join("data");
        let logs_dir = local_dir.join("logs");
        Self {
            config_dir,
            local_dir,
            cache_dir,
            data_dir,
            logs_dir,
        }
    }

    /// Ensures all application directories exist on disk.
    pub fn ensure_dirs(&self) -> Result<(), PathError> {
        for dir in [
            &self.config_dir,
            &self.local_dir,
            &self.cache_dir,
            &self.data_dir,
            &self.logs_dir,
        ] {
            create_dir_all(dir)?;
        }
        Ok(())
    }

    /// SQLite database path under `%LOCALAPPDATA%\Wallora\data\wallora.db`.
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("wallora.db")
    }
}

fn create_dir_all(path: &Path) -> Result<(), PathError> {
    fs::create_dir_all(path).map_err(|source| PathError::CreateDir {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::AppPaths;
    use std::path::PathBuf;

    #[test]
    fn resolves_expected_subdirectories() {
        let paths = AppPaths::from_roots(
            PathBuf::from("C:/Users/Test/AppData/Roaming/Wallora"),
            PathBuf::from("C:/Users/Test/AppData/Local/Wallora"),
        );
        assert_eq!(
            paths.config_dir,
            PathBuf::from("C:/Users/Test/AppData/Roaming/Wallora")
        );
        assert_eq!(
            paths.cache_dir,
            PathBuf::from("C:/Users/Test/AppData/Local/Wallora/cache")
        );
        assert_eq!(
            paths.data_dir,
            PathBuf::from("C:/Users/Test/AppData/Local/Wallora/data")
        );
        assert_eq!(
            paths.logs_dir,
            PathBuf::from("C:/Users/Test/AppData/Local/Wallora/logs")
        );
    }

    #[test]
    fn resolve_succeeds_on_host() {
        let paths = AppPaths::resolve().expect("base dirs should resolve on Windows");
        assert!(paths.config_dir.ends_with("Wallora"));
        assert!(paths.logs_dir.ends_with("logs"));
    }
}
