//! Thin wrapper over the Docker CLI.
//!
//! The launcher shells out to `docker` rather than speaking to the daemon
//! socket directly. The CLI is the only interface guaranteed to behave the same
//! across Docker Desktop on macOS and Windows and a plain daemon on Linux, and
//! it keeps the launcher free of a TLS/named-pipe transport it would otherwise
//! have to reimplement per platform.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Status of the container the launcher owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    /// No container by that name exists.
    Missing,
    Running,
    /// Exists but is not running (exited, created, paused, ...).
    Stopped,
}

/// Progress of a `docker pull`, counted in layers.
#[derive(Debug, Clone)]
pub struct PullProgress {
    pub done: u32,
    pub total: u32,
}

/// Locates the `docker` executable.
///
/// `PATH` alone is not enough. An app opened from Finder or the Start menu
/// inherits a minimal environment rather than the one a login shell builds, so
/// on macOS `PATH` is typically just `/usr/bin:/bin:/usr/sbin:/sbin` and Docker
/// Desktop's binary is in none of those. Falling back to the known install
/// locations is what stops the launcher from reporting "Docker is not
/// installed" on a machine that plainly has it.
pub fn locate() -> Option<PathBuf> {
    let binary = if cfg!(windows) { "docker.exe" } else { "docker" };

    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(binary);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }

    known_locations()
        .into_iter()
        .find(|candidate| candidate.is_file())
}

fn known_locations() -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();

    // Docker Desktop 4.x installs a per-user CLI here on macOS and Windows.
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".docker").join("bin").join("docker"));
    }

    #[cfg(target_os = "macos")]
    {
        out.push("/usr/local/bin/docker".into());
        out.push("/opt/homebrew/bin/docker".into());
        out.push("/Applications/Docker.app/Contents/Resources/bin/docker".into());
    }

    #[cfg(target_os = "linux")]
    {
        out.push("/usr/bin/docker".into());
        out.push("/usr/local/bin/docker".into());
        out.push("/snap/bin/docker".into());
    }

    #[cfg(target_os = "windows")]
    {
        for var in ["ProgramFiles", "ProgramW6432", "LOCALAPPDATA"] {
            if let Some(root) = std::env::var_os(var) {
                out.push(
                    PathBuf::from(root)
                        .join("Docker")
                        .join("Docker")
                        .join("resources")
                        .join("bin")
                        .join("docker.exe"),
                );
            }
        }
    }

    out
}

/// Builds a `docker` invocation, or reports that the CLI could not be found.
///
/// The binary is resolved on every call rather than cached, so a user who
/// installs Docker while the launcher is open gets picked up on their next
/// retry instead of having to restart the app.
fn docker() -> Result<Command, String> {
    let binary = locate().ok_or_else(|| "Docker is not installed".to_string())?;
    let mut command = Command::new(&binary);
    // Locating the CLI is not enough on its own: the CLI spawns helpers of its
    // own and finds them on PATH. See `helper_search_path`.
    if let Ok(path) = std::env::join_paths(helper_search_path(&binary)) {
        command.env("PATH", path);
    }
    suppress_console(&mut command);
    Ok(command)
}

/// Directories the Docker CLI should search for its own helper executables.
///
/// The CLI shells out to a credential helper named after `credsStore` in
/// `~/.docker/config.json` -- `docker-credential-desktop`, `-osxkeychain`, and
/// so on -- and resolves it through PATH. An app launched from Finder or the
/// Start menu inherits a minimal PATH containing none of Docker's directories,
/// so a pull fails with:
///
/// ```text
/// error getting credentials - err: exec: "docker-credential-desktop":
/// executable file not found in $PATH
/// ```
///
/// Whether this bites depends on where a particular Docker install puts the CLI
/// relative to its helpers, which is why it reproduces on some machines and not
/// others. Putting every known Docker directory on the child's PATH makes it
/// independent of that layout.
fn helper_search_path(binary: &Path) -> Vec<PathBuf> {
    let mut preferred: Vec<PathBuf> = Vec::new();
    // The helper usually ships alongside the CLI, so its directory comes first.
    if let Some(parent) = binary.parent() {
        preferred.push(parent.to_path_buf());
    }
    preferred.extend(known_locations().iter().filter_map(|candidate| {
        candidate.parent().map(Path::to_path_buf)
    }));

    let inherited = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();

    merge_search_dirs(preferred, inherited)
}

