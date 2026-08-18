//! File-based control channel between the Kayak container and the launcher.
//!
//! Metal inference has to be started by a native macOS process, and the only
//! one Kayak has is this launcher. The container therefore needs a way to ask
//! for it. A socket would have to be reachable from the container, and
//! `host.docker.internal` resolves to the host's gateway rather than loopback,
//! so any listening endpoint would be bound to every interface -- putting an
//! API that spawns host processes on whatever network the laptop is attached
//! to. The data directory is already bind-mounted into the container, so
//! passing files through it needs no listening socket at all.
//!
//! The protocol is declarative rather than a request/response RPC. Kayak writes
//! the state it wants and the launcher reconciles towards it, which means a
//! missed poll, a restart on either side, or a duplicate write all converge to
//! the same place instead of replaying an action.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Subdirectory of the shared data directory holding the channel files.
const CONTROL_DIR: &str = ".launcher";
const DESIRED_FILE: &str = "desired.json";
const STATUS_FILE: &str = "status.json";

/// What Kayak wants to be true. Written by the container, read by the launcher.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Desired {
    pub metal: MetalDesired,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MetalDesired {
    /// Whether a Metal server should be running.
    pub running: bool,
    /// Model to serve. Ignored unless `running` is set.
    pub model: Option<String>,
}

/// What is actually true. Written by the launcher, read by the container.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Status {
    pub metal: MetalStatus,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct MetalStatus {
    /// Whether this machine can run Metal inference at all.
    pub supported: bool,
    /// Whether the vllm-metal environment is present.
    pub installed: bool,
    /// One of: stopped, installing, starting, ready, error.
    pub state: String,
    pub model: Option<String>,
    pub port: u16,
    pub error: Option<String>,
}

/// Locations of the two channel files inside a data directory.
pub struct ControlPaths {
    pub dir: PathBuf,
    pub desired: PathBuf,
    pub status: PathBuf,
}

impl ControlPaths {
    pub fn under(data_dir: &Path) -> Self {
        let dir = data_dir.join(CONTROL_DIR);
        Self {
            desired: dir.join(DESIRED_FILE),
            status: dir.join(STATUS_FILE),
            dir,
        }
    }
}

/// Reads the desired state, treating anything unreadable as "nothing wanted".
///
/// A partially written or corrupt file must not strand a running server or
/// crash the reconcile loop, so it degrades to the default rather than an
/// error: the next write from Kayak will correct it.
pub fn read_desired(path: &Path) -> Desired {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Writes the status file atomically.
///
/// The container polls this file continuously. Writing in place would let it
/// observe a half-written document and parse it as "no Metal at all", so the
/// replacement is made by rename within the same directory.
pub fn write_status(path: &Path, status: &Status) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Status path has no directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;

    let body = serde_json::to_string_pretty(status)
        .map_err(|err| format!("Could not encode status: {err}"))?;

    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, body)
        .map_err(|err| format!("Could not write {}: {err}", temporary.display()))?;
    std::fs::rename(&temporary, path)
        .map_err(|err| format!("Could not replace {}: {err}", path.display()))?;
    Ok(())
}

/// Decides whether a requested model is safe to hand to `vllm serve`.
///
/// The value crosses a trust boundary: anything able to write into the data
/// directory can set it, and it becomes a command-line argument. Requiring the
/// `mlx-community/` prefix both matches what the Metal backend can actually
/// serve and rules out a value starting with `-`, which would otherwise be
/// parsed as a flag rather than a model.
pub fn accepts_model(model: &str) -> bool {
    if model.len() > 200 || model.contains(char::is_whitespace) {
        return false;
    }
    if model.contains("..") || model.contains('\0') {
        return false;
    }
    crate::metal::is_mlx_model(model)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_desired_file_means_nothing_is_wanted() {
        let desired = read_desired(Path::new("/nonexistent/desired.json"));

        assert!(!desired.metal.running);
        assert_eq!(desired.metal.model, None);
    }

    #[test]
    fn corrupt_desired_file_does_not_panic() {
        let dir = std::env::temp_dir().join("kayak-control-corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("desired.json");
        // A half-flushed write from the container looks exactly like this.
        std::fs::write(&path, "{\"metal\": {\"runn").unwrap();

        assert_eq!(read_desired(&path).metal, MetalDesired::default());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let dir = std::env::temp_dir().join("kayak-control-forward");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("desired.json");
        // A newer Kayak talking to an older launcher must still be understood.
        std::fs::write(
            &path,
            r#"{"metal":{"running":true,"model":"mlx-community/X","future":1},"other":2}"#,
        )
        .unwrap();

        let desired = read_desired(&path);
        assert!(desired.metal.running);
        assert_eq!(desired.metal.model.as_deref(), Some("mlx-community/X"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn status_is_written_and_readable() {
        let dir = std::env::temp_dir().join("kayak-control-status");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("status.json");

        let status = Status {
            metal: MetalStatus {
                supported: true,
                installed: true,
                state: "ready".to_string(),
                model: Some("mlx-community/X".to_string()),
                port: 8001,
                error: None,
            },
        };
        write_status(&path, &status).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"ready\""));
        assert!(raw.contains("8001"));
        // The temporary file must not survive the rename.
        assert!(!dir.join("status.json.tmp").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn accepts_only_mlx_models() {
        assert!(accepts_model("mlx-community/Qwen3.8-27B-8bit"));
        assert!(!accepts_model("Qwen/Qwen2.5-Coder-7B-Instruct"));
    }

    #[test]
    fn rejects_values_that_would_not_be_a_model() {
        // Would be read as a flag by the vllm CLI rather than a repository.
        assert!(!accepts_model("--host"));
        assert!(!accepts_model("-v"));
        assert!(!accepts_model("mlx-community/x y"));
        assert!(!accepts_model("mlx-community/../../etc/passwd"));
        assert!(!accepts_model(""));
        assert!(!accepts_model(&format!("mlx-community/{}", "x".repeat(300))));
    }
}
