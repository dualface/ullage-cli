//! The view settings that outlive one run of the TUI.
//!
//! Only what the user toggled by hand belongs here, and losing the file costs
//! nothing: every read falls back to the default layout, so a missing,
//! unreadable, or corrupt file is never an error the user has to see.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How the cards are placed on the screen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Layout {
    /// Cards flow left to right and wrap onto new bands.
    #[default]
    Columns,
    /// One card per band, however wide the terminal is.
    Vertical,
}

impl Layout {
    pub(crate) fn toggled(self) -> Self {
        match self {
            Self::Columns => Self::Vertical,
            Self::Vertical => Self::Columns,
        }
    }

    pub(crate) fn is_vertical(self) -> bool {
        self == Self::Vertical
    }
}

/// The stored settings, as they sit on disk.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub(crate) struct Preferences {
    #[serde(default)]
    pub(crate) layout: Layout,
}

/// Reads the saved settings, or the defaults when nothing usable is stored.
pub(crate) fn load() -> Preferences {
    preferences_path().map(load_from).unwrap_or_default()
}

fn load_from(path: impl AsRef<Path>) -> Preferences {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Writes the settings, reporting nothing: a view that cannot remember the
/// layout still shows it, and the user is in the middle of reading a screen.
pub(crate) fn save(preferences: Preferences) {
    if let Some(path) = preferences_path() {
        let _ = save_to(path, preferences);
    }
}

fn save_to(path: impl AsRef<Path>, preferences: Preferences) -> io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string(&preferences).map_err(io::Error::other)?;
    // Write beside the target and rename, so an interrupted write leaves the
    // previous settings intact rather than a half-written file.
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, text)?;
    std::fs::rename(&temporary, path)
}

/// `tui.json` beside the daemon's state file, which is where the per-user,
/// per-machine data this program keeps already lives.
fn preferences_path() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("ULLAGE_TUI_STATE_FILE") {
        return Some(PathBuf::from(path));
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/Ullage/tui.json"));
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        if let Some(root) = std::env::var_os("XDG_STATE_HOME") {
            return Some(PathBuf::from(root).join("ullage/tui.json"));
        }
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".local/state/ullage/tui.json"))
    }
    #[cfg(windows)]
    {
        return std::env::var_os("LOCALAPPDATA")
            .map(|root| PathBuf::from(root).join("Ullage/tui.json"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "ullage-cli-tui-preferences-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        directory.join("tui.json")
    }

    #[test]
    fn a_saved_layout_comes_back_on_the_next_run() {
        let path = scratch("roundtrip");

        save_to(
            &path,
            Preferences {
                layout: Layout::Vertical,
            },
        )
        .unwrap();

        assert_eq!(load_from(&path).layout, Layout::Vertical);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_missing_or_corrupt_file_reads_as_the_default_layout() {
        let missing = scratch("missing");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(load_from(&missing).layout, Layout::Columns);

        let corrupt = scratch("corrupt");
        std::fs::write(&corrupt, "{ not json").unwrap();
        assert_eq!(load_from(&corrupt).layout, Layout::Columns);
        std::fs::remove_file(&corrupt).unwrap();
    }

    #[test]
    fn saving_creates_the_directory_it_needs() {
        let path = std::env::temp_dir()
            .join(format!("ullage-cli-tui-prefs-new-{}", std::process::id()))
            .join("nested")
            .join("tui.json");
        let root = path.parent().unwrap().parent().unwrap().to_owned();
        let _ = std::fs::remove_dir_all(&root);

        save_to(
            &path,
            Preferences {
                layout: Layout::Vertical,
            },
        )
        .unwrap();

        assert_eq!(load_from(&path).layout, Layout::Vertical);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
