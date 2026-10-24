//! Thin wrapper over the Docker CLI.
//!
//! The launcher shells out to `docker` rather than speaking to Docker Desktop's
//! socket directly: the CLI is what Docker Desktop installs and keeps working
//! across its own updates, and it handles contexts, proxies and credentials the
//! launcher would otherwise have to reimplement.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::explain::{DOCKER_STOPPED, DOCKER_UNRESPONSIVE, DOCKER_WONT_OPEN, SHUTTING_DOWN};
use crate::lock;
use crate::process::{self, RunError};

/// Longest a quick query -- version, inspect, port, tag, rm -- may take.
///
/// Every Docker call has a limit because Docker Desktop in a bad state accepts
/// the connection and then answers nothing, and an unbounded call turned that
/// into a spinner that never ended and a quit that never finished.
const QUICK: Duration = Duration::from_secs(15);

/// Limit for `docker stop`, which itself waits up to [`STOP_GRACE_SECS`] for
/// the server, so this has to sit comfortably beyond that.
const STOP_LIMIT: Duration = Duration::from_secs(60);

/// Limit for creating and starting a container, which a Docker Desktop that has
/// only just started can be slow to do.
const RUN_LIMIT: Duration = Duration::from_secs(120);

/// Limit for copying the starter files out of the image, and for deleting an
/// image of several gigabytes.
const COPY_LIMIT: Duration = Duration::from_secs(120);

/// How long a download may print nothing before Docker Desktop is checked on.
///
/// Silence alone is not a stall. Piped rather than shown in a terminal, Docker
/// prints one line when a layer starts downloading and the next when it
/// finishes, so a large layer on a slow connection is minutes of silence while
/// everything is fine. What silence does justify is asking whether Docker
/// Desktop is still answering at all.
const PULL_QUIET_LIMIT: Duration = Duration::from_secs(180);

/// Status of the container the launcher owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContainerState {
    /// No container by that name exists.
    Missing,
    Running,
    /// Exists but is not running (exited, created, paused, ...).
    Stopped,
}

/// Whether Docker Desktop is there to talk to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Daemon {
    Running,
    /// Not running, or not installed: the call failed straight away.
    Stopped,
    /// Something accepted the call but never answered it -- typically Docker
    /// Desktop still starting up, or stuck.
    Unresponsive,
}

/// What happened when macOS was asked to open Docker Desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesktopLaunch {
    Opened,
    /// macOS has no app by that name, so Docker Desktop is not installed even
    /// if a `docker` command is.
    NotInstalled,
}

/// Progress of a `docker pull`, counted in layers.
#[derive(Debug, Clone)]
pub struct PullProgress {
    pub done: u32,
    pub total: u32,
}

/// Locates the `docker` executable.
///
/// `PATH` alone is not enough. An app opened from Finder inherits a minimal
/// environment rather than the one a login shell builds, so `PATH` is typically
/// just `/usr/bin:/bin:/usr/sbin:/sbin` and Docker Desktop's binary is in none of
/// those. Falling back to the known install locations is what stops the
/// launcher from reporting "Docker is not installed" on a Mac that plainly has it.
pub fn locate() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join("docker");
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
    // Docker Desktop 4.x installs a per-user CLI here.
    if let Some(home) = dirs::home_dir() {
        out.push(home.join(".docker").join("bin").join("docker"));
    }
    out.push("/usr/local/bin/docker".into());
    out.push("/opt/homebrew/bin/docker".into());
    out.push("/Applications/Docker.app/Contents/Resources/bin/docker".into());
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
    Ok(command)
}

/// Directories the Docker CLI should search for its own helper executables.
///
/// The CLI shells out to a credential helper named after `credsStore` in
/// `~/.docker/config.json` -- `docker-credential-desktop`, `-osxkeychain`, and
/// so on -- and resolves it through PATH. An app launched from Finder inherits
/// a minimal PATH containing none of Docker's directories, so a pull fails with:
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

/// Runs a quick docker subcommand and returns its trimmed stdout.
fn run(args: &[&str]) -> Result<String, String> {
    run_within(args, QUICK)
}

/// Runs a docker subcommand, giving up on it once `limit` has passed.
///
/// A call that runs out of time fails with [`DOCKER_UNRESPONSIVE`] at the start
/// of its message, which is how both [`daemon`] and the failure screen tell a
/// Docker Desktop that is stuck from one that said no.
fn run_within(args: &[&str], limit: Duration) -> Result<String, String> {
    run_within_env(args, &[], limit)
}