/// Concatenates two directory lists, keeping first occurrences only.
///
/// Order matters: Docker's own directories are searched before the inherited
/// PATH, so a helper shipped with the install wins over an unrelated binary of
/// the same name earlier in the user's PATH.
fn merge_search_dirs(preferred: Vec<PathBuf>, inherited: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    preferred
        .into_iter()
        .chain(inherited)
        .filter(|dir| !dir.as_os_str().is_empty() && seen.insert(dir.clone()))
        .collect()
}

/// Stops Windows from flashing a console window for each CLI call.
fn suppress_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    {
        let _ = command;
    }
}

/// Runs a docker subcommand and returns its trimmed stdout.
fn run(args: &[&str]) -> Result<String, String> {
    let output = docker()?
        .args(args)
        .output()
        .map_err(|err| format!("Could not run docker: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("docker {} failed", args.join(" "))
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Reports whether the Docker CLI is present on this machine.
pub fn is_installed() -> bool {
    locate().is_some()
}

/// Reports whether the Docker daemon is reachable.
///
/// Distinct from [`is_installed`]: Docker Desktop is frequently installed but
/// not started, and the two cases need different guidance in the UI.
pub fn is_daemon_running() -> bool {
    run(&["version", "--format", "{{.Server.Version}}"])
        .map(|version| !version.is_empty())
        .unwrap_or(false)
}

/// Asks the OS to start Docker Desktop.
///
/// On Linux the daemon is managed by the init system and starting it needs
/// privileges the launcher does not have, so this is unsupported there.
pub fn start_desktop() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("/usr/bin/open");
        command.args(["-a", "Docker"]);
        command
            .status()
            .map_err(|err| format!("Could not start Docker Desktop: {err}"))?;
        return Ok(());
    }

    #[cfg(target_os = "windows")]
    {
        for var in ["ProgramFiles", "ProgramW6432"] {
            let Some(root) = std::env::var_os(var) else {
                continue;
            };
            let exe = PathBuf::from(root)
                .join("Docker")
                .join("Docker")
                .join("Docker Desktop.exe");
            if exe.is_file() {
                let mut command = Command::new(exe);
                suppress_console(&mut command);
                command
                    .spawn()
                    .map_err(|err| format!("Could not start Docker Desktop: {err}"))?;
                return Ok(());
            }
        }
        return Err("Could not find Docker Desktop".to_string());
    }

    #[allow(unreachable_code)]
    Err("Start the Docker daemon, then try again".to_string())
}

/// Reports whether an image is present locally.
pub fn has_image(image: &str) -> bool {
    run(&["image", "inspect", image, "--format", "{{.Id}}"]).is_ok()
}

/// Returns the repository digest an image was pulled at, if any.
///
/// This is the digest of the manifest Docker resolved the tag to, which is what
/// Docker Hub reports for the same tag, so the two can be compared to detect a
/// newer publish. Images built locally have no repository digest and yield
/// `None`.
pub fn image_digest(image: &str) -> Option<String> {
    let output = run(&["image", "inspect", image, "--format", "{{json .RepoDigests}}"]).ok()?;
    let digests: Vec<String> = serde_json::from_str(&output).ok()?;
    digests
        .into_iter()
        .find_map(|entry| entry.split_once('@').map(|(_, digest)| digest.to_string()))
}

/// Returns the version an image was labelled with when it was built.
///
/// The release workflow stamps `org.opencontainers.image.version`, which gives
/// the user a real version number to see. It is optional on purpose: an image
/// built by hand carries no label, and the digest is shown instead.
pub fn image_version(image: &str) -> Option<String> {
    let output = run(&[
        "image",
        "inspect",
        image,
        "--format",
        r#"{{index .Config.Labels "org.opencontainers.image.version"}}"#,
    ])
    .ok()?;

    let version = output.trim();
    // Docker's template prints this literal when the key is absent.
    if version.is_empty() || version == "<no value>" {
        return None;
    }
    Some(version.to_string())
}

/// Returns the local image ID a tag currently points at.
///
/// Captured before a pull so the image it replaces can be identified
/// afterwards: repointing a tag leaves the old image behind with no tag, and
/// these are multi-gigabyte.
pub fn image_id(image: &str) -> Option<String> {
    run(&["image", "inspect", image, "--format", "{{.Id}}"]).ok()
}

/// Deletes an image by ID.
///
/// Failure is expected and ignored by callers: Docker refuses to remove an
/// image a container still references, which is the correct outcome when
/// something is still using it.
pub fn remove_image(id: &str) -> Result<(), String> {
    run(&["image", "rm", id]).map(|_| ())
}

/// Pulls an image, reporting layer-level progress.
///
/// Progress is counted in layers rather than bytes because Docker only prints
/// byte counters when stdout is a terminal; piped into the launcher it emits one
/// plain status line per layer transition instead.
pub fn pull<F>(image: &str, mut on_progress: F) -> Result<(), String>
where
    F: FnMut(PullProgress),
{
    match pull_once(image, None, &mut on_progress) {
        Err(error) if is_credential_failure(&error) => {
            // The user's Docker config names a credential helper that cannot be
            // run. Both Kayak images are public, so the pull needs no
            // credentials at all and can be retried against a config that asks
            // for none, rather than failing a first launch over it.
            let config = credential_free_config()?;
            pull_once(image, Some(&config), &mut on_progress)
        }
        result => result,
    }
}

/// Reports whether a failure came from Docker's credential helper machinery.
fn is_credential_failure(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("docker-credential") || error.contains("error getting credentials")
}

/// Creates a throwaway Docker config directory that names no credential helper.
///
/// Only the pull is redirected to it. Everything else keeps using the real
/// config, so proxy settings and daemon contexts defined there still apply.
fn credential_free_config() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("kayak-launcher-docker-config");
    std::fs::create_dir_all(&dir)
        .map_err(|err| format!("Could not create a temporary Docker config: {err}"))?;
    std::fs::write(dir.join("config.json"), "{}\n")
        .map_err(|err| format!("Could not write a temporary Docker config: {err}"))?;
    Ok(dir)
}

