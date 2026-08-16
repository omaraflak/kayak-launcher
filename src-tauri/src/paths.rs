//! Host locations the launcher reads and writes.

use std::path::{Path, PathBuf};

/// Directory bind-mounted into the container as `/app/data`.
///
/// This has to be a real host path rather than a Docker named volume: the
/// server starts sandbox containers as siblings on the host daemon and mounts
/// workspace directories into them by host path, which a named volume could not
/// supply.
pub fn data_dir() -> PathBuf {
    let base = dirs::data_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Kayak").join("data")
}

/// Creates the data directory if absent.
///
/// Doing this before `docker run` matters on Linux: the daemon creates a
/// missing bind-mount source itself, but owned by root, which then leaves the
/// user unable to read their own Kayak data.
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
