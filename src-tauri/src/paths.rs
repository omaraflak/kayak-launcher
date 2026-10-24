//! Host locations the launcher reads and writes.

use std::path::{Path, PathBuf};

/// Directory bind-mounted into the container as `/app/data`:
/// `~/Library/Application Support/Kayak/data`.
///
/// This has to be a real Mac path rather than a Docker named volume: the
/// server starts sandbox containers as siblings on the host daemon and mounts
/// workspace directories into them by host path, which a named volume could not
/// supply.
pub fn data_dir() -> PathBuf {
    let base = dirs::data_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Kayak").join("data")
}

/// A file of the launcher's own beside the data directory, in
/// `~/Library/Application Support/Kayak`.
///
/// Beside it rather than in it: parts of the data directory are mounted into
/// containers, and what the launcher keeps for itself is no business of theirs.
pub fn beside_data(name: &str) -> PathBuf {
    data_dir().with_file_name(name)
}

/// Writes one of those files, owner-only, creating the folder first: on a first
/// launch nothing has made `~/Library/Application Support/Kayak` yet, and a
/// write that fails there fails silently in effect, since nothing reads it back
/// until the next launch.
pub fn write_beside_data(name: &str, body: &[u8]) -> Result<(), String> {
    let path = beside_data(name);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;
    }
    crate::control::replace_file(&path, body, 0o600)
}

/// Creates the data directory if absent.
///
/// Done before anything else touches it: seeding copies the image's defaults
/// into it, and `docker run` mounts it.
pub fn ensure_data_dir() -> Result<PathBuf, String> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("Could not create {}: {err}", dir.display()))?;
    Ok(dir)
}

/// Reports whether the data directory still needs its defaults copied in.
///
/// Emptiness is the signal, so seeding happens exactly once. Re-seeding on
/// every start would resurrect defaults the user had deliberately deleted, and
/// could overwrite ones they had edited.
pub fn is_unseeded(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Err(_) => true,
        Ok(mut entries) => entries.next().is_none(),
    }
}

/// Where the starter files are copied before being moved into place.
///
/// A sibling of the data directory, so the final move is a rename within one
/// volume rather than a copy.
pub fn seed_staging_dir(data_dir: &Path) -> PathBuf {
    data_dir.with_file_name("data.seeding")
}

/// Moves a complete set of starter files into the (empty) data directory.
///
/// Copying straight into the data directory left a half-copied one behind
/// when the copy was interrupted, and because emptiness is the signal for
/// seeding, the missing defaults were never copied again. Copying elsewhere
/// first and renaming means the data directory goes from empty to complete in
/// one step. A rename replaces an empty directory outright; anything that has
/// appeared in it meanwhile is kept, and the staged files are moved in beside it.
pub fn adopt_seed(staging: &Path, data_dir: &Path) -> Result<(), String> {
    if std::fs::rename(staging, data_dir).is_ok() {
        return Ok(());
    }
    let entries = std::fs::read_dir(staging)
        .map_err(|err| format!("Could not read {}: {err}", staging.display()))?;
    for entry in entries {
        let entry = entry.map_err(|err| format!("Could not read {}: {err}", staging.display()))?;
        let target = data_dir.join(entry.file_name());
        if target.exists() {
            continue;
        }
        std::fs::rename(entry.path(), &target)
            .map_err(|err| format!("Could not move {}: {err}", entry.path().display()))?;
    }
    let _ = std::fs::remove_dir_all(staging);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("kayak-launcher-seed").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn staged_files_replace_an_empty_data_directory() {
        let root = scratch("empty");
        let data = root.join("data");
        std::fs::create_dir_all(&data).unwrap();
        let staging = seed_staging_dir(&data);
        std::fs::create_dir_all(staging.join("agents")).unwrap();
        std::fs::write(staging.join("agents").join("default.json"), "{}").unwrap();

        assert!(is_unseeded(&data));
        adopt_seed(&staging, &data).unwrap();

        assert!(data.join("agents").join("default.json").is_file());
        assert!(!staging.exists());
        assert!(!is_unseeded(&data));
    }

    #[test]
    fn staged_files_join_anything_already_there() {
        let root = scratch("busy");
        let data = root.join("data");
        std::fs::create_dir_all(data.join(".launcher")).unwrap();
        let staging = seed_staging_dir(&data);
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("settings.json"), "{}").unwrap();

        adopt_seed(&staging, &data).unwrap();

        assert!(data.join(".launcher").is_dir());
        assert!(data.join("settings.json").is_file());
        assert!(!staging.exists());
    }

    #[test]
    fn the_launchers_own_files_sit_beside_the_data_directory() {
        let file = beside_data("access-key");
        assert_eq!(file.parent(), data_dir().parent());
        assert_eq!(file.file_name().unwrap(), "access-key");
    }

    #[test]
    fn the_staging_directory_sits_beside_the_data_directory() {
        let data = PathBuf::from("/Users/me/Library/Application Support/Kayak/data");
        assert_eq!(
            seed_staging_dir(&data),
            PathBuf::from("/Users/me/Library/Application Support/Kayak/data.seeding")
        );
    }
}
