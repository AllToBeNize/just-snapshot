//! The move from the old name (Snapshot, up to 0.3) to just-snapshot. On
//! the first start after the rename the settings folder, the translation
//! keys in the Credential Manager, the default save folder in Pictures and
//! the default file name move over; nothing is overwritten, and whatever
//! already exists under the new name wins. The start-at-login entry moves
//! in `startup::follow_executable`.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

pub const OLD_NAME: &str = "Snapshot";
pub const NAME: &str = "just-snapshot";
const OLD_TEMPLATE: &str = "Snapshot {date} {time}";

/// `%LOCALAPPDATA%\just-snapshot`, where every settings file lives.
pub fn app_data() -> Result<PathBuf, String> {
    let app_data = env::var_os("LOCALAPPDATA").ok_or("找不到 LOCALAPPDATA。")?;
    Ok(PathBuf::from(app_data).join(NAME))
}

/// Moves everything over when only the old settings folder exists; returns
/// what could not be moved, for one message.
pub fn run() -> Vec<String> {
    let Ok(new) = app_data() else {
        return Vec::new();
    };
    let mut failures = Vec::new();
    match move_folder(&new.with_file_name(OLD_NAME), &new, "设置文件夹") {
        Ok(true) => {}
        Ok(false) => return failures,
        Err(error) => {
            failures.push(error);
            return failures;
        }
    }
    failures.extend(crate::credential::migrate(OLD_NAME, NAME));
    if let Err(error) = migrate_output() {
        failures.push(error);
    }
    failures
}

/// The default save folder and file name, unless the user picked their own.
fn migrate_output() -> Result<(), String> {
    use crate::output_settings::{pictures, OutputConfig, DEFAULT_TEMPLATE};
    let mut config = OutputConfig::load()?;
    let mut changed = false;
    if let Some(pictures) = pictures() {
        let (old, new) = (pictures.join(OLD_NAME), pictures.join(NAME));
        let chosen = config.folder.trim();
        let default = chosen.is_empty();
        let old_chosen = !default && PathBuf::from(chosen) == old;
        if (default || old_chosen) && move_folder(&old, &new, "截图文件夹")? {
            if old_chosen {
                config.folder = new.display().to_string();
                changed = true;
            }
        }
    }
    if config.template == OLD_TEMPLATE {
        config.template = DEFAULT_TEMPLATE.into();
        changed = true;
    }
    if changed {
        config.save()?;
    }
    Ok(())
}

/// Renames `old` to `new` when only `old` exists; `Ok(false)` when there was
/// nothing to move or `new` is already taken.
fn move_folder(old: &Path, new: &Path, what: &str) -> Result<bool, String> {
    if new.exists() || !old.is_dir() {
        return Ok(false);
    }
    fs::rename(old, new)
        .map(|()| true)
        .map_err(|error| format!("{what} {} 没能改名为 {}：{error}", old.display(), new.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_only_into_a_free_name() {
        let root = env::temp_dir().join(format!("just-snapshot-migrate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (old, new) = (root.join(OLD_NAME), root.join(NAME));
        assert_eq!(move_folder(&old, &new, "x"), Ok(false), "nothing to move");
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("shortcuts.txt"), "a").unwrap();
        assert_eq!(move_folder(&old, &new, "x"), Ok(true));
        assert_eq!(fs::read_to_string(new.join("shortcuts.txt")).unwrap(), "a");
        assert!(!old.exists());
        fs::create_dir_all(&old).unwrap();
        fs::write(old.join("shortcuts.txt"), "b").unwrap();
        assert_eq!(move_folder(&old, &new, "x"), Ok(false), "the new folder wins");
        assert_eq!(fs::read_to_string(new.join("shortcuts.txt")).unwrap(), "a");
        assert!(old.exists(), "the old one is left alone");
        fs::remove_dir_all(&root).unwrap();
    }
}