/// One `docker pull` attempt.
fn pull_once(
    image: &str,
    config_dir: Option<&Path>,
    on_progress: &mut dyn FnMut(PullProgress),
) -> Result<(), String> {
    let mut command = docker()?;
    // `--config` is a global flag and has to precede the subcommand.
    if let Some(dir) = config_dir {
        command.arg("--config").arg(dir);
    }

    let mut child = command
        .args(["pull", image])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Could not run docker pull: {err}"))?;

    // stderr is drained on its own thread: leaving it unread risks the pipe
    // filling and blocking the pull if Docker writes warnings during a long
    // download.
    let stderr = child.stderr.take();
    let stderr_reader = std::thread::spawn(move || {
        let mut collected = String::new();
        if let Some(stderr) = stderr {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                collected.push_str(&line);
                collected.push('\n');
            }
        }
        collected
    });

    if let Some(stdout) = child.stdout.take() {
        let mut layers: HashMap<String, String> = HashMap::new();
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(progress) = absorb_line(&line, &mut layers) {
                on_progress(progress);
            }
        }
    }

    let status = child
        .wait()
        .map_err(|err| format!("docker pull did not finish: {err}"))?;
    let stderr = stderr_reader.join().unwrap_or_default();

    if status.success() {
        return Ok(());
    }
    let detail = stderr.trim();
    Err(if detail.is_empty() {
        format!("Could not download {image}")
    } else {
        detail.to_string()
    })
}