/// `run_within`, with extra variables in the Docker CLI's own environment.
fn run_within_env(args: &[&str], environment: &[(&str, &str)], limit: Duration) -> Result<String, String> {
    let mut command = docker()?;
    command.args(args);
    for (name, value) in environment {
        command.env(name, value);
    }
    let output = match process::output_within(&mut command, limit) {
        Ok(output) => output,
        Err(RunError::TimedOut) => {
            return Err(format!(
                "{DOCKER_UNRESPONSIVE}: `docker {}` gave no answer within {} seconds",
                args.first().copied().unwrap_or_default(),
                limit.as_secs()
            ))
        }
        Err(RunError::Spawn(err)) => return Err(format!("Could not run docker: {err}")),
    };

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

fn timed_out(error: &str) -> bool {
    error.starts_with(DOCKER_UNRESPONSIVE)
}

/// Reports whether the Docker CLI is present on this machine.
pub fn is_installed() -> bool {
    locate().is_some()
}

/// Asks Docker Desktop whether it is running.
///
/// Distinct from [`is_installed`]: Docker Desktop is frequently installed but
/// not started, and the two cases need different handling. A call that times
/// out is reported separately again, because a Docker Desktop that is stuck
/// needs restarting, not opening.
pub fn daemon() -> Daemon {
    match run(&["version", "--format", "{{.Server.Version}}"]) {
        Ok(version) if !version.is_empty() => Daemon::Running,
        Err(error) if timed_out(&error) => Daemon::Unresponsive,
        _ => Daemon::Stopped,
    }
}

/// Asks macOS to open Docker Desktop, or to bring it forward if it is open.
///
/// The exit status is the point: `open` fails when there is no app by that
/// name, which is how a Mac with only a stray `docker` command -- and no Docker
/// Desktop -- is told apart from one where Docker Desktop is just not running.
pub fn open_desktop() -> Result<DesktopLaunch, String> {
    match process::output_within(Command::new("/usr/bin/open").args(["-a", "Docker"]), QUICK) {
        Ok(output) if output.status.success() => Ok(DesktopLaunch::Opened),
        Ok(output) => {
            crate::logs::record(
                "docker",
                &format!(
                    "macOS could not open Docker Desktop: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            );
            Ok(DesktopLaunch::NotInstalled)
        }
        Err(RunError::TimedOut) => Err(format!("{DOCKER_WONT_OPEN}: `open` did not return")),
        Err(RunError::Spawn(err)) => Err(format!("{DOCKER_WONT_OPEN}: {err}")),
    }
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
    run_within(&["image", "rm", id], COPY_LIMIT).map(|_| ())
}

/// Points a tag at an image, which is how a failed update puts the previous
/// version back in place.
pub fn tag(image_id: &str, tag: &str) -> Result<(), String> {
    run(&["tag", image_id, tag]).map(|_| ())
}

/// Pulls an image, reporting layer-level progress.
///
/// Progress is counted in layers rather than bytes because Docker only prints
/// byte counters when stdout is a terminal; piped into the launcher it emits one
/// plain status line per layer transition instead.
///
/// The pull is abandoned as soon as `cancel` is set, which is how quitting
/// stops a download of several gigabytes instead of waiting it out.
pub fn pull<F>(image: &str, cancel: &AtomicBool, mut on_progress: F) -> Result<(), String>
where
    F: FnMut(PullProgress),
{
    match pull_once(image, None, cancel, &mut on_progress) {
        Err(error) if is_credential_failure(&error) => {
            // The user's Docker config names a credential helper that cannot be
            // run. Both Kayak images are public, so the pull needs no
            // credentials at all and can be retried against a config that asks
            // for none, rather than failing a first launch over it.
            let config = credential_free_config()?;
            pull_once(image, Some(&config), cancel, &mut on_progress)
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
    cancel: &AtomicBool,
    on_progress: &mut dyn FnMut(PullProgress),
) -> Result<(), String> {
    let mut command = docker()?;
    // `--config` is a global flag and has to precede the subcommand.
    if let Some(dir) = config_dir {
        command.arg("--config").arg(dir);
    }

    let mut child = command
        .args(["pull", image])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Could not run docker pull: {err}"))?;

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Shared with the watchdog, which may have to kill it while this thread is
    // blocked reading its output.
    let child = Mutex::new(child);
    let last_output = Mutex::new(Instant::now());
    let reading = AtomicBool::new(true);
    let abandoned: Mutex<Option<String>> = Mutex::new(None);

    let stderr = std::thread::scope(|scope| {
        // stderr is drained on its own thread: leaving it unread risks the
        // pipe filling and blocking the pull if Docker writes warnings during
        // a long download.
        let stderr_reader = scope.spawn(|| {
            let mut collected = String::new();
            if let Some(stderr) = stderr {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    *lock(&last_output) = Instant::now();
                    collected.push_str(&line);
                    collected.push('\n');
                }
            }
            collected
        });
        scope.spawn(|| watch_pull(&child, &last_output, &reading, cancel, &abandoned));

        if let Some(stdout) = stdout {
            let mut layers: HashMap<String, String> = HashMap::new();
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                *lock(&last_output) = Instant::now();
                if let Some(progress) = absorb_line(&line, &mut layers) {
                    on_progress(progress);
                }
            }
        }
        reading.store(false, Ordering::SeqCst);
        stderr_reader.join().unwrap_or_default()
    });

    let status = child
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .wait()
        .map_err(|err| format!("docker pull did not finish: {err}"))?;

    if let Some(reason) = abandoned
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
    {
        return Err(reason);
    }
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

/// Kills a pull that is no longer wanted, or that Docker Desktop has stopped
/// serving.
///
/// Runs beside the thread reading the pull's output, because that thread is
/// blocked in a read and cannot notice anything until Docker writes again.
fn watch_pull(
    child: &Mutex<Child>,
    last_output: &Mutex<Instant>,
    reading: &AtomicBool,
    cancel: &AtomicBool,
    abandoned: &Mutex<Option<String>>,
) {
    while reading.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(250));

        let reason = if cancel.load(Ordering::SeqCst) {
            Some(SHUTTING_DOWN.to_string())
        } else if lock(last_output).elapsed() >= PULL_QUIET_LIMIT {
            match daemon() {
                // Still answering, so the silence is a large layer still on its
                // way; check again after another quiet spell.
                Daemon::Running => {
                    *lock(last_output) = Instant::now();
                    None
                }
                Daemon::Stopped => Some(format!("{DOCKER_STOPPED} during the download")),
                Daemon::Unresponsive => Some(format!(
                    "{DOCKER_UNRESPONSIVE}: the download printed nothing for {} seconds",
                    PULL_QUIET_LIMIT.as_secs()
                )),
            }
        } else {
            None
        };

        if let Some(reason) = reason {
            crate::logs::record("docker", &format!("abandoning the download: {reason}"));
            *lock(abandoned) = Some(reason);
            let _ = lock(child).kill();
            return;
        }
    }
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
    run_within(&["create", "--name", SCRATCH, image], RUN_LIMIT)?;

    // The trailing `/.` copies the directory's contents into the destination
    // rather than nesting the directory itself inside it.
    let from = format!("{SCRATCH}:{source}/.");
    let to = destination.to_string_lossy().to_string();
    let result = run_within(&["cp", &from, &to], COPY_LIMIT);

    let _ = run(&["rm", "--force", SCRATCH]);
    result.map(|_| ())
}

/// Reports the state of a container by name.
///
/// Only "no such container" means missing. Any other failure -- Docker Desktop
/// stopped or not answering -- is an error, because reading it as "missing"
/// told the launcher a healthy server had exited the moment Docker hiccuped.
pub fn container_state(name: &str) -> Result<ContainerState, String> {
    match run(&["inspect", "--type", "container", "--format", "{{.State.Status}}", name]) {
        Ok(status) if status == "running" => Ok(ContainerState::Running),
        Ok(_) => Ok(ContainerState::Stopped),
        Err(error) if error.to_ascii_lowercase().contains("no such") => Ok(ContainerState::Missing),
        Err(error) => Err(error),
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
    if container_state(name)? == ContainerState::Missing {
        return Ok(());
    }
    // `rm --force` also stops it, so a single call covers running and stopped.
    run(&["rm", "--force", name]).map(|_| ())
}

/// How long the server is given to shut down before the daemon kills it.
///
/// Spelled out rather than left to the daemon because the daemon's own default
/// is a local setting, and Docker Desktop has been seen using three seconds.
/// Kayak stops its sandbox containers on the way down, which does not reliably
/// fit in three seconds, and being killed part-way through leaves the sandboxes
/// it had not reached still running. The call returns as soon as the container
/// exits, so a generous limit costs nothing in the normal case.
const STOP_GRACE_SECS: &str = "30";

/// Stops a container without deleting it.
pub fn stop_container(name: &str) -> Result<(), String> {
    if container_state(name)? != ContainerState::Running {
        return Ok(());
    }
    run_within(&["stop", "-t", STOP_GRACE_SECS, name], STOP_LIMIT).map(|_| ())
}

/// Everything needed to start the Kayak server container.
pub struct RunSpec<'a> {
    pub image: &'a str,
    pub name: &'a str,
    pub port: u16,
    pub data_dir: &'a Path,
    /// The key the server will require of every request; see `access`.
    pub access_key: Option<&'a str>,
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
    // The server checks the request origin, and the browser loads Kayak
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

    let mut args = vec![
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
        // No --add-host: Docker Desktop resolves host.docker.internal by itself,
        // which is how the server reaches model servers published on the Mac.
    ];
    // Named only: Docker copies the value from its own environment, so the key
    // never appears in a command line that any process on the Mac can list.
    let mut environment: Vec<(&str, &str)> = Vec::new();
    if let Some(key) = spec.access_key {
        args.extend(["--env", crate::access::ENV_NAME]);
        environment.push((crate::access::ENV_NAME, key));
    }
    args.push(spec.image);
    run_within_env(&args, &environment, RUN_LIMIT).map(|_| ())
}

/// Whether a container was created with `name=value` in its environment.
///
/// How a running server is checked for the access key it should require. False
/// when the container cannot be inspected, which leads to it being replaced --
/// the safe direction.
pub fn container_has_env(container: &str, name: &str, value: &str) -> bool {
    let Ok(output) = run(&[
        "container",
        "inspect",
        container,
        "--format",
        "{{range .Config.Env}}{{println .}}{{end}}",
    ]) else {
        return false;
    };
    env_lines_contain(&output, name, value)
}

fn env_lines_contain(output: &str, name: &str, value: &str) -> bool {
    let wanted = format!("{name}={value}");
    output.lines().any(|line| line.trim_end() == wanted)
}

/// Returns the last lines of a container's logs, for diagnosing a failed start.
pub fn container_logs(name: &str, lines: u32) -> String {
    let count = lines.to_string();
    let Ok(mut command) = docker() else {
        return String::new();
    };
    command.args(["logs", "--tail", &count, name]);
    let Ok(output) = process::output_within(&mut command, QUICK) else {
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
/// When every port in the range is taken the system is asked for any free port
/// rather than giving up: an unusual port is a far better outcome than Kayak
/// not starting at all.
pub fn find_free_port(preferred: u16, range: u16) -> Result<u16, String> {
    for port in preferred.max(1)..preferred.saturating_add(range) {
        if TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok() {
            return Ok(port);
        }
    }
    TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|err| format!("Could not find a free port: {err}"))
}

/// Reports whether `docker run` failed because the host port was taken.
///
/// [`find_free_port`] checks a port by binding it and letting go, so another
/// program can take it in the moment before Docker does. Docker Desktop also
/// keeps ports of its own that a bind from this process does not see.
pub fn is_port_clash(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("port is already allocated")
        || error.contains("address already in use")
        || error.contains("ports are not available")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_variable_in_a_container_environment() {
        let output = "PATH=/usr/bin\nKAYAK_AUTH_TOKEN=abc123\nKAYAK_PORT=8000\n";

        assert!(env_lines_contain(output, "KAYAK_AUTH_TOKEN", "abc123"));
        assert!(!env_lines_contain(output, "KAYAK_AUTH_TOKEN", "abc"));
        assert!(!env_lines_contain(output, "KAYAK_AUTH_TOKEN", "other"));
        assert!(!env_lines_contain("", "KAYAK_AUTH_TOKEN", "abc123"));
    }

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
    fn a_free_preferred_port_is_used() {
        let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        // The listener is dropped above, so the port is free again.
        assert_eq!(find_free_port(port, 1).unwrap(), port);
    }

    #[test]
    fn a_full_range_falls_back_to_a_port_the_system_picks() {
        // Used to fail outright with "No free port between ...", which stopped
        // Kayak from starting on a machine with busy development ports.
        let taken = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = taken.local_addr().unwrap().port();

        let chosen = find_free_port(port, 1).unwrap();

        assert_ne!(chosen, port);
        assert_ne!(chosen, 0);
    }

    #[test]
    fn recognises_a_port_clash_from_docker() {
        assert!(is_port_clash(
            "docker: Error response from daemon: Ports are not available: exposing port TCP \
             127.0.0.1:8000 -> 0.0.0.0:0: listen tcp 127.0.0.1:8000: bind: address already in use."
        ));
        assert!(is_port_clash(
            "Bind for 127.0.0.1:8000 failed: port is already allocated"
        ));
        assert!(!is_port_clash("no space left on device"));
        assert!(!is_port_clash("Conflict. The container name \"/kayak-app\" is already in use"));
    }

    #[test]
    fn only_a_timed_out_call_reads_as_unresponsive() {
        assert!(timed_out(&format!("{DOCKER_UNRESPONSIVE}: `docker version` gave no answer")));
        assert!(!timed_out("Cannot connect to the Docker daemon"));
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
