use std::path::{Path, PathBuf};

#[cfg(target_os = "windows")]
const PORTABLE_MARKER: &str = "portable.dbx";
#[cfg(target_os = "windows")]
const INSTALLER_MARKER: &str = "uninstall.exe";
const DATA_DIR_OVERRIDE_FILE: &str = "data-dir";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DataDirMode {
    Default,
    EnvOverride,
    ConfiguredOverride,
    Portable { exe_dir: PathBuf },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DataDirResolution {
    pub data_dir: PathBuf,
    pub default_data_dir: PathBuf,
    pub mode: DataDirMode,
    portable_data_dir: Option<PathBuf>,
}

impl DataDirResolution {
    pub fn uses_custom_data_dir(&self) -> bool {
        matches!(self.mode, DataDirMode::EnvOverride | DataDirMode::ConfiguredOverride | DataDirMode::Portable { .. })
    }

    pub fn is_portable_mode(&self) -> bool {
        matches!(self.mode, DataDirMode::Portable { .. })
    }
}

pub fn resolve_data_dir_with_mode(default_app_data_dir: PathBuf, config_dir: Option<&Path>) -> DataDirResolution {
    let env_data_dir = std::env::var_os("DBX_DATA_DIR").filter(|value| !value.is_empty()).map(PathBuf::from);
    let configured_data_dir = config_dir.and_then(load_configured_data_dir);
    let default_data_dir = default_data_dir(default_app_data_dir);

    #[cfg(target_os = "windows")]
    let exe_dir = current_exe_dir();
    #[cfg(not(target_os = "windows"))]
    let exe_dir = None;

    let portable_marker_exists = exe_dir.as_deref().is_some_and(portable_marker_exists);
    let installer_marker_exists = exe_dir.as_deref().is_some_and(installer_marker_exists);

    resolve_data_dir_from_inputs(
        default_data_dir,
        exe_dir,
        portable_marker_exists,
        installer_marker_exists,
        env_data_dir,
        configured_data_dir,
    )
}

fn default_data_dir(fallback: PathBuf) -> PathBuf {
    home_dir_from_env().map(default_home_data_dir).unwrap_or(fallback)
}

fn home_dir_from_env() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("USERPROFILE")
            .filter(|value| !value.is_empty())
            .or_else(|| std::env::var_os("HOME").filter(|value| !value.is_empty()))
            .map(PathBuf::from)
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME").filter(|value| !value.is_empty()).map(PathBuf::from)
    }
}

pub fn default_home_data_dir(home_dir: PathBuf) -> PathBuf {
    home_dir.join(".drx")
}

pub fn alternative_data_dir(resolution: &DataDirResolution) -> Option<PathBuf> {
    match &resolution.mode {
        DataDirMode::Portable { .. } => Some(resolution.default_data_dir.clone()),
        DataDirMode::Default => resolution.portable_data_dir.clone(),
        DataDirMode::EnvOverride | DataDirMode::ConfiguredOverride => None,
    }
}

pub fn is_portable_mode() -> bool {
    resolve_data_dir_with_mode(PathBuf::new(), None).is_portable_mode()
}

pub fn data_dir_override_file(config_dir: &Path) -> PathBuf {
    config_dir.join(DATA_DIR_OVERRIDE_FILE)
}

pub fn load_configured_data_dir(config_dir: &Path) -> Option<PathBuf> {
    let value = std::fs::read_to_string(data_dir_override_file(config_dir)).ok()?;
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

pub fn save_configured_data_dir(config_dir: &Path, data_dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(config_dir).map_err(|e| format!("Failed to create config dir: {e}"))?;
    std::fs::write(data_dir_override_file(config_dir), data_dir.to_string_lossy().as_ref())
        .map_err(|e| format!("Failed to save data dir config: {e}"))
}

pub fn clear_configured_data_dir(config_dir: &Path) -> Result<(), String> {
    let path = data_dir_override_file(config_dir);
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(format!("Failed to clear data dir config: {err}")),
    }
}

#[cfg(target_os = "windows")]
fn current_exe_dir() -> Option<PathBuf> {
    std::env::current_exe().ok().and_then(|path| path.parent().map(Path::to_path_buf))
}

#[cfg(target_os = "windows")]
fn portable_marker_exists(exe_dir: &Path) -> bool {
    exe_dir.join(PORTABLE_MARKER).is_file()
}

#[cfg(not(target_os = "windows"))]
fn portable_marker_exists(_exe_dir: &Path) -> bool {
    false
}

#[cfg(target_os = "windows")]
fn installer_marker_exists(exe_dir: &Path) -> bool {
    exe_dir.join(INSTALLER_MARKER).is_file()
}

#[cfg(not(target_os = "windows"))]
fn installer_marker_exists(_exe_dir: &Path) -> bool {
    false
}

