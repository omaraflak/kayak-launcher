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
/// Process group of the running GPU server, so a launcher that crashed or was
/// force-quit can be followed by one that stops what it left behind.
const SERVER_PID_FILE: &str = "metal.pid";
/// The same, for the GPU installer.
const INSTALLER_PID_FILE: &str = "metal-install.pid";

/// What Kayak wants to be true. Written by the container, read by the launcher.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Desired {
    pub metal: MetalDesired,
}

#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MetalDesired {
    /// Whether a Metal server should be running.
    pub running: bool,
    /// Model to serve. Ignored unless `running` is set.
    pub model: Option<String>,
    /// Opaque token Kayak attaches to each start request. Echoed back in
    /// `MetalStatus` so Kayak can tell a status that answers *this* request
    /// from one written before the request existed — without the echo, a
    /// stale "ready" from the previous server reads as instant success.
    pub request: Option<String>,
    /// The user's Hugging Face access token, handed to the GPU server so it
    /// can download gated models. Without it every gated model failed to
    /// start. A secret: it is never logged, and `Debug` leaves it out.
    pub hf_token: Option<String>,
}

/// Written by hand so the token cannot end up in a log line or a panic
/// message through a stray `{:?}`.
impl std::fmt::Debug for MetalDesired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MetalDesired")
            .field("running", &self.running)
            .field("model", &self.model)
            .field("request", &self.request)
            .field("hf_token", &self.hf_token.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// What is actually true. Written by the launcher, read by the container.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Status {
    pub metal: MetalStatus,
    pub versions: Versions,
}

/// What is installed, on both sides of the container boundary.
///
/// Kayak cannot read either of these itself: the launcher's version is a
/// different program, and the server's version is a label on the image it is
/// running from, which is not visible from inside that image.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct Versions {
    pub launcher: String,
    pub kayak: Option<String>,
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
    /// Port the GPU server for `model` listens on, chosen when it starts.
    /// Zero when no server is starting or running.
    pub port: u16,
    /// What went wrong, as one sentence Kayak can show as it is.
    pub error: Option<String>,
    /// What the installer or the server printed before it failed, for Kayak's
    /// logs panel. Never meant to be shown as the explanation itself.
    pub error_detail: Option<String>,
    /// Why Metal is unavailable, when the hardware could otherwise do it.
    /// Distinct from `error`, which describes a start that was attempted.
    pub detail: Option<String>,
    /// The `request` token from the desired state this status answers.
    /// Always serialised, even when null: the key's presence is how Kayak
    /// knows this launcher echoes requests at all.
    pub request: Option<String>,
}

/// Locations of the channel files inside a data directory.
pub struct ControlPaths {
    pub dir: PathBuf,
    pub desired: PathBuf,
    pub status: PathBuf,
    pub server_pid: PathBuf,
    pub installer_pid: PathBuf,
}

impl ControlPaths {
    pub fn under(data_dir: &Path) -> Self {
        let dir = data_dir.join(CONTROL_DIR);
        Self {
            desired: dir.join(DESIRED_FILE),
            status: dir.join(STATUS_FILE),
            server_pid: dir.join(SERVER_PID_FILE),
            installer_pid: dir.join(INSTALLER_PID_FILE),
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
    let body = serde_json::to_string_pretty(status)
        .map_err(|err| format!("Could not encode status: {err}"))?;
    replace_file(path, body.as_bytes(), 0o644)
}

/// Asks for no GPU server, as if Kayak had.
///
/// Called once when the launcher starts. The desired state outlives the app,
/// so without this the last model Kayak asked for started again by itself at
/// the next launch -- after a crash, a force-quit, or simply quitting with a
/// model loaded -- before anyone had asked for it. Owner-only, like the copy
/// Kayak writes, since that one can carry a token.
pub fn reset_desired(path: &Path) -> Result<(), String> {
    let body = r#"{"metal": {"running": false, "model": null, "request": null}}"#;
    replace_file(path, body.as_bytes(), 0o600)
}

/// Replaces a file in one step, by writing beside it and renaming over it.
pub(crate) fn replace_file(path: &Path, body: &[u8], mode: u32) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| "Control path has no directory".to_string())?;
    std::fs::create_dir_all(parent)
        .map_err(|err| format!("Could not create {}: {err}", parent.display()))?;

    let temporary = path.with_extension("json.tmp");
    let _ = std::fs::remove_file(&temporary);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&temporary)
        .and_then(|mut file| file.write_all(body))
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
                error_detail: None,
                detail: None,
                request: None,
            },
            versions: Versions {
                launcher: "1.2.3".to_string(),
                kayak: Some("4.5.6".to_string()),
            },
        };
        write_status(&path, &status).unwrap();

        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"ready\""));
        assert!(raw.contains("8001"));
        // Both versions travel with every status write, because Kayak can read
        // neither of them on its own.
        assert!(raw.contains("1.2.3"));
        assert!(raw.contains("4.5.6"));
        // The temporary file must not survive the rename.
        assert!(!dir.join("status.json.tmp").exists());
        // The request key must be present even when empty: its presence is
        // how Kayak knows this launcher can echo requests.
        assert!(raw.contains("\"request\""));
        // Likewise the raw detail, so Kayak can tell this launcher sends it.
        assert!(raw.contains("\"error_detail\""));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hugging_face_token_is_read_but_never_printed() {
        let dir = std::env::temp_dir().join("kayak-control-token");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("desired.json");
        std::fs::write(
            &path,
            r#"{"metal":{"running":true,"model":"mlx-community/X","hf_token":"hf_secret123"}}"#,
        )
        .unwrap();

        let desired = read_desired(&path);

        assert_eq!(desired.metal.hf_token.as_deref(), Some("hf_secret123"));
        let printed = format!("{desired:?}");
        assert!(!printed.contains("hf_secret123"), "{printed}");
        assert!(printed.contains("redacted"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_desired_file_without_a_token_still_parses() {
        let dir = std::env::temp_dir().join("kayak-control-no-token");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("desired.json");
        std::fs::write(&path, r#"{"metal":{"running":true,"model":"mlx-community/X"}}"#).unwrap();

        let desired = read_desired(&path);

        assert!(desired.metal.running);
        assert_eq!(desired.metal.hf_token, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resetting_asks_for_nothing_and_keeps_the_file_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join("kayak-control-reset");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("desired.json");
        std::fs::write(
            &path,
            r#"{"metal":{"running":true,"model":"mlx-community/X","hf_token":"hf_x"}}"#,
        )
        .unwrap();

        reset_desired(&path).unwrap();

        let desired = read_desired(&path);
        assert!(!desired.metal.running);
        assert_eq!(desired.metal.model, None);
        assert_eq!(desired.metal.hf_token, None);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn desired_request_token_is_read() {
        let dir = std::env::temp_dir().join("kayak-control-request");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("desired.json");
        std::fs::write(
            &path,
            r#"{"metal":{"running":true,"model":"mlx-community/X","request":"abc123"}}"#,
        )
        .unwrap();

        assert_eq!(read_desired(&path).metal.request.as_deref(), Some("abc123"));
        // An older Kayak that sends no token must still be understood.
        std::fs::write(&path, r#"{"metal":{"running":true,"model":"mlx-community/X"}}"#).unwrap();
        assert_eq!(read_desired(&path).metal.request, None);
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