/// Layer status lines Docker emits, in the order a layer moves through them.
const LAYER_STATUSES: [&str; 8] = [
    "Pulling fs layer",
    "Waiting",
    "Downloading",
    "Verifying Checksum",
    "Download complete",
    "Extracting",
    "Pull complete",
    "Already exists",
];

/// Statuses that mean a layer needs no further work.
const LAYER_DONE: [&str; 2] = ["Pull complete", "Already exists"];

/// Folds one line of `docker pull` output into the layer table.
///
/// Returns updated progress when the line changed something, and `None` for
/// lines that carry no progress information.
pub fn absorb_line(line: &str, layers: &mut HashMap<String, String>) -> Option<PullProgress> {
    let (id, rest) = line.split_once(": ")?;
    let id = id.trim();
    let rest = rest.trim();

    let status = LAYER_STATUSES
        .iter()
        .find(|status| rest.starts_with(*status))?;

    // Global lines such as "latest: Pulling from omaraflak/kayak" share the
    // `prefix: suffix` shape, so layer IDs are identified by their form: short
    // lowercase hex.
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    let previous = layers.insert(id.to_string(), (*status).to_string());
    if previous.as_deref() == Some(*status) {
        return None;
    }

    let total = layers.len() as u32;
    let done = layers
        .values()
        .filter(|status| LAYER_DONE.contains(&status.as_str()))
        .count() as u32;

    Some(PullProgress { done, total })
}

/// Copies a directory out of an image onto the host.
///
/// Used to seed the data directory. The image ships default agents, skills and
/// tools in `/app/data`, and bind-mounting a host directory over that path
/// hides them completely -- so on a first run the contents have to be lifted out
/// of the image before the mount exists, or the user opens Kayak to find it
/// empty.
///
/// A container is created but never started; `docker cp` reads from its
/// filesystem directly, which is why this works without running the server.
pub fn copy_out_of_image(image: &str, source: &str, destination: &Path) -> Result<(), String> {
    const SCRATCH: &str = "kayak-seed";

    // A leftover from an interrupted previous attempt would make `create` fail
    // on the name.
    let _ = run(&["rm", "--force", SCRATCH]);
    run(&["create", "--name", SCRATCH, image])?;

    // The trailing `/.` copies the directory's contents into the destination
    // rather than nesting the directory itself inside it.
    let from = format!("{SCRATCH}:{source}/.");
    let to = destination.to_string_lossy().to_string();
    let result = run(&["cp", &from, &to]);

    let _ = run(&["rm", "--force", SCRATCH]);
    result.map(|_| ())
}

/// Reports the state of a container by name.
pub fn container_state(name: &str) -> ContainerState {
    match run(&["inspect", "--format", "{{.State.Status}}", name]) {
        Err(_) => ContainerState::Missing,
        Ok(status) if status == "running" => ContainerState::Running,
        Ok(_) => ContainerState::Stopped,
    }
}

/// Returns the image ID a container was created from.
///
/// Compared against the current image ID to notice a container that predates an
/// update. Docker records the resolved ID, so this stays correct even though the
/// container was created from a moving tag.
pub fn container_image_id(name: &str) -> Option<String> {
    run(&["inspect", "--format", "{{.Image}}", name]).ok()
}

/// Returns the host port a running container publishes its server port on.
pub fn published_port(name: &str) -> Option<u16> {
    let mapping = run(&["port", name, &format!("{}/tcp", crate::config::INTERNAL_PORT)]).ok()?;
    // Output looks like "127.0.0.1:8000"; with several bindings, one per line.
    mapping
        .lines()
        .next()?
        .rsplit_once(':')?
        .1
        .trim()
        .parse()
        .ok()
}

/// Stops and deletes a container, ignoring one that is already gone.
pub fn remove_container(name: &str) -> Result<(), String> {
    if container_state(name) == ContainerState::Missing {
        return Ok(());
    }
    // `rm --force` also stops it, so a single call covers running and stopped.
    run(&["rm", "--force", name]).map(|_| ())
}

/// Stops a container without deleting it.
pub fn stop_container(name: &str) -> Result<(), String> {
    if container_state(name) != ContainerState::Running {
        return Ok(());
    }
    run(&["stop", name]).map(|_| ())
}

