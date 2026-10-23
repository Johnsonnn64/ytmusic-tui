//! Firefox's Linux XDG profile location is missing from rookie 0.5.6.
//! Keep its cookie reader and legacy discovery, adding only the missing path.

use std::path::{Path, PathBuf};

pub(super) fn extract(domains: Option<Vec<String>>) -> rookie::Result<Vec<rookie::enums::Cookie>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from);
    if let Some(config) = config_home(home, xdg)
        && let Some(path) = cookie_path(&config.join("mozilla/firefox"))?
    {
        return rookie::firefox_based(path, domains);
    }
    // Preserve rookie's ~/.mozilla/firefox, Snap, and Flatpak discovery.
    rookie::firefox(domains)
}

fn config_home(home: Option<PathBuf>, xdg: Option<PathBuf>) -> Option<PathBuf> {
    // The XDG specification requires an absolute path; ignore empty/relative values.
    xdg.filter(|path| path.is_absolute())
        .or_else(|| home.map(|path| path.join(".config")))
}

fn cookie_path(root: &Path) -> rookie::Result<Option<PathBuf>> {
    let profiles_path = root.join("profiles.ini");
    if !profiles_path.is_file() {
        return Ok(None);
    }
    let profiles = ini::Ini::load_from_file(profiles_path)?;
    // Match rookie's priority: per-install default, profile marked Default=1,
    // then another registered profile. Skip stale entries whose store is gone.
    let installs = profiles.iter().filter_map(|(section, properties)| {
        section
            .is_some_and(|name| name.starts_with("Install"))
            .then(|| properties.get("Default"))
            .flatten()
    });
    let defaults = profiles.iter().filter_map(|(section, properties)| {
        (section.is_some_and(|name| name.starts_with("Profile"))
            && properties.get("Default") == Some("1"))
        .then(|| properties.get("Path"))
        .flatten()
    });
    let others = profiles.iter().filter_map(|(section, properties)| {
        section
            .is_some_and(|name| name.starts_with("Profile"))
            .then(|| properties.get("Path"))
            .flatten()
    });
    Ok(installs
        .chain(defaults)
        .chain(others)
        .filter(|path| !path.is_empty())
        // Path::join also handles absolute profile paths (IsRelative=0).
        .map(|path| root.join(path).join("cookies.sqlite"))
        .find(|path| path.is_file()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(profile: &Path) -> PathBuf {
        std::fs::create_dir_all(profile).unwrap();
        let path = profile.join("cookies.sqlite");
        // Discovery only: no real cookies or SQLite contents are needed.
        std::fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn xdg_location_defaults_to_dot_config_and_honors_absolute_override() {
        let home = Some(PathBuf::from("/home/example"));
        for xdg in [None, Some(PathBuf::new()), Some(PathBuf::from("relative"))] {
            assert_eq!(
                config_home(home.clone(), xdg),
                Some(PathBuf::from("/home/example/.config"))
            );
        }
        assert_eq!(
            config_home(home, Some(PathBuf::from("/custom/config"))),
            Some(PathBuf::from("/custom/config"))
        );
    }

    #[test]
    fn discovers_xdg_install_default_ahead_of_other_profiles() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(".config/mozilla/firefox");
        let expected = store(&root.join("selected.default-release"));
        store(&root.join("other.default"));
        std::fs::write(root.join("profiles.ini"), "[Profile0]\nPath=other.default\nDefault=1\n[InstallABC]\nDefault=selected.default-release\n").unwrap();
        assert_eq!(cookie_path(&root).unwrap(), Some(expected));
    }

    #[test]
    fn supports_absolute_profile_paths_and_skips_stale_install_defaults() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("config/mozilla/firefox");
        std::fs::create_dir_all(&root).unwrap();
        let profile = temp.path().join("external-profile");
        let expected = store(&profile);
        std::fs::write(
            root.join("profiles.ini"),
            format!(
                "[InstallABC]\nDefault=missing\n[Profile0]\nIsRelative=0\nPath={}\nDefault=1\n",
                profile.display()
            ),
        )
        .unwrap();
        assert_eq!(cookie_path(&root).unwrap(), Some(expected));
    }

    #[test]
    fn missing_xdg_store_leaves_legacy_discovery_available() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(cookie_path(temp.path()).unwrap(), None);
        std::fs::write(
            temp.path().join("profiles.ini"),
            "[Profile0]\nPath=missing\nDefault=1\n",
        )
        .unwrap();
        assert_eq!(cookie_path(temp.path()).unwrap(), None);
    }
}