fn resolve_data_dir_from_inputs(
    default_app_data_dir: PathBuf,
    exe_dir: Option<PathBuf>,
    portable_marker_exists: bool,
    installer_marker_exists: bool,
    env_data_dir: Option<PathBuf>,
    configured_data_dir: Option<PathBuf>,
) -> DataDirResolution {
    let portable_data_dir = exe_dir.as_ref().filter(|_| portable_marker_exists).map(|dir| dir.join("data"));

    if let Some(env_dir) = env_data_dir {
        return DataDirResolution {
            data_dir: env_dir,
            default_data_dir: default_app_data_dir,
            mode: DataDirMode::EnvOverride,
            portable_data_dir,
        };
    }

    if let Some(configured_dir) = configured_data_dir {
        return DataDirResolution {
            data_dir: configured_dir,
            default_data_dir: default_app_data_dir,
            mode: DataDirMode::ConfiguredOverride,
            portable_data_dir,
        };
    }

    if portable_marker_exists && !installer_marker_exists {
        if let Some(exe_dir) = exe_dir {
            return DataDirResolution {
                data_dir: exe_dir.join("data"),
                default_data_dir: default_app_data_dir,
                mode: DataDirMode::Portable { exe_dir },
                portable_data_dir,
            };
        }
    }

    DataDirResolution {
        data_dir: default_app_data_dir.clone(),
        default_data_dir: default_app_data_dir,
        mode: DataDirMode::Default,
        portable_data_dir,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{alternative_data_dir, default_home_data_dir, resolve_data_dir_from_inputs, DataDirMode};

    #[test]
    fn default_data_dir_is_home_drx_on_all_platforms() {
        assert_eq!(default_home_data_dir(PathBuf::from(r"C:\Users\alice")), PathBuf::from(r"C:\Users\alice\.drx"));
        assert_eq!(default_home_data_dir(PathBuf::from("/Users/alice")), PathBuf::from("/Users/alice/.drx"));
        assert_eq!(default_home_data_dir(PathBuf::from("/home/alice")), PathBuf::from("/home/alice/.drx"));
    }

    #[test]
    fn uses_portable_data_dir_when_marker_exists_without_installer_marker() {
        let default_dir = PathBuf::from(r"C:\Users\Administrator\AppData\Roaming\com.dbx.app");
        let exe_dir = PathBuf::from(r"D:\Apps\DBX");

        let resolution = resolve_data_dir_from_inputs(default_dir, Some(exe_dir.clone()), true, false, None, None);

        assert_eq!(resolution.data_dir, exe_dir.join("data"));
        assert_eq!(resolution.mode, DataDirMode::Portable { exe_dir });
        assert!(resolution.uses_custom_data_dir());
        assert!(resolution.is_portable_mode());
    }

    #[test]
    fn installer_marker_keeps_installed_mode_even_when_portable_marker_exists() {
        let default_dir = PathBuf::from(r"C:\Users\Administrator\AppData\Roaming\com.dbx.app");
        let exe_dir = PathBuf::from(r"C:\Program Files\DBX");

        let resolution = resolve_data_dir_from_inputs(default_dir.clone(), Some(exe_dir), true, true, None, None);

        assert_eq!(resolution.data_dir, default_dir);
        assert_eq!(resolution.mode, DataDirMode::Default);
        assert!(!resolution.uses_custom_data_dir());
        assert!(!resolution.is_portable_mode());
    }

    #[test]
    fn env_override_wins_over_installer_and_portable_markers() {
        let default_dir = PathBuf::from(r"C:\Users\Administrator\AppData\Roaming\com.dbx.app");
        let exe_dir = PathBuf::from(r"C:\Program Files\DBX");
        let env_dir = PathBuf::from(r"E:\DBXData");

        let configured_dir = PathBuf::from(r"D:\ConfiguredDBXData");
        let resolution = resolve_data_dir_from_inputs(
            default_dir,
            Some(exe_dir),
            true,
            true,
            Some(env_dir.clone()),
            Some(configured_dir),
        );

        assert_eq!(resolution.data_dir, env_dir);
        assert_eq!(resolution.mode, DataDirMode::EnvOverride);
        assert!(resolution.uses_custom_data_dir());
        assert!(!resolution.is_portable_mode());
    }

    #[test]
    fn configured_override_wins_over_portable_mode_when_env_is_not_set() {
        let default_dir = PathBuf::from(r"C:\Users\Administrator\.drx");
        let exe_dir = PathBuf::from(r"D:\Apps\DBX");
        let configured_dir = PathBuf::from(r"E:\DBXData");

        let resolution =
            resolve_data_dir_from_inputs(default_dir, Some(exe_dir), true, false, None, Some(configured_dir.clone()));

        assert_eq!(resolution.data_dir, configured_dir);
        assert_eq!(resolution.mode, DataDirMode::ConfiguredOverride);
        assert!(resolution.uses_custom_data_dir());
        assert!(!resolution.is_portable_mode());
        assert_eq!(alternative_data_dir(&resolution), None);
    }

    #[test]
    fn portable_mode_can_import_from_default_data_dir() {
        let default_dir = PathBuf::from(r"C:\Users\Administrator\AppData\Roaming\com.dbx.app");
        let exe_dir = PathBuf::from(r"D:\Apps\DBX");

        let resolution = resolve_data_dir_from_inputs(default_dir.clone(), Some(exe_dir), true, false, None, None);

        assert_eq!(alternative_data_dir(&resolution), Some(default_dir));
    }

    #[test]
    fn installed_mode_can_import_from_leftover_portable_data_dir() {
        let default_dir = PathBuf::from(r"C:\Users\Administrator\AppData\Roaming\com.dbx.app");
        let exe_dir = PathBuf::from(r"C:\Program Files\DBX");

        let resolution = resolve_data_dir_from_inputs(default_dir, Some(exe_dir.clone()), true, true, None, None);

        assert_eq!(alternative_data_dir(&resolution), Some(exe_dir.join("data")));
    }

    #[test]
    fn env_override_does_not_import_from_implicit_alternative_dir() {
        let default_dir = PathBuf::from(r"C:\Users\Administrator\AppData\Roaming\com.dbx.app");
        let exe_dir = PathBuf::from(r"D:\Apps\DBX");

        let resolution = resolve_data_dir_from_inputs(
            default_dir,
            Some(exe_dir),
            true,
            false,
            Some(PathBuf::from(r"E:\DBXData")),
            None,
        );

        assert_eq!(alternative_data_dir(&resolution), None);
    }
}