/// Everything needed to start the Kayak server container.
pub struct RunSpec<'a> {
    pub image: &'a str,
    pub name: &'a str,
    pub port: u16,
    pub data_dir: &'a Path,
}

/// Creates and starts the Kayak server container.
///
/// The configuration mirrors the project's docker-compose service, with two
/// deliberate differences: the port is resolved at runtime rather than fixed,
/// and no restart policy is set. A desktop app that quits should stop its
/// server, not leave a container that silently restarts on every boot.
pub fn run_container(spec: &RunSpec) -> Result<(), String> {
    let data = spec.data_dir.to_string_lossy().to_string();
    let port_binding = format!(
        "127.0.0.1:{}:{}",
        spec.port,
        crate::config::INTERNAL_PORT
    );
    let data_mount = format!("{data}:/app/data");
    let socket_mount = format!(
        "{}:{}",
        crate::config::DOCKER_SOCKET,
        crate::config::DOCKER_SOCKET
    );
    let internal_port = format!("KAYAK_PORT={}", crate::config::INTERNAL_PORT);
    // The server checks the request origin, and the launcher window loads Kayak
    // over the resolved host port, which is not always the default one.
    let cors = format!(
        "KAYAK_CORS_ORIGINS=http://127.0.0.1:{port},http://localhost:{port}",
        port = spec.port
    );
    // The published repository, so the server starts sandboxes from the same
    // image the launcher pulled rather than from a second name for it.
    let sandbox = format!("KAYAK_SANDBOX_IMAGE={}", crate::config::sandbox_image());
    // Sandboxes are siblings on the host daemon, so paths the server asks it to
    // mount have to be host paths. Inside the container it only knows
    // `/app/data`, so the host equivalent is passed in explicitly.
    let host_data = format!("KAYAK_HOST_DATA_DIR={data}");

    run(&[
        "run",
        "--detach",
        "--name",
        spec.name,
        // Published to loopback only. The server grants agents shell access and,
        // through the mounted socket, control of the host daemon, so it must not
        // be reachable from the network.
        "--publish",
        &port_binding,
        "--volume",
        &data_mount,
        "--volume",
        &socket_mount,
        "--env",
        "KAYAK_HOST=0.0.0.0",
        "--env",
        &internal_port,
        "--env",
        &cors,
        "--env",
        &sandbox,
        "--env",
        &host_data,
        // Lets the server reach services on the host, such as a local vLLM.
        "--add-host",
        "host.docker.internal:host-gateway",
        spec.image,
    ])
    .map(|_| ())
}

/// Returns the last lines of a container's logs, for diagnosing a failed start.
pub fn container_logs(name: &str, lines: u32) -> String {
    let count = lines.to_string();
    let Ok(mut command) = docker() else {
        return String::new();
    };
    let Ok(output) = command.args(["logs", "--tail", &count, name]).output() else {
        return String::new();
    };
    // Container logs arrive on both streams; failures are usually on stderr.
    let mut combined = String::from_utf8_lossy(&output.stdout).to_string();
    combined.push_str(&String::from_utf8_lossy(&output.stderr));
    combined.trim().to_string()
}

/// Finds a free loopback port, starting from `preferred`.
///
/// Falling back matters because 8000 is a popular development port; without
/// this, Kayak would fail to start for anyone already running something there.
pub fn find_free_port(preferred: u16, range: u16) -> Result<u16, String> {
    for port in preferred..preferred.saturating_add(range) {
        if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return Ok(port);
        }
    }
    Err(format!(
        "No free port between {preferred} and {}",
        preferred.saturating_add(range)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(lines: &[&str]) -> Option<PullProgress> {
        let mut layers = HashMap::new();
        let mut last = None;
        for line in lines {
            if let Some(update) = absorb_line(line, &mut layers) {
                last = Some(update);
            }
        }
        last
    }

    #[test]
    fn counts_layers_as_they_complete() {
        let update = progress(&[
            "latest: Pulling from omaraflak/kayak",
            "a1b2c3d4e5f6: Pulling fs layer",
            "f6e5d4c3b2a1: Pulling fs layer",
            "a1b2c3d4e5f6: Downloading",
            "a1b2c3d4e5f6: Pull complete",
        ])
        .expect("expected progress");

        assert_eq!(update.total, 2);
        assert_eq!(update.done, 1);
    }

    #[test]
    fn counts_cached_layers_as_done() {
        let update = progress(&[
            "a1b2c3d4e5f6: Already exists",
            "f6e5d4c3b2a1: Already exists",
        ])
        .expect("expected progress");

        assert_eq!(update.done, 2);
        assert_eq!(update.total, 2);
    }

    #[test]
    fn ignores_non_layer_lines() {
        let mut layers = HashMap::new();

        // The repository line shares the "prefix: suffix" shape but names a tag,
        // and would otherwise be counted as a layer.
        assert!(absorb_line("latest: Pulling from omaraflak/kayak", &mut layers).is_none());
        assert!(absorb_line("Digest: sha256:abc", &mut layers).is_none());
        assert!(absorb_line("Status: Image is up to date", &mut layers).is_none());
        assert!(absorb_line("no colon here", &mut layers).is_none());
        assert!(layers.is_empty());
    }

    #[test]
    fn reports_nothing_when_a_status_repeats() {
        let mut layers = HashMap::new();

        assert!(absorb_line("a1b2c3d4e5f6: Downloading", &mut layers).is_some());
        assert!(absorb_line("a1b2c3d4e5f6: Downloading", &mut layers).is_none());
    }

    #[test]
    fn docker_directories_are_searched_before_the_inherited_path() {
        // A helper shipped with the Docker install must win over an unrelated
        // binary of the same name sitting earlier in the user's PATH.
        let merged = merge_search_dirs(
            vec![PathBuf::from("/usr/local/bin")],
            vec![PathBuf::from("/opt/mystuff/bin"), PathBuf::from("/usr/bin")],
        );

        assert_eq!(merged[0], PathBuf::from("/usr/local/bin"));
        assert_eq!(merged.len(), 3);
    }

    #[test]
    fn search_directories_are_deduplicated() {
        let merged = merge_search_dirs(
            vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/usr/bin")],
            vec![PathBuf::from("/usr/local/bin"), PathBuf::from("/bin")],
        );

        assert_eq!(
            merged,
            vec![
                PathBuf::from("/usr/local/bin"),
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
            ]
        );
    }

    #[test]
    fn empty_path_entries_are_dropped() {
        // An empty entry in PATH means "the current directory" to some tools,
        // which is not somewhere we want a helper resolved from.
        let merged = merge_search_dirs(vec![], vec![PathBuf::from(""), PathBuf::from("/bin")]);

        assert_eq!(merged, vec![PathBuf::from("/bin")]);
    }

    #[test]
    fn recognises_credential_helper_failures() {
        assert!(is_credential_failure(
            "error getting credentials - err: exec: \"docker-credential-desktop\": \
             executable file not found in $PATH, out: ``"
        ));
        assert!(is_credential_failure(
            "docker-credential-osxkeychain not found"
        ));
    }

    #[test]
    fn leaves_unrelated_failures_alone() {
        // Retrying these against a different config would only waste a second
        // attempt and report the wrong cause.
        assert!(!is_credential_failure("no such host"));
        assert!(!is_credential_failure(
            "manifest for omaraflak/kayak:latest not found"
        ));
        assert!(!is_credential_failure("no space left on device"));
    }

    #[test]
    fn a_layer_moving_backwards_still_counts_once() {
        // Docker can re-report a layer; the table holds one status per ID, so the
        // completed count must not double.
        let update = progress(&[
            "a1b2c3d4e5f6: Pull complete",
            "a1b2c3d4e5f6: Extracting",
            "a1b2c3d4e5f6: Pull complete",
        ])
        .expect("expected progress");

        assert_eq!(update.total, 1);
        assert_eq!(update.done, 1);
    }
}
