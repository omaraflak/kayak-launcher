//! Launcher for Kayak.
//!
//! Kayak ships as a Docker image. This app exists so that running it never
//! requires a terminal: it finds Docker, downloads the images, starts the
//! server, shows Kayak in a desktop window, and watches Docker Hub so a new
//! release can be installed from a button rather than a `docker pull`.

mod access;
mod config;
mod control;
mod docker;
mod explain;
mod logs;
mod metal;
mod paths;
mod process;
mod registry;

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::menu::{AboutMetadata, Menu, MenuBuilder, MenuItemBuilder, SubmenuBuilder};
use tauri::{
    webview::DownloadEvent, AppHandle, Emitter, Manager, RunEvent, Url, WebviewUrl,
    WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_dialog::{DialogExt, MessageDialogKind};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

use docker::{ContainerState, Daemon, DesktopLaunch};
use explain::Problem;

const LAUNCHER_WINDOW: &str = "launcher";
const KAYAK_WINDOW: &str = "kayak";

/// Injected into the Kayak window so the launcher can draw an update banner
/// over a page it does not control.
const BANNER_JS: &str = include_str!("banner.js");

/// Prefix for the fake navigations the injected banner uses to call back in.
const ACTION_PREFIX: &str = "/__launcher/";

/// Gap between background update checks.
const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Limit on asking GitHub whether a newer app exists. Without one, a check that
/// hung stalled the update watcher, and with it every later check, for good.
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the app's own update download may receive nothing before it is
/// abandoned. A stalled download used to leave a progress screen over Kayak
/// with no way past it.
const LAUNCHER_DOWNLOAD_STALL: Duration = Duration::from_secs(60);

/// How long Docker Desktop is given to start answering after being opened.
/// Its first start after installing, or after a macOS update, is slow.
const DOCKER_START_LIMIT: Duration = Duration::from_secs(5 * 60);

/// When the wait for Docker Desktop starts offering the user a way to help.
const DOCKER_HINT_AFTER: Duration = Duration::from_secs(45);

/// How long quitting waits for a start or an update that is under way to
/// notice and give up, before stopping things underneath it.
const BUSY_GRACE: Duration = Duration::from_secs(20);

/// How long quitting keeps waiting, once everything is stopped, for a start or
/// an update still inside a Docker call -- so that a server the call creates
/// can be stopped too.
const SHUTDOWN_LIMIT: Duration = Duration::from_secs(45);

/// The longest quitting may take. Past this the app exits regardless, because
/// a quit that never finishes is worse than one that leaves Docker to clean up.
/// It leaves room after [`SHUTDOWN_LIMIT`] for one more graceful stop; an
/// ordinary quit is over in seconds and never comes near it.
const QUIT_DEADLINE: Duration = Duration::from_secs(90);

/// Gap between health checks of a running Kayak.
const WATCH_INTERVAL: Duration = Duration::from_secs(5);

/// Missed health checks in a row before the watch looks into why.
const WATCH_MISSES: u32 = 3;

/// Missed health checks in a row after which a server that is still running is
/// treated as hung rather than busy: two minutes.
const HUNG_MISSES: u32 = 24;

/// How many times `docker run` is retried on another port after a clash.
const PORT_ATTEMPTS: u32 = 3;

/// Lines of the launcher log "Copy details" puts on the clipboard.
const DETAIL_LINES: usize = 200;

const MENU_RELOAD: &str = "reload";
const MENU_SHOW_LOG: &str = "show-log";

/// Locks a mutex, carrying on with its data if another thread panicked while
/// holding it.
///
/// What these mutexes guard stays meaningful after a panic elsewhere, and the
/// alternative -- every later lock panicking in turn -- is a launcher that can
/// no longer even quit cleanly.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the launcher window is currently showing.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Stage {
    Checking,
    /// Docker Desktop is not on this Mac.
    DockerMissing {
        url: String,
    },
    /// Docker Desktop has been opened and the launcher is waiting for it.
    WaitingForDocker,
    /// Docker Desktop is not running and the user is asked to open it; the
    /// launcher carries on by itself once it answers. `closed` is set when it
    /// went away while Kayak was in use.
    DockerAsleep {
        closed: bool,
    },
    Downloading {
        step: u32,
        steps: u32,
        done: u32,
        total: u32,
    },
    Starting {
        headline: String,
        hint: String,
    },
    /// An update is being applied with no Kayak window to show it in.
    Updating {
        message: String,
        percent: i32,
    },
    Ready,
    Failed {
        headline: String,
        body: String,
    },
    Stopping,
}

/// What is known about the published version versus the installed one.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateInfo {
    pub available: bool,
    pub checking: bool,
    /// Publish date of the available version, already formatted for display.
    pub date: Option<String>,
    pub installed: Option<String>,
    pub published: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct Snapshot {
    stage: Stage,
    update: UpdateInfo,
}

/// A message drawn over Kayak that has to outlive a reload of the page.
///
/// A failed update can end with the window being pointed at a restarted
/// server, which reloads the page and wipes anything drawn on it; held here,
/// the message is drawn again when the new page announces itself.
#[derive(Debug, Clone, Serialize)]
struct Notice {
    title: String,
    message: String,
    /// Label of the button that closes it.
    dismiss: String,
}

/// A GPU start that failed, and the request it was the answer to.
///
/// Remembered so the reconcile loop reports it instead of starting the same
/// thing again every two seconds -- burning the machine and never saying why
/// the first attempt died.
struct MetalFailure {
    model: String,
    request: Option<String>,
    failure: metal::Failure,
}

impl MetalFailure {
    /// Reports whether this failure answers what Kayak is asking for now.
    ///
    /// A failure belongs to one request rather than to a model: Kayak retries
    /// by asking for the same model with a new request token, and remembering
    /// failures per model meant Try again was answered at once with the old
    /// error and never tried again.
    fn answers(&self, model: &str, request: Option<&str>) -> bool {
        self.model == model && self.request.as_deref() == request
    }
}

pub struct Launcher {
    stage: Mutex<Stage>,
    update: Mutex<UpdateInfo>,
    port: Mutex<Option<u16>>,
    /// The Metal inference server, when one is running. Native to the host
    /// rather than containerised, because Metal has no passthrough into Docker.
    metal: Mutex<Option<metal::Server>>,
    metal_failure: Mutex<Option<MetalFailure>>,
    /// The GPU installer's process group while it runs, so quitting can stop
    /// a download of several gigabytes instead of leaving it to finish alone.
    installer: Mutex<Option<process::Group>>,
    /// Serialises writes of the status file, so the final "stopped" written
    /// while quitting cannot be overwritten by a reconcile pass under way.
    status_write: Mutex<()>,
    /// A launcher update waiting to be installed, held so the button in the
    /// Kayak window can apply the one already found rather than checking again.
    launcher_update: Mutex<Option<tauri_plugin_updater::Update>>,
    notice: Mutex<Option<Notice>>,
    /// What the last failed start was, which decides what Try again does first.
    last_problem: Mutex<Option<Problem>>,
    /// Set when the running server must be replaced rather than reconnected
    /// to at the next start, because it stopped answering.
    recreate_next: AtomicBool,
    /// Set once the app has begun quitting. Every loop and sequence checks it,
    /// so nothing new is started underneath the shutdown.
    shutting_down: AtomicBool,
    /// Set once everything has been stopped, so it happens only once whichever
    /// way the app is quitting.
    stopped: AtomicBool,
    /// Guards the boot and update sequences so a double-click cannot run two
    /// `docker run` calls against the same container name.
    busy: AtomicBool,
    /// Whether the GPU control channel has been set up for this launch.
    bridge_started: AtomicBool,
}

impl Launcher {
    fn new() -> Self {
        Self {
            stage: Mutex::new(Stage::Checking),
            update: Mutex::new(UpdateInfo::default()),
            port: Mutex::new(None),
            metal: Mutex::new(None),
            metal_failure: Mutex::new(None),
            installer: Mutex::new(None),
            status_write: Mutex::new(()),
            launcher_update: Mutex::new(None),
            notice: Mutex::new(None),
            last_problem: Mutex::new(None),
            recreate_next: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            bridge_started: AtomicBool::new(false),
        }
    }
}

/// Claims the busy flag, returning false when a sequence is already running.
struct BusyGuard(Arc<Launcher>);

impl BusyGuard {
    fn acquire(state: &Arc<Launcher>) -> Option<Self> {
        if state.busy.swap(true, Ordering::SeqCst) {
            return None;
        }
        Some(Self(state.clone()))
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

fn stopping(state: &Launcher) -> bool {
    state.shutting_down.load(Ordering::SeqCst)
}

/// Fails the current step if the app has begun quitting, so a start or an
/// update in progress stops between steps instead of starting things the
/// shutdown has already been through.
fn ensure_not_stopping(state: &Launcher) -> Result<(), String> {
    if stopping(state) {
        Err(explain::SHUTTING_DOWN.to_string())
    } else {
        Ok(())
    }
}

fn set_stage(app: &AppHandle, state: &Launcher, stage: Stage) {
    *lock(&state.stage) = stage.clone();
    let _ = app.emit("stage", stage);
}

fn set_update(app: &AppHandle, state: &Launcher, update: UpdateInfo) {
    *lock(&state.update) = update.clone();
    let _ = app.emit("update", update);
}

fn kayak_window_visible(app: &AppHandle) -> bool {
    app.get_webview_window(KAYAK_WINDOW)
        .is_some_and(|window| window.is_visible().unwrap_or(false))
}

/// Brings the launcher window forward and hides Kayak's.
///
/// Used whenever Kayak cannot be used: the Kayak window would otherwise sit on
/// a dead or blank page with nothing to say why, while the explanation was in
/// a window the user could not see.
fn surface_launcher(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(KAYAK_WINDOW) {
        let _ = window.hide();
    }
    if let Some(window) = app.get_webview_window(LAUNCHER_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

/// Shows a failure in the launcher window, in words rather than error text.
fn show_problem(app: &AppHandle, state: &Launcher, problem: Problem) {
    *lock(&state.last_problem) = Some(problem);
    set_stage(
        app,
        state,
        Stage::Failed {
            headline: problem.headline().to_string(),
            body: problem.body().to_string(),
        },
    );
    surface_launcher(app);
}

/// Reports a failed start. The raw error goes to the log, where Copy details
/// and Help › Show Kayak Log can reach it; the screen gets an explanation.
fn fail(app: &AppHandle, state: &Launcher, raw: &str) {
    logs::record("boot", &format!("kayak could not be started: {raw}"));
    show_problem(app, state, explain::classify(raw));
}

fn show_docker_missing(app: &AppHandle, state: &Launcher) {
    // The download for the right kind of chip, so nobody has to know which one
    // their Mac has. A translated launcher is on an Apple chip too.
    let url = match metal::silicon() {
        metal::Silicon::None => config::DOCKER_DOWNLOAD_INTEL_CHIP,
        metal::Silicon::Native | metal::Silicon::Translated => config::DOCKER_DOWNLOAD_APPLE_CHIP,
    };
    set_stage(app, state, Stage::DockerMissing { url: url.to_string() });
    surface_launcher(app);
}

/// Reports whether the Kayak server answers its health endpoint.
fn kayak_answers(port: u16) -> bool {
    ureq::get(&format!("http://127.0.0.1:{port}/api/health"))
        .timeout(Duration::from_secs(3))
        .call()
        .is_ok()
}

/// Waits for the Kayak server to answer its health endpoint.
///
/// The container is polled alongside the endpoint so that a server which exits
/// on startup -- a bad image, a port clash inside the container -- surfaces its
/// logs immediately instead of after the full timeout.
fn wait_for_health(port: u16, timeout: Duration, state: &Launcher) -> Result<(), String> {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        ensure_not_stopping(state)?;
        if kayak_answers(port) {
            return Ok(());
        }

        if docker::container_state(config::CONTAINER_NAME)? != ContainerState::Running {
            let logs = docker::container_logs(config::CONTAINER_NAME, 20);
            return Err(if logs.is_empty() {
                format!("{}.", explain::SERVER_EXITED)
            } else {
                format!("{}:\n{logs}", explain::SERVER_EXITED)
            });
        }

        thread::sleep(Duration::from_millis(600));
    }

    Err(format!("{}.", explain::HEALTH_TIMEOUT))
}

/// Ensures both images are present locally, downloading whichever are missing.
fn ensure_images(app: &AppHandle, state: &Launcher, force: bool) -> Result<(), String> {
    let server = config::server_image();
    let sandbox = config::sandbox_image();

    let mut wanted: Vec<&str> = Vec::new();
    if force || !docker::has_image(&server) {
        wanted.push(server.as_str());
    }
    if force || !docker::has_image(&sandbox) {
        wanted.push(sandbox.as_str());
    }

    let steps = wanted.len() as u32;
    for (index, image) in wanted.iter().enumerate() {
        ensure_not_stopping(state)?;
        let step = index as u32 + 1;
        set_stage(
            app,
            state,
            Stage::Downloading {
                step,
                steps,
                done: 0,
                total: 0,
            },
        );

        docker::pull(image, &state.shutting_down, |progress| {
            set_stage(
                app,
                state,
                Stage::Downloading {
                    step,
                    steps,
                    done: progress.done,
                    total: progress.total,
                },
            );
        })?;
    }

    drop_legacy_sandbox_tag();
    record_image_identity(&server, if steps > 0 { "pulled" } else { "already present" });
    Ok(())
}

/// Removes the second name earlier versions gave the sandbox image.
///
/// They pulled `omaraflak/kayak-sandbox` and then tagged it `kayak-sandbox`,
/// because the server defaulted to the unqualified name. That is one image with
/// two tags rather than two images, but `docker images` lists it twice and it
/// reads as Kayak having installed the same thing twice. The server is told the
/// published name directly now, so the extra tag has no purpose.
///
/// Failure is ignored: the tag is absent on a fresh install, and a machine that
/// still has a container referencing it is one where removing it would be wrong
/// anyway.
fn drop_legacy_sandbox_tag() {
    if docker::has_image(config::LEGACY_SANDBOX_TAG) {
        let _ = docker::remove_image(config::LEGACY_SANDBOX_TAG);
        logs::record("image", "removed the duplicate kayak-sandbox tag");
    }
}

/// Records exactly which image the launcher ended up with.
///
/// Without this a report of "it is still the old version" cannot be told apart
/// from "the tag did not move", "the pull was served something stale", or "the
/// launcher never pulled at all" -- the three have the same symptom and
/// completely different causes. The digest identifies the image regardless of
/// what the tag currently points at.
fn record_image_identity(image: &str, how: &str) {
    let version = docker::image_version(image).unwrap_or_else(|| "unlabelled".to_string());
    let digest = docker::image_digest(image).unwrap_or_else(|| "no repository digest".to_string());
    let id = docker::image_id(image).unwrap_or_else(|| "unknown".to_string());
    logs::record(
        "image",
        &format!("{image} {how}: version {version}, digest {digest}, id {id}"),
    );
}

/// Brings Kayak up, from a cold machine to a served UI.
fn boot(app: AppHandle, state: Arc<Launcher>) {
    let Some(_guard) = BusyGuard::acquire(&state) else {
        return;
    };
    if stopping(&state) {
        return;
    }

    logs::record("boot", "starting up");
    set_stage(&app, &state, Stage::Checking);

    if !docker::is_installed() {
        logs::record("boot", "docker CLI not found on this machine");
        show_docker_missing(&app, &state);
        return;
    }

    if !ensure_docker(&app, &state) {
        return;
    }

    // A server that stopped right after starting is most often a broken
    // release, and the fix for one is the next release. Without this, Try
    // again started the same broken version forever.
    let mut superseded = Vec::new();
    if lock(&state.last_problem).take() == Some(Problem::ServerExited) {
        match fetch_newer_kayak(&app, &state) {
            Ok(replaced) => superseded = replaced,
            Err(error) => {
                if !stopping(&state) {
                    fail(&app, &state, &error);
                }
                return;
            }
        }
    }

    let port = match start_server(&app, &state) {
        Ok(port) => port,
        Err(error) => {
            if !stopping(&state) {
                fail(&app, &state, &error);
            }
            return;
        }
    };
    *lock(&state.port) = Some(port);
    logs::record("boot", &format!("kayak is serving on port {port}"));
    remove_superseded(&superseded);

    if !present_kayak(&app, &state, port) {
        return;
    }

    // The check made when the launcher opened could not read the installed
    // version if Docker Desktop was not running yet, which left the About panel
    // and the update banner a check behind.
    if lock(&state.update).installed.is_none() {
        thread::spawn(move || check_for_updates(app, state));
    }
}

/// Makes sure Docker Desktop is answering, opening it if it is not running.
///
/// Opened automatically: asking someone who has never heard of Docker to click
/// a button that starts it was a step that only made sense to people who
/// already knew what it was. Returns false when boot cannot carry on, having
/// already put the reason on screen.
fn ensure_docker(app: &AppHandle, state: &Launcher) -> bool {
    let mut daemon = docker::daemon();
    match daemon {
        Daemon::Running => return true,
        Daemon::Unresponsive => logs::record(
            "boot",
            "docker desktop is not answering; waiting in case it is still starting",
        ),
        Daemon::Stopped => {
            logs::record("boot", "docker desktop is not running; opening it");
            match docker::open_desktop() {
                Ok(DesktopLaunch::Opened) => {}
                Ok(DesktopLaunch::NotInstalled) => {
                    show_docker_missing(app, state);
                    return false;
                }
                Err(error) => {
                    fail(app, state, &error);
                    return false;
                }
            }
        }
    }

    set_stage(app, state, Stage::WaitingForDocker);
    let started = Instant::now();
    let mut nudged = false;
    while started.elapsed() < DOCKER_START_LIMIT {
        thread::sleep(Duration::from_secs(2));
        if stopping(state) {
            return false;
        }
        daemon = docker::daemon();
        if daemon == Daemon::Running {
            logs::record("boot", "docker desktop is running");
            return true;
        }
        // Docker Desktop says nothing while it starts, and can quietly fail to
        // start at all. After a while the user is offered the button to open it
        // themselves -- which also brings forward a Docker Desktop that is
        // waiting on a question nobody has seen.
        if !nudged && daemon == Daemon::Stopped && started.elapsed() >= DOCKER_HINT_AFTER {
            nudged = true;
            set_stage(app, state, Stage::DockerAsleep { closed: false });
        }
    }

    fail(
        app,
        state,
        if daemon == Daemon::Unresponsive {
            explain::DOCKER_UNRESPONSIVE
        } else {
            explain::DOCKER_SLOW_TO_START
        },
    );
    false
}

/// Downloads the published Kayak when it differs from the installed one,
/// returning the images it replaced so they can be removed once Kayak runs.
///
/// Docker Hub being unreachable is not an error here: the retry can still go
/// ahead with what is installed.
fn fetch_newer_kayak(app: &AppHandle, state: &Launcher) -> Result<Vec<String>, String> {
    let installed = docker::image_digest(&config::server_image());
    let remote = match registry::latest_tag(config::SERVER_REPO, config::TAG) {
        Ok(remote) => remote,
        Err(error) => {
            logs::record("boot", &format!("could not check for a newer Kayak: {error}"));
            return Ok(Vec::new());
        }
    };
    if installed.as_deref() == Some(remote.digest.as_str()) {
        logs::record("boot", "the installed Kayak is the published one");
        return Ok(Vec::new());
    }

    logs::record("boot", "a newer Kayak is published; downloading it before trying again");
    let previous = Previous::note();
    ensure_images(app, state, true)?;
    Ok(previous.ids())
}

/// Starts the host side of Kayak's GPU support, once per launch.
///
/// Waits for Kayak to be serving because the control channel lives in the data
/// directory, which is not seeded until the container has run.
fn connect_gpu_bridge(state: &Arc<Launcher>) {
    if state.bridge_started.swap(true, Ordering::SeqCst) {
        return;
    }
    let Ok(data_dir) = paths::ensure_data_dir() else {
        return;
    };
    let control = control::ControlPaths::under(&data_dir);
    let _ = std::fs::create_dir_all(&control.dir);
    // Mirrored into the directory Kayak already shares, which is how a
    // support dump assembled inside the container can include host-side
    // records it could not otherwise see.
    logs::mirror_to(&control.dir.join("launcher.log"));
    if let Err(error) = control::reset_desired(&control.desired) {
        logs::record("metal", &format!("could not reset the requested model: {error}"));
    }
    spawn_metal_reconciler(state.clone(), control);
}

/// Reuses a running container, or creates one, and waits until it serves.
/// Returns the port it serves on.
fn start_server(app: &AppHandle, state: &Launcher) -> Result<u16, String> {
    let data_dir = paths::ensure_data_dir()?;
    let recreate = state.recreate_next.swap(false, Ordering::SeqCst);

    // A container left running by a previous launch is reused, because
    // recreating it would drop whatever agents are mid-run inside it -- but only
    // when it came from the image that is current now, and only while it is
    // actually answering.
    if recreate {
        logs::record("boot", "replacing the server that stopped answering");
    } else if docker::container_state(config::CONTAINER_NAME)? == ContainerState::Running {
        let running_from = docker::container_image_id(config::CONTAINER_NAME);
        let current = docker::image_id(&config::server_image());

        logs::record(
            "boot",
            &format!(
                "a container is already running (built from {}, current image is {})",
                running_from.as_deref().unwrap_or("unknown"),
                current.as_deref().unwrap_or("absent"),
            ),
        );

        // A server that should require the access key and was started without it
        // -- by a launcher from before keys -- is replaced, or it stays open to
        // every container until the next update.
        let missing_key = server_key(&config::server_image()).is_some_and(|key| {
            !docker::container_has_env(config::CONTAINER_NAME, access::ENV_NAME, &key)
        });
        if missing_key {
            logs::record("boot", "the running server does not require the access key; replacing it");
        } else if should_reuse(running_from.as_deref(), current.as_deref()) {
            if let Some(port) = docker::published_port(config::CONTAINER_NAME) {
                set_stage(
                    app,
                    state,
                    Stage::Starting {
                        headline: "Reconnecting to Kayak".to_string(),
                        hint: "This usually takes a few seconds.".to_string(),
                    },
                );
                match wait_for_health(port, Duration::from_secs(config::HEALTH_TIMEOUT_SECS), state)
                {
                    Ok(()) => return Ok(port),
                    // A server that was already running and still does not
                    // answer is hung, not starting; waiting on it again would
                    // only repeat the wait. It is replaced instead.
                    Err(error)
                        if matches!(
                            explain::classify(&error),
                            Problem::HealthTimeout | Problem::ServerExited
                        ) =>
                    {
                        logs::record("boot", &format!("the running server is unusable: {error}"));
                    }
                    Err(error) => return Err(error),
                }
            }
        } else {
            logs::record(
                "boot",
                "the running container was built from an older image; replacing it",
            );
        }
    }

    ensure_not_stopping(state)?;
    ensure_images(app, state, false)?;
    ensure_not_stopping(state)?;

    show_step(app, state, STARTING_KAYAK, "This can take a minute the first time.");

    // A container may have been created with a port that is now taken, or from
    // an image that has since been replaced, so it is rebuilt rather than
    // restarted. All state lives in the mounted data directory, so nothing is
    // lost by doing so -- provided it is stopped rather than killed, since the
    // server owns a SQLite database.
    docker::stop_container(config::CONTAINER_NAME)?;
    docker::remove_container(config::CONTAINER_NAME)?;

    let port = launch_container(app, &config::server_image(), &data_dir, state)?;
    wait_for_health(port, Duration::from_secs(config::HEALTH_TIMEOUT_SECS), state)?;
    Ok(port)
}

const STARTING_KAYAK: &str = "Starting Kayak";

/// Says what a start is doing, in the launcher window -- unless Kayak's own
/// window is up, where an update is already showing its progress.
fn show_step(app: &AppHandle, state: &Launcher, headline: &str, hint: &str) {
    if kayak_window_visible(app) {
        return;
    }
    set_stage(
        app,
        state,
        Stage::Starting {
            headline: headline.to_string(),
            hint: hint.to_string(),
        },
    );
}

/// Copies the defaults `image` ships into the data directory, if it is empty.
///
/// Must happen before the container runs: the bind mount hides whatever the
/// image ships at /app/data, so the defaults are lifted out of the image first
/// and become the initial contents of the user's own data directory. Copied
/// into a folder beside it first and then moved in, so an interrupted copy
/// cannot leave a half-seeded directory that would never be seeded again.
fn seed_if_empty(app: &AppHandle, state: &Launcher, image: &str, data_dir: &Path) -> Result<(), String> {
    if !paths::is_unseeded(data_dir) {
        return Ok(());
    }
    show_step(app, state, "Setting up your workspace", "This happens once.");
    logs::record("boot", "seeding the data directory from the image");

    let staging = paths::seed_staging_dir(data_dir);
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging)
        .map_err(|err| format!("Could not create {}: {err}", staging.display()))?;
    docker::copy_out_of_image(image, "/app/data", &staging)?;
    paths::adopt_seed(&staging, data_dir)?;

    show_step(app, state, STARTING_KAYAK, "This can take a minute the first time.");
    Ok(())
}

/// Creates and starts the server container, returning the port it was given.
///
/// Every way Kayak is started goes through here -- a normal start, an update,
/// and putting the previous version back after a failed one -- so this is where
/// an empty data directory is seeded and where quitting is checked for last.
///
/// A free port is checked by binding it and letting go, so it can be taken in
/// the moment before Docker binds it; Docker Desktop also holds ports a bind
/// from here cannot see. On a clash the next port is tried rather than failing
/// the start.
fn launch_container(
    app: &AppHandle,
    image: &str,
    data_dir: &Path,
    state: &Launcher,
) -> Result<u16, String> {
    seed_if_empty(app, state, image, data_dir)?;

    let key = server_key(image);
    if key.is_some() {
        logs::record("boot", "the server will require the access key");
    }

    let mut from = config::PREFERRED_PORT;
    let mut attempt = 0;
    let port = loop {
        let port = docker::find_free_port(from, config::PORT_SCAN_RANGE)?;
        // Checked immediately before every `docker run`: a container created
        // after the shutdown has stopped the old one is one nothing will stop.
        ensure_not_stopping(state)?;
        let launched = docker::run_container(&docker::RunSpec {
            image,
            name: config::CONTAINER_NAME,
            port,
            data_dir,
            access_key: key.as_deref(),
        });
        match launched {
            Ok(()) => break port,
            Err(error) if docker::is_port_clash(&error) && attempt < PORT_ATTEMPTS => {
                logs::record("boot", &format!("port {port} was taken after all ({error}); trying another"));
                // The failed run still created the container, which holds the name.
                docker::remove_container(config::CONTAINER_NAME)?;
                from = port.saturating_add(1);
                attempt += 1;
            }
            Err(error) => return Err(error),
        }
    };

    // Quitting may still have begun while `docker run` was under way, after the
    // shutdown had already stopped the one that existed. This one is stopped
    // here, or nothing would.
    if stopping(state) {
        let _ = docker::stop_container(config::CONTAINER_NAME);
        let _ = docker::remove_container(config::CONTAINER_NAME);
        return Err(explain::SHUTTING_DOWN.to_string());
    }
    Ok(port)
}

/// Decides whether a running container can serve the current image.
///
/// A container records the image ID it was created from, not the tag, so a tag
/// that has since moved leaves the two different. Without this check the
/// launcher reused whatever was already running forever: the container kept the
/// old image alive, Docker then refused to delete that image because a container
/// referenced it, and a user who deleted everything and started again was handed
/// the same old version with nothing to explain why.
///
/// Reuse only on a definite match. An unknown ID on either side means the safe
/// answer is to rebuild, which costs a few seconds.
fn should_reuse(container_image: Option<&str>, current_image: Option<&str>) -> bool {
    match (container_image, current_image) {
        (Some(running), Some(current)) => running == current,
        _ => false,
    }
}

/// A token unique to this moment, for addresses no cache can hold.
fn fresh_token() -> String {
    // The counter makes consecutive calls distinct even within one clock
    // millisecond, so no two navigations can ever share a cache entry.
    static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis())
        .unwrap_or(0);
    format!("{millis}-{sequence}")
}

/// A copy of `url` that no cache can already hold an entry for.
///
/// Navigations go through the webview's HTTP cache, and an index.html cached
/// before Kayak sent cache headers stays "fresh" by heuristic for a long time
/// -- long enough that opening or updating the app showed a previous release's
/// UI without the webview ever asking the running server. A query string
/// unique to this moment cannot match any stored entry, so the app shell is
/// always fetched from the server that is actually running.
fn cache_busted(url: &str) -> String {
    format!("{url}/?fresh={}", fresh_token())
}

/// The page the window is on, at an address no cache can hold.
///
/// For reloading in place: the user stays on the page they were on, and only
/// the cache-busting query changes.
fn refreshed(url: &Url) -> Url {
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key != "fresh")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let mut fresh = url.clone();
    fresh
        .query_pairs_mut()
        .clear()
        .extend_pairs(kept)
        .append_pair("fresh", &fresh_token());
    fresh
}

/// Asks where to save a file Kayak offered for download, then saves it there.
///
/// With no handler registered, the webview cancels a `download` link outright, so
/// pressing Download in Kayak did nothing at all. The webview's own download is
/// declined and the file fetched here instead: WebKit wants the destination decided
/// synchronously, inside a callback on the main thread, which is exactly where a
/// Save panel cannot be shown without stalling the page behind it.
fn handle_download(app: &AppHandle, event: DownloadEvent<'_>) -> bool {
    let DownloadEvent::Requested { url, destination } = event else {
        return true;
    };

    // A blob: or data: URL exists only inside the page, so only the webview can
    // save it; it lands in Downloads under the name the page offered.
    if !matches!(url.scheme(), "http" | "https") {
        logs::record("download", &format!("saving a {} url to Downloads", url.scheme()));
        return true;
    }

    let suggested = destination
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".to_string());
    let mut dialog = app.dialog().file().set_file_name(suggested);
    if let Some(folder) = destination.parent() {
        dialog = dialog.set_directory(folder);
    }
    // Attached to the Kayak window as a sheet, the way a Mac app asks.
    if let Some(window) = app.get_webview_window(KAYAK_WINDOW) {
        dialog = dialog.set_parent(&window);
    }

    let app = app.clone();
    let url = url.to_string();
    dialog.save_file(move |chosen| {
        let Some(path) = chosen.and_then(|chosen| chosen.into_path().ok()) else {
            return; // Cancelled.
        };
        thread::spawn(move || match save_url(&url, &path, access::key().as_deref()) {
            Ok(()) => logs::record("download", &format!("saved {url} to {}", path.display())),
            Err(error) => {
                // The reason stays in the log; the dialog says what to do.
                logs::record("download", &error);
                app.dialog()
                    .message("Check that there's enough free space and try again.")
                    .title("Couldn't save the file")
                    .kind(MessageDialogKind::Error)
                    .show(|_| {});
            }
        });
    });
    false
}

/// Fetches a URL into a file, leaving nothing behind if it fails part-way.
///
/// `key` is sent as the access key when the file is on Kayak's own server: it
/// is fetched here rather than by the window, so the session the window holds
/// is not sent along, and the key stands in for it.
fn save_url(url: &str, path: &Path, key: Option<&str>) -> Result<(), String> {
    let mut request = ureq::get(url);
    if let (Some(key), true) = (key, is_loopback_url(url)) {
        request = request.set(access::HEADER_NAME, key);
    }
    let response = request
        .call()
        .map_err(|error| format!("Could not download the file: {error}"))?;
    let written = std::fs::File::create(path).and_then(|mut file| {
        std::io::copy(&mut response.into_reader(), &mut file).map(|_| ())
    });
    written.map_err(|error| {
        let _ = std::fs::remove_file(path);
        format!("Could not write {}: {error}", path.display())
    })
}

/// The access key a server started from `image` should require, if any.
///
/// None when this Mac has no key, or when the image predates keys: its server
/// could not sign the window in, and requiring a key it cannot hand out would
/// lock the user out of Kayak.
fn server_key(image: &str) -> Option<String> {
    let key = access::key()?;
    access::image_supports(docker::image_version(image).as_deref()).then_some(key)
}

/// Whether a URL points at this Mac, which is where Kayak's server is.
fn is_loopback_url(url: &str) -> bool {
    url.parse::<Url>()
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .is_some_and(|host| matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]"))
}

/// Puts Kayak on screen, served from `port`, and steps the launcher aside.
/// Returns false when the window could not be shown, having said so.
///
/// Every path that brings Kayak up ends here, so this is also where the GPU
/// control channel is connected. Done only on a normal start, an update
/// started from the launcher brought Kayak up with nothing answering its GPU
/// requests, and every GPU model failed.
fn present_kayak(app: &AppHandle, state: &Arc<Launcher>, port: u16) -> bool {
    connect_gpu_bridge(state);
    set_stage(app, state, Stage::Ready);
    // Signed in only when the server actually holds the key: one that does not
    // has no sign-in address, and sending the window there would show an error.
    let key = access::key().filter(|key| {
        docker::container_has_env(config::CONTAINER_NAME, access::ENV_NAME, key)
    });
    if let Err(error) = show_kayak(app, &format!("http://127.0.0.1:{port}"), key.as_deref()) {
        fail(app, state, &format!("{}: {error}", explain::WINDOW_FAILED));
        return false;
    }
    // The launcher window has done its job; from here the update banner inside
    // the Kayak window is the only launcher surface the user needs.
    if let Some(window) = app.get_webview_window(LAUNCHER_WINDOW) {
        let _ = window.hide();
    }
    true
}

/// Opens (or focuses) the window that shows Kayak itself.
///
/// A window that already exists is pointed at `url` again rather than just
/// shown: it was hidden because Kayak stopped, so the page it holds belongs to
/// a server that is gone, and the server that replaced it may be on another
/// port.
fn show_kayak(app: &AppHandle, url: &str, key: Option<&str>) -> Result<(), String> {
    let page = cache_busted(url);
    let parsed: Url = match key {
        // The server signs the window in, then sends it on to the page.
        Some(key) => access::entry_url(url, &page[url.len()..], key)?,
        None => page.parse().map_err(|err| format!("Could not parse {url}: {err}"))?,
    };

    if let Some(window) = app.get_webview_window(KAYAK_WINDOW) {
        window.navigate(parsed).map_err(|err| format!("{err}"))?;
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }

    let navigation_handle = app.clone();
    let close_handle = app.clone();
    let download_handle = app.clone();

    WebviewWindowBuilder::new(app, KAYAK_WINDOW, WebviewUrl::External(parsed))
        .title("Kayak")
        .inner_size(1280.0, 860.0)
        .min_inner_size(900.0, 600.0)
        .center()
        .initialization_script(BANNER_JS)
        .on_download(move |_webview, event| handle_download(&download_handle, event))
        .on_navigation(move |url| {
            let Some(action) = url.path().strip_prefix(ACTION_PREFIX) else {
                // A scheme this window cannot render, such as mailto:. Kayak
                // sets `location.href` for these, which is not a click and so
                // never reaches the page's own link handling; left alone the
                // webview simply refuses to go there and nothing happens.
                if !matches!(url.scheme(), "http" | "https") {
                    open_externally(&navigation_handle, url.as_str());
                    return false;
                }
                return true;
            };
            // The query carries an argument for actions that need one, such as
            // the URL to hand to the operating system.
            let query: std::collections::HashMap<String, String> = url
                .query_pairs()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect();
            handle_banner_action(&navigation_handle, action.to_string(), query);
            // Cancelled: these paths are a message channel, not a real page.
            false
        })
        .build()
        .map_err(|err| format!("{err}"))?;

    if let Some(window) = app.get_webview_window(KAYAK_WINDOW) {
        window.on_window_event(move |event| {
            // Closing Kayak quits, rather than leaving a hidden launcher window
            // and an orphaned container behind. The close is held off while the
            // container stops so the window can say what is happening.
            if let WindowEvent::CloseRequested { api, .. } = event {
                let Some(state) = close_handle.try_state::<Arc<Launcher>>() else {
                    return;
                };
                api.prevent_close();
                begin_shutdown(close_handle.clone(), state.inner().clone());
            }
        });
    }

    Ok(())
}

/// Closing the launcher window quits, the same as closing Kayak's.
///
/// The launcher is the only window on screen whenever Kayak cannot be used --
/// and then the Kayak window is hidden behind it rather than closed -- so
/// closing it is the user saying they are done. Left to the default it simply
/// vanished, while the hidden window kept the app, and Kayak, running.
fn watch_launcher_window(app: &AppHandle) {
    let Some(window) = app.get_webview_window(LAUNCHER_WINDOW) else {
        return;
    };
    let handle = app.clone();
    window.on_window_event(move |event| {
        if let WindowEvent::CloseRequested { api, .. } = event {
            let Some(state) = handle.try_state::<Arc<Launcher>>() else {
                return;
            };
            api.prevent_close();
            begin_shutdown(handle.clone(), state.inner().clone());
        }
    });
}

/// Reloads the Kayak window in place, from the server rather than a cache.
fn reload_kayak(app: &AppHandle) {
    let Some(window) = app.get_webview_window(KAYAK_WINDOW) else {
        return;
    };
    logs::record("window", "reloading Kayak");
    match window.url() {
        Ok(current) => {
            let _ = window.navigate(refreshed(&current));
        }
        Err(_) => {
            let _ = window.reload();
        }
    }
}

/// Brings back a page whose web content process ended.
///
/// WebKit runs each page in a process of its own, which macOS can end under
/// memory pressure, leaving a blank window with nothing to click. The Kayak
/// page is reloaded where it was, at a fresh address so the app shell comes
/// from the running server rather than a cache.
fn reload_after_crash(webview: &tauri::Webview) {
    logs::record(
        "window",
        &format!("the {} page's content process ended; reloading it", webview.label()),
    );
    match webview.url() {
        Ok(current) if webview.label() == KAYAK_WINDOW => {
            let _ = webview.navigate(refreshed(&current));
        }
        _ => {
            let _ = webview.reload();
        }
    }
}

/// Runs a JS snippet inside the Kayak window, if it is open.
fn eval_in_kayak(app: &AppHandle, script: &str) {
    if let Some(window) = app.get_webview_window(KAYAK_WINDOW) {
        let _ = window.eval(script);
    }
}

/// Encodes a value as a JS literal, so user-visible text cannot break the
/// snippet it is interpolated into.
fn js_literal<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

fn show_banner_progress(app: &AppHandle, message: &str, percent: i32) {
    eval_in_kayak(
        app,
        &format!(
            "window.__kayakLauncher && window.__kayakLauncher.showProgress({}, {percent})",
            js_literal(&message)
        ),
    );
}

/// Shows update progress wherever the user can see it: over Kayak when its
/// window is up, otherwise in the launcher's own window. An update started from
/// the launcher -- because Kayak could not start -- used to report its progress
/// into a window that did not exist.
fn report_update(app: &AppHandle, state: &Launcher, message: &str, percent: i32) {
    if kayak_window_visible(app) {
        show_banner_progress(app, message, percent);
    } else {
        set_stage(
            app,
            state,
            Stage::Updating {
                message: message.to_string(),
                percent,
            },
        );
    }
}

/// Builds the application menu, with both versions in the About panel.
///
/// Two separate things are installed on a user's machine and either can be out
/// of date independently: this launcher, which updates through GitHub, and the
/// Kayak server image, which updates through Docker Hub. The About panel showed
/// only the launcher's version, which is the less interesting of the two --
/// Kayak is what the user actually works in.
///
/// The menu is rebuilt rather than mutated because the Kayak version is not
/// known at startup: it is read from the image label once the container is up.
fn build_menu(app: &AppHandle, kayak_version: Option<&str>) -> tauri::Result<Menu<tauri::Wry>> {
    let about = AboutMetadata {
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
        comments: Some(match kayak_version {
            Some(version) => format!("Kayak {version}"),
            None => "Kayak is not running yet".to_string(),
        }),
        ..Default::default()
    };

    let application = SubmenuBuilder::new(app, "Kayak")
        .about(Some(about))
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit()
        .build()?;

    // Rebuilt explicitly because replacing the menu drops the defaults, and
    // without these the Kayak window loses copy and paste entirely.
    let edit = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;

    // A page that went blank or stuck had no way back short of quitting.
    let reload = MenuItemBuilder::with_id(MENU_RELOAD, "Reload")
        .accelerator("CmdOrCtrl+R")
        .build(app)?;
    let view = SubmenuBuilder::new(app, "View").item(&reload).build()?;

    let window = SubmenuBuilder::new(app, "Window")
        .minimize()
        .separator()
        .close_window()
        .build()?;

    // The log is the record of what went wrong; without this, finding it
    // needed a terminal or knowing where macOS keeps logs.
    let show_log = MenuItemBuilder::with_id(MENU_SHOW_LOG, "Show Kayak Log").build(app)?;
    let help = SubmenuBuilder::new(app, "Help").item(&show_log).build()?;

    MenuBuilder::new(app)
        .items(&[&application, &edit, &view, &window, &help])
        .build()
}

/// Puts the Kayak version into the About panel once it is known.
fn refresh_menu(app: &AppHandle, kayak_version: Option<&str>) {
    if let Ok(menu) = build_menu(app, kayak_version) {
        let _ = app.set_menu(menu);
    }
}

fn handle_menu(app: &AppHandle, id: &str) {
    match id {
        MENU_RELOAD => reload_kayak(app),
        MENU_SHOW_LOG => {
            let Some(path) = logs::launcher_log_path() else {
                return;
            };
            if let Err(error) = app.opener().reveal_item_in_dir(&path) {
                logs::record("open", &format!("could not show the log: {error}"));
            }
        }
        _ => {}
    }
}

/// Draws whatever the user should currently be told, into the Kayak window.
///
/// Called both when something changes and when the page announces itself, so a
/// banner survives a reload and no longer depends on the webview happening to
/// have a document at the moment a check finished.
///
/// One banner regardless of what is pending. Two things can be out of date —
/// the Kayak image and this launcher — but that is plumbing the user should
/// not have to know about: showing two different banners with two different
/// buttons meant updating twice in a row. The single button applies both.
fn push_banner_state(app: &AppHandle, state: &Launcher) {
    if let Some(notice) = lock(&state.notice).clone() {
        eval_in_kayak(
            app,
            &format!(
                "window.__kayakLauncher && window.__kayakLauncher.showError({})",
                js_literal(&notice)
            ),
        );
        return;
    }

    let update = lock(&state.update).clone();
    let restarts = lock(&state.launcher_update).is_some();
    if update.available || restarts {
        eval_in_kayak(
            app,
            &format!(
                "window.__kayakLauncher && window.__kayakLauncher.showUpdate({{date: {}, restarts: {}}})",
                js_literal(&update.date),
                restarts,
            ),
        );
    }
}

/// Tells the user an update did not go through: over Kayak when it is on
/// screen, otherwise on the launcher's failure screen.
fn report_update_failure(app: &AppHandle, state: &Launcher, message: String, dismiss: &str) {
    if kayak_window_visible(app) {
        *lock(&state.notice) = Some(Notice {
            title: explain::UPDATE_FAILED.to_string(),
            message,
            dismiss: dismiss.to_string(),
        });
        push_banner_state(app, state);
    } else {
        set_stage(
            app,
            state,
            Stage::Failed {
                headline: explain::UPDATE_FAILED.to_string(),
                body: message,
            },
        );
        surface_launcher(app);
    }
}

/// The updater for this app, configured for this Mac.
///
/// The Intel build running on an Apple chip is pointed at the Apple-chip build,
/// and offered it even at the same version: it cannot use the graphics chip,
/// and its users have no other way of finding out they downloaded the wrong
/// one. Once installed, the new build runs natively and this no longer applies.
fn launcher_updater(app: &AppHandle) -> Result<tauri_plugin_updater::Updater, String> {
    let mut builder = app.updater_builder().timeout(UPDATE_CHECK_TIMEOUT);
    if metal::silicon() == metal::Silicon::Translated {
        builder = builder
            .target("darwin-aarch64")
            .version_comparator(|current, release| release.version >= current);
    }
    builder.build().map_err(|error| error.to_string())
}

/// Asks GitHub whether a newer launcher has been published.
///
/// Runs in Rust rather than the launcher window's page because that window is
/// hidden once Kayak opens, so an update found there would never be seen.
fn check_launcher_update(app: AppHandle, state: Arc<Launcher>) {
    let updater = match launcher_updater(&app) {
        Ok(updater) => updater,
        Err(error) => {
            logs::record("update", &format!("could not set up the app updater: {error}"));
            return;
        }
    };
    let found = match tauri::async_runtime::block_on(updater.check()) {
        Ok(found) => found,
        // Offline, or no release published yet. Neither is worth showing.
        Err(error) => {
            logs::record("update", &format!("could not check for a new app: {error}"));
            return;
        }
    };
    let Some(update) = found else {
        return;
    };

    let version = update.version.clone();
    logs::record("update", &format!("app version {version} is available"));
    *lock(&state.launcher_update) = Some(update);
    let _ = app.emit("launcher-update", version);
    push_banner_state(&app, &state);
}

/// Applies everything that is pending: the Kayak images first, then the
/// launcher itself.
///
/// This order is the only one that works unattended. Installing the launcher
/// restarts the app, so doing it first would abandon a Kayak update halfway
/// and greet the user with a second banner after the restart — the exact
/// two-step experience the single banner exists to avoid.
fn apply_updates(app: AppHandle, state: Arc<Launcher>) {
    let Some(_guard) = BusyGuard::acquire(&state) else {
        return;
    };
    if stopping(&state) {
        return;
    }
    lock(&state.notice).take();

    let kayak_pending = lock(&state.update).available;
    if kayak_pending && !update_kayak_image(&app, &state) {
        // The failure has its own message and retry; restarting the launcher
        // on top of it would wipe the explanation away.
        return;
    }
    apply_launcher_update(&app, &state, kayak_pending);
}

/// Downloads and installs the launcher update, then restarts into it.
/// Does nothing when no launcher update is pending.
///
/// The pending update is cloned rather than taken: taking it meant a failed
/// download lost it, and Try again then had nothing left to install.
fn apply_launcher_update(app: &AppHandle, state: &Arc<Launcher>, after_kayak: bool) {
    let Some(update) = lock(&state.launcher_update).clone() else {
        return;
    };
    if stopping(state) {
        return;
    }

    let label = if after_kayak {
        "Finishing the update"
    } else {
        "Downloading the update"
    };
    report_update(app, state, label, -1);
    match download_launcher_update(app, state, &update, label) {
        Ok(()) => {
            lock(&state.launcher_update).take();
            logs::record("update", "the new app is installed; restarting");
            report_update(app, state, "Restarting Kayak…", 100);
            // Everything this process started is stopped first: the restart
            // replaces the process, an orphaned container would refuse the
            // name on the way back up, and an orphaned GPU server would keep
            // its model in memory with nothing left to stop it.
            stop_everything(state);
            app.restart();
        }
        Err(error) => {
            logs::record("update", &format!("the app could not be updated: {error}"));
            if stopping(state) {
                return;
            }
            report_update_failure(
                app,
                state,
                explain::launcher_update_failure(&error).to_string(),
                "Not now",
            );
        }
    }
}

/// Downloads and installs the app's update, abandoning it if the download
/// stalls or the app begins quitting.
fn download_launcher_update(
    app: &AppHandle,
    state: &Launcher,
    update: &tauri_plugin_updater::Update,
    label: &str,
) -> Result<(), String> {
    // When a chunk last arrived, how much has arrived, and the expected size.
    let received = Arc::new(Mutex::new((Instant::now(), 0u64, None::<u64>)));
    let task = {
        let update = update.clone();
        let received = received.clone();
        tauri::async_runtime::spawn(async move {
            update
                .download_and_install(
                    move |chunk, total| {
                        let mut seen = lock(&received);
                        seen.0 = Instant::now();
                        seen.1 += chunk as u64;
                        seen.2 = total;
                    },
                    || {},
                )
                .await
        })
    };

    while !task.inner().is_finished() {
        let (last, bytes, total) = *lock(&received);
        if stopping(state) {
            task.abort();
            return Err(explain::SHUTTING_DOWN.to_string());
        }
        if last.elapsed() >= LAUNCHER_DOWNLOAD_STALL {
            task.abort();
            return Err(format!(
                "the download received nothing for {} seconds",
                LAUNCHER_DOWNLOAD_STALL.as_secs()
            ));
        }
        let percent = total
            .filter(|total| *total > 0)
            .map(|total| (bytes.saturating_mul(100) / total).min(100) as i32)
            .unwrap_or(-1);
        report_update(app, state, label, percent);
        thread::sleep(Duration::from_millis(500));
    }

    match tauri::async_runtime::block_on(task) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(error.to_string()),
        Err(error) => Err(error.to_string()),
    }
}

/// Stops everything the launcher started, then quits.
///
/// Run on its own thread with the window still up: `docker stop` gives the
/// server time to close its database, and doing that on the main thread froze
/// the window for the duration, which reads as a crash rather than a shutdown.
///
/// A start or an update under way is given a moment to notice and stop between
/// steps, so the shutdown does not stop a container that the start then
/// recreates. And whatever happens, the app exits within [`SHUTDOWN_LIMIT`].
fn begin_shutdown(app: AppHandle, state: Arc<Launcher>) {
    if state.shutting_down.swap(true, Ordering::SeqCst) {
        return;
    }
    logs::record("shutdown", "quitting");
    eval_in_kayak(&app, "window.__kayakLauncher && window.__kayakLauncher.showShutdown()");
    set_stage(&app, &state, Stage::Stopping);

    {
        let app = app.clone();
        thread::spawn(move || {
            thread::sleep(QUIT_DEADLINE);
            logs::record("shutdown", "stopping took too long; quitting anyway");
            app.exit(0);
        });
    }

    thread::spawn(move || {
        let began = Instant::now();
        wait_while_busy(&state, began + BUSY_GRACE);
        stop_everything(&state);

        // A start or an update can be inside a Docker call that began before
        // quitting did -- `docker run` is allowed two minutes -- and the server
        // such a call creates is one the stop above never saw. Each start
        // removes what it made once it notices the app is quitting, which is
        // what the wait is for; whatever is still there afterwards is removed
        // here, before the app is gone and nothing could.
        if state.busy.load(Ordering::SeqCst) {
            wait_while_busy(&state, began + SHUTDOWN_LIMIT);
            logs::record("shutdown", "making sure no server was started while quitting");
            let _ = docker::stop_container(config::CONTAINER_NAME);
            let _ = docker::remove_container(config::CONTAINER_NAME);
        }
        app.exit(0);
    });
}

fn wait_while_busy(state: &Launcher, deadline: Instant) {
    while state.busy.load(Ordering::SeqCst) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(100));
    }
}

/// Stops everything Kayak started, so that nothing outlives it.
///
/// The one place quitting, restarting into an app update, and the last-resort
/// exit hook all go through. The first caller does the work; the rest return
/// at once.
fn stop_everything(state: &Launcher) {
    state.shutting_down.store(true, Ordering::SeqCst);
    if state.stopped.swap(true, Ordering::SeqCst) {
        return;
    }
    logs::record("shutdown", "stopping Kayak and anything it started");

    // In parallel: each can take a good part of the time quitting is allowed,
    // and most of it is spent waiting rather than working.
    thread::scope(|scope| {
        scope.spawn(|| {
            stop_gpu(state);
            write_stopped_status(state);
        });
        let _ = docker::stop_container(config::CONTAINER_NAME);
        let _ = docker::remove_container(config::CONTAINER_NAME);
    });
    logs::record("shutdown", "everything is stopped");
}

/// Stops the GPU installer and server, along with everything they started.
///
/// Waits, through the server's lock, for any stop the reconcile loop has under
/// way to finish, rather than letting the app exit partway through it.
fn stop_gpu(state: &Launcher) {
    if let Some(installer) = lock(&state.installer).take() {
        logs::record("shutdown", "stopping the GPU installer");
        installer.terminate(Duration::from_secs(5), || {});
    }
    stop_metal_server(state);
}

/// Stops the GPU server, if one is running, while holding the lock it is kept
/// under.
///
/// Holding the lock for the whole stop -- the grace period and the kill that
/// may follow it -- is the point. Taking the server out first and stopping it
/// afterwards left quitting finding nothing to wait for, so the app could exit
/// halfway through the grace period, losing the kill and leaving the server's
/// PID file behind.
fn stop_metal_server(state: &Launcher) {
    let mut running = lock(&state.metal);
    if let Some(server) = running.as_mut() {
        server.stop();
    }
    *running = None;
}

/// Schemes the launcher is willing to hand to the operating system.
///
/// An allow-list rather than a block-list. The page asking for this is served
/// from the container, where an agent can influence what ends up on it, so a
/// `file:` or a shell-adjacent scheme arriving here must not be honoured.
const OPENABLE_SCHEMES: [&str; 3] = ["http", "https", "mailto"];

/// Opens a URL in whatever the operating system uses for it.
fn open_externally(app: &AppHandle, target: &str) {
    let scheme = target
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .unwrap_or_default();

    if !OPENABLE_SCHEMES.contains(&scheme.as_str()) {
        logs::record("open", &format!("refused to open a {scheme} url"));
        return;
    }

    logs::record("open", &format!("opening {scheme} link externally"));
    if let Err(error) = app.opener().open_url(target, None::<&str>) {
        logs::record("open", &format!("could not open the link: {error}"));
    }
}

/// Responds to a button press in the injected banner.
fn handle_banner_action(
    app: &AppHandle,
    action: String,
    query: std::collections::HashMap<String, String>,
) {
    let Some(state) = app.try_state::<Arc<Launcher>>() else {
        return;
    };
    let state = state.inner().clone();

    match action.as_str() {
        // The page has a document and can be drawn on. Anything found before
        // this point was pushed into a webview that could not receive it.
        "ready" => push_banner_state(app, &state),
        // "self-update" survives as an alias so a banner injected by an older
        // launcher build still lands somewhere sensible.
        "update" | "self-update" => {
            let app = app.clone();
            thread::spawn(move || apply_updates(app, state));
        }
        "dismiss" => {
            lock(&state.notice).take();
            lock(&state.update).available = false;
            // The one banner covers both updates, so Later must silence both.
            // The next scheduled check finds the launcher update again.
            lock(&state.launcher_update).take();
        }
        "open" => {
            if let Some(target) = query.get("url") {
                open_externally(app, target);
            }
        }
        _ => {}
    }
}

/// Compares the installed image against what Docker Hub currently publishes.
fn check_for_updates(app: AppHandle, state: Arc<Launcher>) {
    {
        let mut update = lock(&state.update);
        update.checking = true;
        let _ = app.emit("update", update.clone());
    }

    let installed = docker::image_digest(&config::server_image());
    // The label is the friendlier of the two, so it wins when the image carries
    // one; the digest is the fallback for hand-built images.
    let label = docker::image_version(&config::server_image());
    let display = label.clone().or_else(|| {
        installed
            .as_deref()
            .map(registry::short_digest)
    });
    let result = registry::latest_tag(config::SERVER_REPO, config::TAG);
    match result.as_ref() {
        Err(error) => logs::record("update", &format!("could not reach Docker Hub: {error}")),
        Ok(remote) => logs::record(
            "update",
            &format!(
                "docker hub publishes {}; installed digest is {}",
                remote.digest,
                installed.as_deref().unwrap_or("none"),
            ),
        ),
    }

    let info = match result {
        Err(error) => UpdateInfo {
            available: false,
            checking: false,
            date: None,
            installed: display,
            published: None,
            // Reported but not surfaced as a failure: being offline is normal
            // and must not look like something is broken.
            error: Some(error),
        },
        Ok(remote) => {
            // With nothing installed there is nothing to compare, and the boot
            // sequence will pull it anyway, so this is not an "update".
            let available = installed
                .as_ref()
                .is_some_and(|local| local != &remote.digest);
            UpdateInfo {
                available,
                checking: false,
                date: remote
                    .published
                    .as_deref()
                    .and_then(registry::friendly_date),
                installed: display,
                published: Some(registry::short_digest(&remote.digest)),
                error: None,
            }
        }
    };

    // Read from the result rather than from the stored state, which still holds
    // the previous check at this point and would leave the menu a check behind.
    refresh_menu(&app, info.installed.as_deref());

    set_update(&app, &state, info);
    push_banner_state(&app, &state);
}

/// Checks when the launcher opens and then on a long interval, for the life of
/// the app.
///
/// Started with the launcher rather than once Kayak is serving: a release that
/// stops Kayak from starting at all could otherwise never be replaced by the
/// one that fixes it. The app's own update in particular needs no Docker.
fn spawn_update_watcher(app: AppHandle, state: Arc<Launcher>) {
    thread::spawn(move || loop {
        check_for_updates(app.clone(), state.clone());
        // Checked on the same schedule so a launcher released while the app is
        // open is offered too, rather than only at the next cold start.
        check_launcher_update(app.clone(), state.clone());
        thread::sleep(UPDATE_INTERVAL);
    });
}

/// The images Kayak ran from before an update, so a failed update can be undone.
struct Previous(Vec<(String, Option<String>)>);

impl Previous {
    /// Noted before the pull: repointing a tag leaves the image it replaced
    /// behind, untagged and several gigabytes large, and after the pull there is
    /// no way to tell which of the untagged images used to be ours.
    fn note() -> Self {
        Self(
            [config::server_image(), config::sandbox_image()]
                .into_iter()
                .map(|tag| {
                    let id = docker::image_id(&tag);
                    (tag, id)
                })
                .collect(),
        )
    }

    fn ids(&self) -> Vec<String> {
        self.0.iter().filter_map(|(_, id)| id.clone()).collect()
    }

    /// Points each tag back at the image it named before the update, and
    /// returns the images the update had downloaded in their place.
    ///
    /// Without this, a failed update was undone only until the next launch,
    /// which started the very version that had just failed. The downloaded
    /// images are only returned, not deleted: the failed server may still be
    /// using one, and Docker refuses to delete an image a container uses.
    fn retag(&self) -> Vec<String> {
        let mut downloaded = Vec::new();
        for (tag, previous) in &self.0 {
            let Some(previous) = previous else {
                continue;
            };
            let now = docker::image_id(tag);
            if now.as_deref() == Some(previous.as_str()) {
                continue;
            }
            match docker::tag(previous, tag) {
                Ok(()) => {
                    logs::record("update", &format!("{tag} points at the previous version again"));
                    downloaded.extend(now);
                }
                Err(error) => logs::record("update", &format!("could not restore {tag}: {error}")),
            }
        }
        downloaded
    }
}

/// Deletes images by ID. Failures are ignored: an image something still uses
/// is one that should stay.
fn remove_images(ids: &[String]) {
    for id in ids {
        let _ = docker::remove_image(id);
    }
}

/// Deletes images that have been replaced.
///
/// Only once the new server is running: Docker refuses to remove an image a
/// container still references, so doing this any earlier would fail on the
/// very image being replaced.
fn remove_superseded(ids: &[String]) {
    if ids.is_empty() {
        return;
    }
    let current: Option<Vec<String>> = [config::server_image(), config::sandbox_image()]
        .iter()
        .map(|image| docker::image_id(image))
        .collect();
    remove_images(&no_longer_used(ids, current));
}

/// The images among `ids` that no tag points at any more.
///
/// When the current images could not all be read, nothing is: a lookup that
/// failed for a moment would otherwise leave the image in use off the list of
/// current ones, and delete it from under Kayak.
fn no_longer_used(ids: &[String], current: Option<Vec<String>>) -> Vec<String> {
    let Some(current) = current else {
        logs::record("update", "could not read the current images; keeping the old ones");
        return Vec::new();
    };
    ids.iter().filter(|id| !current.contains(id)).cloned().collect()
}

/// Pulls the new images and replaces the running container with them.
///
/// Reports success so the combined flow knows whether to carry on to the
/// launcher update. The caller holds the busy guard.
///
/// The previous version is only discarded once the new one has proved itself.
/// A failure at any step puts the previous version back -- tags, images and,
/// if it had already been stopped, the running server -- so "Not now" never
/// leaves Kayak dead behind it.
fn update_kayak_image(app: &AppHandle, state: &Arc<Launcher>) -> bool {
    logs::record("update", "applying a Kayak update");
    let previous = Previous::note();
    // Whether the running server has been taken down, after which a failure
    // has to start the previous version again rather than leave it running.
    let mut replaced = false;

    let result = (|| -> Result<u16, String> {
        pull_update(app, state)?;
        ensure_not_stopping(state)?;

        report_update(app, state, "Restarting Kayak", 80);
        let data_dir = paths::ensure_data_dir()?;

        replaced = true;
        // Stopped gracefully rather than killed: the server owns a SQLite
        // database, and a SIGKILL mid-write can leave it corrupt.
        docker::stop_container(config::CONTAINER_NAME)?;
        docker::remove_container(config::CONTAINER_NAME)?;

        let port = launch_container(app, &config::server_image(), &data_dir, state)?;
        report_update(app, state, "Waiting for Kayak to come back", 92);
        wait_for_health(port, Duration::from_secs(config::HEALTH_TIMEOUT_SECS), state)?;
        Ok(port)
    })();

    match result {
        Ok(port) => {
            remove_superseded(&previous.ids());
            *lock(&state.port) = Some(port);
            lock(&state.update).available = false;
            // The port can change across a restart, so the window is pointed
            // at the new address rather than simply reloaded, and cache-busted:
            // a plain navigation is served from the webview's HTTP cache, which
            // is exactly how an update used to bring back the previous
            // release's UI instead of the one just installed.
            present_kayak(app, state, port);
            // Read again so the About panel, the banner and the versions Kayak
            // shows describe what is now installed rather than what was.
            let (app, state) = (app.clone(), state.clone());
            thread::spawn(move || check_for_updates(app, state));
            true
        }
        Err(error) => {
            logs::record("update", &format!("update failed: {error}"));
            let downloaded = previous.retag();
            if stopping(state) {
                // Quitting removes the failed server; an image it was using
                // is refused here and left for the next update to replace.
                remove_images(&downloaded);
            } else {
                recover_from_failed_update(app, state, replaced, &downloaded, &error);
            }
            false
        }
    }
}

/// Puts Kayak back on screen after a failed update, on the version the user had,
/// deletes what the update downloaded, and says what happened.
fn recover_from_failed_update(
    app: &AppHandle,
    state: &Arc<Launcher>,
    replaced: bool,
    downloaded: &[String],
    error: &str,
) {
    if !replaced {
        // The previous server was never touched, so nothing uses the
        // downloaded images and they can go straight away.
        remove_images(downloaded);
    } else {
        report_update(app, state, "Going back to the version you had", 95);
        let restarted = (|| -> Result<u16, String> {
            let data_dir = paths::ensure_data_dir()?;
            // Whatever the update started is in the way of the name.
            docker::stop_container(config::CONTAINER_NAME)?;
            docker::remove_container(config::CONTAINER_NAME)?;
            // Only now that the failed server is gone can Docker delete the
            // images it was started from.
            remove_images(downloaded);
            let port = launch_container(app, &config::server_image(), &data_dir, state)?;
            wait_for_health(port, Duration::from_secs(config::HEALTH_TIMEOUT_SECS), state)?;
            Ok(port)
        })();
        match restarted {
            Ok(port) => {
                logs::record("update", "the previous version is running again");
                *lock(&state.port) = Some(port);
                if !present_kayak(app, state, port) {
                    return;
                }
            }
            Err(restart_error) => {
                logs::record(
                    "update",
                    &format!("the previous version could not be started again: {restart_error}"),
                );
                if !stopping(state) {
                    fail(app, state, &restart_error);
                }
                return;
            }
        }
    }
    report_update_failure(app, state, explain::update_failure(error), "OK");
}

/// Downloads both images for an update, as one progress bar.
fn pull_update(app: &AppHandle, state: &Launcher) -> Result<(), String> {
    let images = [
        (config::server_image(), 0, 50),
        (config::sandbox_image(), 50, 75),
    ];
    let steps = images.len();
    for (index, (image, from, to)) in images.iter().enumerate() {
        let label = format!("Downloading the update ({} of {steps})", index + 1);
        report_update(app, state, &label, *from);
        docker::pull(image, &state.shutting_down, |progress| {
            report_update(app, state, &label, progress_within(*from, *to, progress.done, progress.total));
        })?;
    }

    drop_legacy_sandbox_tag();
    Ok(())
}

/// Maps one download's progress onto its share of the update's progress bar.
///
/// Each download used to fill the whole bar and then start again from empty,
/// which reads as something having gone wrong.
fn progress_within(from: i32, to: i32, done: u32, total: u32) -> i32 {
    if total == 0 {
        return from;
    }
    let share = i64::from(to - from) * i64::from(done.min(total)) / i64::from(total);
    from + share as i32
}

/// What the health watch concluded about a Kayak that stopped answering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outage {
    /// Still running, so most likely just busy: keep watching.
    Busy,
    /// The server has stopped, or has gone so long without answering while
    /// still running that it is hung.
    ServerDown { hung: bool },
    DockerClosed,
    DockerUnresponsive,
}

/// Decides what a run of missed health checks means.
///
/// The container is asked about before anything is done, because a Mac under
/// heavy load -- a large model loading on the graphics chip, say -- can leave a
/// perfectly healthy server too slow to answer for a while, and replacing it
/// then would throw away whatever its agents were in the middle of. Docker
/// Desktop is given the same patience: it is slow to answer for a while after
/// the Mac wakes from sleep, and that is not something to interrupt anyone for.
fn assess_outage(daemon: Daemon, container: Option<ContainerState>, misses: u32) -> Outage {
    match daemon {
        Daemon::Stopped => Outage::DockerClosed,
        Daemon::Unresponsive if misses < HUNG_MISSES => Outage::Busy,
        Daemon::Unresponsive => Outage::DockerUnresponsive,
        Daemon::Running => match container {
            Some(ContainerState::Running) | None if misses < HUNG_MISSES => Outage::Busy,
            Some(ContainerState::Running) | None => Outage::ServerDown { hung: true },
            Some(_) => Outage::ServerDown { hung: false },
        },
    }
}

/// Watches a running Kayak, for the life of the app.
///
/// Without it, a server that stopped -- or a Docker Desktop that was quit --
/// left the Kayak window on a dead page with no explanation and no way back
/// short of quitting. Quiet whenever Kayak is not meant to be running, or a
/// start or an update is replacing it on purpose.
fn spawn_health_watch(app: AppHandle, state: Arc<Launcher>) {
    thread::spawn(move || {
        let mut misses = 0;
        while !stopping(&state) {
            thread::sleep(WATCH_INTERVAL);
            let watching =
                !state.busy.load(Ordering::SeqCst) && matches!(*lock(&state.stage), Stage::Ready);
            let port = *lock(&state.port);
            let (true, Some(port)) = (watching, port) else {
                misses = 0;
                continue;
            };
            if kayak_answers(port) {
                misses = 0;
                continue;
            }
            misses += 1;
            if misses < WATCH_MISSES {
                continue;
            }

            let daemon = docker::daemon();
            let container = (daemon == Daemon::Running)
                .then(|| docker::container_state(config::CONTAINER_NAME).ok())
                .flatten();
            match assess_outage(daemon, container, misses) {
                Outage::Busy => {}
                outage => {
                    misses = 0;
                    respond_to_outage(&app, &state, outage);
                }
            }
        }
    });
}

fn respond_to_outage(app: &AppHandle, state: &Arc<Launcher>, outage: Outage) {
    let Some(guard) = BusyGuard::acquire(state) else {
        return;
    };
    if stopping(state) {
        return;
    }

    match outage {
        Outage::Busy => {}
        Outage::ServerDown { hung } => {
            logs::record(
                "watch",
                if hung {
                    "kayak has not answered for two minutes; it will be replaced"
                } else {
                    "kayak stopped unexpectedly"
                },
            );
            // A hung server is still running, so the next start would
            // otherwise reconnect to it and wait all over again.
            if hung {
                state.recreate_next.store(true, Ordering::SeqCst);
            }
            show_problem(app, state, Problem::ServerStopped);
        }
        Outage::DockerUnresponsive => {
            logs::record("watch", "docker desktop stopped answering");
            show_problem(app, state, Problem::DockerUnresponsive);
        }
        Outage::DockerClosed => {
            logs::record("watch", "docker desktop went away; waiting for it to come back");
            set_stage(app, state, Stage::DockerAsleep { closed: true });
            surface_launcher(app);
            drop(guard);
            if wait_for_docker_to_return(state) {
                boot(app.clone(), state.clone());
            }
        }
    }
}

/// Waits, for as long as it takes, for Docker Desktop to answer again.
///
/// Gives up when the app quits, or when something else has taken over the
/// launcher window in the meantime -- an update started from it, for instance.
fn wait_for_docker_to_return(state: &Launcher) -> bool {
    loop {
        thread::sleep(Duration::from_secs(3));
        if stopping(state) || !matches!(*lock(&state.stage), Stage::DockerAsleep { closed: true }) {
            return false;
        }
        if docker::daemon() == Daemon::Running {
            logs::record("watch", "docker desktop is back; starting Kayak again");
            return true;
        }
    }
}

/// Why GPU inference is unavailable, when the hardware could otherwise do it.
///
/// Worth saying out loud: the machine can use its graphics chip, and the only
/// thing stopping it is which build of the app is installed. Silently
/// reporting "unsupported" here is what hid GPU inference from the users who
/// actually had the hardware.
fn rosetta_detail(silicon: metal::Silicon) -> Option<String> {
    (silicon == metal::Silicon::Translated).then(|| metal::NEEDS_APPLE_BUILD.to_string())
}

/// Writes the status file, always carrying the current versions with it.
///
/// Kayak displays both, and it can read neither on its own: the launcher is a
/// separate program, and the server's version is a label on the image it runs
/// from, which is not visible from inside that image.
fn write_status_with_versions(
    state: &Launcher,
    paths: &control::ControlPaths,
    metal: control::MetalStatus,
) {
    let versions = control::Versions {
        launcher: env!("CARGO_PKG_VERSION").to_string(),
        kayak: lock(&state.update).installed.clone(),
    };
    let _ = control::write_status(&paths.status, &control::Status { metal, versions });
}

/// Writes a status from the reconcile loop, unless the app has begun quitting.
///
/// Quitting writes the final "stopped" status itself, and a reconcile pass that
/// was already under way must not overwrite it with the "ready" it was about to
/// report. Both writes hold the same lock and quitting raises its flag before
/// taking it, so the final word is always the shutdown's.
fn publish(state: &Launcher, paths: &control::ControlPaths, status: control::MetalStatus) {
    let _writing = lock(&state.status_write);
    if stopping(state) {
        return;
    }
    write_status_with_versions(state, paths, status);
}

/// Records a failed GPU start against the request it answers, and reports it.
fn report_metal_failure(
    state: &Launcher,
    paths: &control::ControlPaths,
    mut status: control::MetalStatus,
    model: &str,
    request: Option<String>,
    failure: metal::Failure,
) {
    logs::record("metal", &format!("could not serve {model}: {}", failure.reason));
    status.state = "error".to_string();
    status.port = 0;
    status.error = Some(failure.reason.to_string());
    status.error_detail = failure.detail.clone();
    *lock(&state.metal_failure) = Some(MetalFailure {
        model: model.to_string(),
        request,
        failure,
    });
    publish(state, paths, status);
}

/// Drives the Metal server towards whatever Kayak has asked for.
///
/// Runs on its own thread and is allowed to block: installing the environment
/// downloads gigabytes and takes minutes, and doing that inline keeps the whole
/// sequence in one place instead of spread across a state machine.
fn reconcile_metal(state: &Launcher, paths: &control::ControlPaths) {
    let silicon = metal::silicon();
    let mut desired = control::read_desired(&paths.desired).metal;
    let mut status = control::MetalStatus {
        supported: silicon == metal::Silicon::Native,
        installed: metal::is_installed(),
        state: "stopped".to_string(),
        detail: rosetta_detail(silicon),
        // Echoed on every write, so Kayak can tell a status that answers its
        // latest request from one describing the world before it.
        request: desired.request.clone(),
        ..Default::default()
    };

    if !status.supported {
        // Nothing else can be true on this machine, and saying so lets Kayak
        // hide the option rather than offer something that cannot work.
        publish(state, paths, status);
        return;
    }

    if !desired.running {
        // Nothing is wanted. Whatever failed is no longer being asked for, so
        // the next request starts with a clean slate.
        lock(&state.metal_failure).take();
        stop_metal_server(state);
        publish(state, paths, status);
        return;
    }

    let Some(model) = desired
        .model
        .clone()
        .filter(|model| control::accepts_model(model))
    else {
        // What was asked for is not servable, so any running server comes down.
        stop_metal_server(state);
        status.state = "error".to_string();
        status.error = Some(metal::NOT_FOR_MAC.to_string());
        publish(state, paths, status);
        return;
    };
    status.model = Some(model.clone());

    {
        let mut failure = lock(&state.metal_failure);
        if let Some(known) = failure.as_ref() {
            if known.answers(&model, desired.request.as_deref()) {
                status.state = "error".to_string();
                status.error = Some(known.failure.reason.to_string());
                status.error_detail = known.failure.detail.clone();
                drop(failure);
                publish(state, paths, status);
                return;
            }
            // A new request for something that failed: Kayak is asking to try
            // again, so the failure is forgotten and the attempt made.
            *failure = None;
        }
    }

    let mut running = lock(&state.metal);
    let current = running
        .as_mut()
        .filter(|server| server.model == model)
        .map(|server| (server.is_alive(), server.port));
    match current {
        // Serving this model already: only its health needs reporting, and it
        // is checked without holding the lock quitting needs.
        Some((true, port)) => {
            drop(running);
            let answered = metal::serves(port, &model);
            let mut running = lock(&state.metal);
            let Some(server) = running
                .as_mut()
                .filter(|server| server.model == model && server.port == port)
            else {
                return;
            };
            let health = server.observe(answered);
            drop(running);

            status.port = port;
            match health {
                metal::Health::Starting => status.state = "starting".to_string(),
                metal::Health::Ready => status.state = "ready".to_string(),
                metal::Health::Unresponsive => {
                    status.state = "error".to_string();
                    status.error = Some(metal::UNRESPONSIVE.to_string());
                }
            }
            publish(state, paths, status);
            return;
        }
        // Exited on its own. What it printed is the only account of why, so it
        // is carried into the status rather than left in the log. Whatever it
        // left running is stopped under the lock, like any other stop, so
        // quitting waits for it.
        Some((false, _)) => {
            let failure = running.as_mut().map(|server| {
                let failure = server.failure();
                server.stop();
                failure
            });
            *running = None;
            drop(running);
            if let Some(failure) = failure {
                report_metal_failure(state, paths, status, &model, desired.request.clone(), failure);
            }
            return;
        }
        // A different model was asked for, so whatever is running makes way.
        None => {
            drop(running);
            stop_metal_server(state);
        }
    }

    if stopping(state) {
        return;
    }

    if !metal::is_installed() {
        status.state = "installing".to_string();
        publish(state, paths, status.clone());

        logs::record("metal", "installing the GPU environment");
        let installed = install_metal(state, paths, &status);
        if stopping(state) {
            return;
        }
        // The install takes minutes, and what Kayak wants can change in the
        // meantime -- another model, a new request, or nothing at all. The
        // next pass acts on that, rather than this one starting, or reporting
        // a failure for, something nobody is asking for any more.
        let now = control::read_desired(&paths.desired).metal;
        if !still_wanted(&desired, &now) {
            return;
        }
        desired = now;
        if let Err(failure) = installed {
            report_metal_failure(state, paths, status, &model, desired.request.clone(), failure);
            return;
        }
        status.installed = true;
    }

    let port = match docker::find_free_port(config::METAL_PORT, config::METAL_PORT_SCAN_RANGE) {
        Ok(port) => port,
        Err(error) => {
            logs::record("metal", &format!("no port for the GPU server: {error}"));
            return;
        }
    };

    let mut running = lock(&state.metal);
    // Checked under the lock quitting takes the server with, so a server is
    // either started before quitting looks -- and stopped by it -- or not
    // started at all.
    if stopping(state) {
        return;
    }
    match metal::spawn_server(
        &model,
        port,
        desired.hf_token.as_deref(),
        Some(&paths.server_pid),
    ) {
        Ok(server) => {
            *running = Some(server);
            drop(running);
            status.state = "starting".to_string();
            status.port = port;
            publish(state, paths, status);
        }
        Err(failure) => {
            drop(running);
            report_metal_failure(state, paths, status, &model, desired.request.clone(), failure);
        }
    }
}

/// Runs the GPU installer, keeping the status fresh while it does.
///
/// The install blocks for minutes, and Kayak treats a status file nobody has
/// touched recently as "no launcher running", so a heartbeat rewrites it every
/// couple of seconds. The installer's process group is kept where quitting
/// can find it, and recorded on disk for a launch that follows a crash.
fn install_metal(
    state: &Launcher,
    paths: &control::ControlPaths,
    status: &control::MetalStatus,
) -> Result<(), metal::Failure> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let result = thread::scope(|scope| {
        scope.spawn(move || {
            let outcome = metal::run_install(&|group| track_installer(state, paths, group));
            let _ = sender.send(outcome);
        });
        loop {
            match receiver.recv_timeout(Duration::from_secs(2)) {
                Ok(outcome) => break outcome,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    publish(state, paths, status.clone());
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(metal::Failure::setup("The installer stopped without reporting."));
                }
            }
        }
    });
    lock(&state.installer).take();
    let _ = std::fs::remove_file(&paths.installer_pid);
    result
}

/// Keeps the process group of whatever the GPU install is running -- the
/// installer, or a check of what it installed -- where quitting can find it,
/// in memory and on disk, and forgets it the moment that process has ended,
/// since its ID can then be handed to something else entirely.
fn track_installer(state: &Launcher, paths: &control::ControlPaths, group: Option<process::Group>) {
    match group {
        Some(group) => {
            group.record(&paths.installer_pid);
            *lock(&state.installer) = Some(group);
            // Quitting may have looked for it a moment before it was stored;
            // checking after storing means one of the two always stops it.
            // Killed outright: this runs on the thread that reaps it, which
            // cannot wait out a grace period on its own child.
            if stopping(state) {
                if let Some(group) = lock(&state.installer).take() {
                    group.kill();
                }
            }
        }
        None => {
            lock(&state.installer).take();
            let _ = std::fs::remove_file(&paths.installer_pid);
        }
    }
}

/// Reports whether what Kayak asks for now is still what a pass began on.
fn still_wanted(began: &control::MetalDesired, now: &control::MetalDesired) -> bool {
    now.running && now.model == began.model && now.request == began.request
}

/// Polls the control channel for as long as the app runs.
///
/// Stops reconciling the moment shutdown begins: the desired state still says
/// "running" at that point, and one more pass would restart the server that
/// shutdown just stopped — leaving an orphaned vLLM process behind when the
/// launcher exits moments later.
fn spawn_metal_reconciler(state: Arc<Launcher>, paths: control::ControlPaths) {
    thread::spawn(move || {
        while !stopping(&state) {
            reconcile_metal(&state, &paths);
            thread::sleep(Duration::from_secs(2));
        }
    });
}

/// Leaves the status file telling the truth once the Metal server is gone.
///
/// Without this, the last status written before quitting — often "ready" —
/// sits on disk indefinitely, and a Kayak opened later reads it as a server
/// that is up when nothing is listening at all.
fn write_stopped_status(state: &Launcher) {
    let control = control::ControlPaths::under(&paths::data_dir());
    if !control.dir.is_dir() {
        // Kayak has never run here, so there is no one to tell.
        return;
    }
    let silicon = metal::silicon();
    let status = control::MetalStatus {
        supported: silicon == metal::Silicon::Native,
        installed: metal::is_installed(),
        state: "stopped".to_string(),
        detail: rosetta_detail(silicon),
        request: control::read_desired(&control.desired).metal.request,
        ..Default::default()
    };
    let _writing = lock(&state.status_write);
    write_status_with_versions(state, &control, status);
}

/// Stops GPU processes a previous launch left running.
///
/// Done before anything else at startup, so a model the last launch was serving
/// is out of memory before a new one is asked for.
fn stop_left_behind_gpu() {
    let control = control::ControlPaths::under(&paths::data_dir());
    for pid_file in [&control.server_pid, &control.installer_pid] {
        if pid_file.exists() {
            metal::stop_left_behind(pid_file);
        }
    }
}

/// What "Copy details" puts on the clipboard: what the user was told, then what
/// the launcher was doing, which is what someone helping them needs to see.
fn details_text(summary: &str, kayak_version: Option<&str>, lines: &[String]) -> String {
    format!(
        "Kayak app {} ({}), Kayak {}\n\n{summary}\n\nWhat Kayak was doing:\n{}\n",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::ARCH,
        kayak_version.unwrap_or("not installed yet"),
        lines.join("\n"),
    )
}

fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::io::Write;

    let mut child = std::process::Command::new("/usr/bin/pbcopy")
        // An app opened from Finder has no LANG, and without one pbcopy does
        // not read its input as UTF-8.
        .env("LANG", "en_US.UTF-8")
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not run pbcopy: {error}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(text.as_bytes())
            .map_err(|error| format!("could not write to pbcopy: {error}"))?;
    }
    let status = child
        .wait()
        .map_err(|error| format!("pbcopy did not finish: {error}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("pbcopy failed: {status}"))
    }
}

#[tauri::command]
fn snapshot(state: tauri::State<'_, Arc<Launcher>>) -> Snapshot {
    Snapshot {
        stage: lock(&state.stage).clone(),
        update: lock(&state.update).clone(),
    }
}

#[tauri::command]
fn start(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) {
    let state = state.inner().clone();
    thread::spawn(move || boot(app, state));
}

/// Opens Docker Desktop, or brings it forward. Whatever is waiting for it
/// carries on by itself once it answers, so this starts nothing else.
#[tauri::command]
fn open_docker() {
    thread::spawn(|| match docker::open_desktop() {
        Ok(DesktopLaunch::Opened) => logs::record("docker", "opened docker desktop"),
        Ok(DesktopLaunch::NotInstalled) => logs::record("docker", "docker desktop is not installed"),
        Err(error) => logs::record("docker", &error),
    });
}

/// Shows the Kayak window, for when the launcher window is the one in focus.
#[tauri::command]
fn open_kayak(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) -> Result<(), String> {
    let port = lock(&state.port).ok_or_else(|| "Kayak is not running yet".to_string())?;
    // No sign-in here: the window was signed in when Kayak was first shown, and
    // its session holds the key itself, so it outlasts a replaced server too.
    show_kayak(&app, &format!("http://127.0.0.1:{port}"), None)?;
    if let Some(window) = app.get_webview_window(LAUNCHER_WINDOW) {
        let _ = window.hide();
    }
    Ok(())
}

#[tauri::command]
fn check_updates(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) {
    let state = state.inner().clone();
    thread::spawn(move || check_for_updates(app, state));
}

/// Applies whatever is pending — Kayak images, launcher, or both.
#[tauri::command]
fn install_update(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) {
    let state = state.inner().clone();
    thread::spawn(move || apply_updates(app, state));
}

/// Puts what went wrong, and what the launcher was doing, on the clipboard,
/// for the user to paste wherever they ask for help.
#[tauri::command]
fn copy_details(state: tauri::State<'_, Arc<Launcher>>) -> Result<(), String> {
    let summary = match &*lock(&state.stage) {
        Stage::Failed { headline, body } => format!("{headline}\n{body}"),
        other => format!("{other:?}"),
    };
    let kayak_version = lock(&state.update).installed.clone();
    let text = details_text(&summary, kayak_version.as_deref(), &logs::tail(DETAIL_LINES));
    copy_to_clipboard(&text).inspect_err(|error| logs::record("copy", error))
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Arc::new(Launcher::new()))
        .invoke_handler(tauri::generate_handler![
            snapshot,
            start,
            open_docker,
            open_kayak,
            check_updates,
            install_update,
            copy_details,
        ])
        .on_menu_event(|app, event| handle_menu(app, event.id().as_ref()))
        .on_web_content_process_terminate(reload_after_crash)
        .setup(|app| {
            // First, so that a launch which fails before Kayak ever runs still
            // leaves a record somewhere the user can find.
            if let Some(path) = logs::launcher_log_path() {
                logs::mirror_to(&path);
            }
            logs::record(
                "launcher",
                &format!("Kayak app {} on {}", env!("CARGO_PKG_VERSION"), std::env::consts::ARCH),
            );

            let handle = app.handle().clone();
            refresh_menu(&handle, None);
            watch_launcher_window(&handle);
            let state = app.state::<Arc<Launcher>>().inner().clone();
            spawn_update_watcher(handle.clone(), state.clone());
            spawn_health_watch(handle.clone(), state.clone());
            thread::spawn(move || {
                stop_left_behind_gpu();
                boot(handle, state);
            });
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("could not start the launcher")
        .run(|app, event| match event {
            // Quitting from the menu, the Dock or Cmd+Q arrives here rather
            // than as a window close, and it needs the same treatment: the
            // stop takes seconds, and doing it inline freezes the window.
            RunEvent::ExitRequested { api, code, .. } => {
                let Some(state) = app.try_state::<Arc<Launcher>>() else {
                    return;
                };
                if stopping(&state) {
                    // Only the exit quitting itself asks for is let through:
                    // `app.exit` and the restart into an update both carry a
                    // code. A second Cmd+Q, or Quit from the Dock, carries none,
                    // and letting it through ended the app partway through
                    // stopping what it had started.
                    if code.is_none() {
                        api.prevent_exit();
                    }
                    return;
                }
                api.prevent_exit();
                begin_shutdown(app.clone(), state.inner().clone());
            }
            // Last resort. Both the container and the Metal server outlive this
            // process unless they are stopped, and either left running would
            // keep serving an app that no longer has a window.
            RunEvent::Exit => {
                if let Some(state) = app.try_state::<Arc<Launcher>>() {
                    stop_everything(state.inner());
                }
            }
            _ => {}
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;

    #[test]
    fn cache_busted_urls_point_at_the_server_root() {
        let busted = cache_busted("http://127.0.0.1:8001");
        assert!(
            busted.starts_with("http://127.0.0.1:8001/?fresh="),
            "unexpected shape: {busted}"
        );
        assert!(busted.parse::<tauri::Url>().is_ok());
    }

    #[test]
    fn no_two_navigations_share_a_cache_entry() {
        // The whole point: a URL that already sits in the webview cache would
        // be served from it, resurrecting a previous release's UI.
        assert_ne!(cache_busted("http://127.0.0.1:8001"), cache_busted("http://127.0.0.1:8001"));
    }

    #[test]
    fn reloading_keeps_the_page_and_replaces_the_cache_buster() {
        let current: Url = "http://127.0.0.1:8000/agents/42?tab=runs&fresh=1-0".parse().unwrap();

        let reloaded = refreshed(&current);

        assert_eq!(reloaded.path(), "/agents/42");
        let pairs: Vec<(String, String)> = reloaded.query_pairs().into_owned().collect();
        assert_eq!(pairs.iter().filter(|(key, _)| key == "fresh").count(), 1);
        assert!(pairs.contains(&("tab".to_string(), "runs".to_string())));
        assert!(!pairs.contains(&("fresh".to_string(), "1-0".to_string())));
    }

    #[test]
    fn reuses_a_container_from_the_current_image() {
        assert!(should_reuse(Some("sha256:aaa"), Some("sha256:aaa")));
    }

    #[test]
    fn replaces_a_container_from_an_older_image() {
        // The case that pinned users to an old version: the tag moved, but the
        // container still referenced what it was built from.
        assert!(!should_reuse(Some("sha256:old"), Some("sha256:new")));
    }

    #[test]
    fn rebuilds_when_either_side_is_unknown() {
        assert!(!should_reuse(None, Some("sha256:aaa")));
        assert!(!should_reuse(Some("sha256:aaa"), None));
        assert!(!should_reuse(None, None));
    }

    fn failure(model: &str, request: Option<&str>) -> MetalFailure {
        MetalFailure {
            model: model.to_string(),
            request: request.map(str::to_string),
            failure: metal::Failure::setup("boom"),
        }
    }

    #[test]
    fn a_gpu_failure_answers_only_the_request_that_caused_it() {
        let failed = failure("mlx-community/X", Some("first"));

        assert!(failed.answers("mlx-community/X", Some("first")));
        // Try again sends the same model with a new token: that must be a real
        // second attempt, not the old error echoed back.
        assert!(!failed.answers("mlx-community/X", Some("second")));
        assert!(!failed.answers("mlx-community/Y", Some("first")));
    }

    #[test]
    fn a_gpu_failure_from_an_older_kayak_without_tokens_still_sticks() {
        // Without tokens the only signal is the model, which is what stops the
        // loop restarting a failing server every two seconds.
        let failed = failure("mlx-community/X", None);

        assert!(failed.answers("mlx-community/X", None));
        assert!(!failed.answers("mlx-community/Y", None));
    }

    fn desired(running: bool, model: &str, request: &str) -> control::MetalDesired {
        control::MetalDesired {
            running,
            model: Some(model.to_string()),
            request: Some(request.to_string()),
            hf_token: None,
        }
    }

    #[test]
    fn a_request_that_changed_during_an_install_is_not_started() {
        let began = desired(true, "mlx-community/X", "first");

        assert!(still_wanted(&began, &desired(true, "mlx-community/X", "first")));
        assert!(!still_wanted(&began, &desired(false, "mlx-community/X", "first")));
        assert!(!still_wanted(&began, &desired(true, "mlx-community/Y", "first")));
        assert!(!still_wanted(&began, &desired(true, "mlx-community/X", "second")));
        // A token added meanwhile is not a different request; it is picked up.
        let with_token = control::MetalDesired {
            hf_token: Some("hf_x".to_string()),
            ..desired(true, "mlx-community/X", "first")
        };
        assert!(still_wanted(&began, &with_token));
    }

    #[test]
    fn old_images_are_kept_when_the_current_ones_cannot_all_be_read() {
        let old = vec!["sha256:old-server".to_string(), "sha256:sandbox".to_string()];

        // A failed lookup must not leave the sandbox in use off the list of
        // current images, and so get it deleted.
        assert!(no_longer_used(&old, None).is_empty());
        assert_eq!(
            no_longer_used(
                &old,
                Some(vec!["sha256:new-server".to_string(), "sha256:sandbox".to_string()])
            ),
            vec!["sha256:old-server".to_string()]
        );
    }

    #[test]
    fn update_progress_fills_one_bar_instead_of_restarting() {
        assert_eq!(progress_within(0, 50, 0, 0), 0);
        assert_eq!(progress_within(0, 50, 5, 10), 25);
        assert_eq!(progress_within(0, 50, 10, 10), 50);
        assert_eq!(progress_within(50, 75, 0, 4), 50);
        assert_eq!(progress_within(50, 75, 4, 4), 75);
        // Docker can briefly report more layers done than it has announced.
        assert_eq!(progress_within(50, 75, 9, 4), 75);
    }

    #[test]
    fn a_slow_server_that_is_still_running_is_left_alone() {
        assert_eq!(
            assess_outage(Daemon::Running, Some(ContainerState::Running), WATCH_MISSES),
            Outage::Busy
        );
        // Not knowing is not evidence of anything either.
        assert_eq!(assess_outage(Daemon::Running, None, WATCH_MISSES), Outage::Busy);
    }

    #[test]
    fn a_server_that_stays_silent_long_enough_is_hung() {
        assert_eq!(
            assess_outage(Daemon::Running, Some(ContainerState::Running), HUNG_MISSES),
            Outage::ServerDown { hung: true }
        );
    }

    #[test]
    fn a_server_that_exited_is_reported_at_once() {
        for state in [ContainerState::Stopped, ContainerState::Missing] {
            assert_eq!(
                assess_outage(Daemon::Running, Some(state), WATCH_MISSES),
                Outage::ServerDown { hung: false }
            );
        }
    }

    #[test]
    fn docker_going_away_is_told_apart_from_kayak_stopping() {
        assert_eq!(assess_outage(Daemon::Stopped, None, WATCH_MISSES), Outage::DockerClosed);
        assert_eq!(
            assess_outage(Daemon::Unresponsive, None, HUNG_MISSES),
            Outage::DockerUnresponsive
        );
    }

    #[test]
    fn a_docker_desktop_slow_to_answer_is_given_time() {
        // Typical for a minute after the Mac wakes from sleep.
        assert_eq!(assess_outage(Daemon::Unresponsive, None, WATCH_MISSES), Outage::Busy);
    }

    #[test]
    fn copied_details_lead_with_what_the_user_was_told() {
        let text = details_text(
            "Kayak couldn't start\nClick Try again.",
            Some("2.4.0"),
            &["[boot] starting up".to_string(), "[boot] boom".to_string()],
        );

        assert!(text.starts_with(&format!("Kayak app {}", env!("CARGO_PKG_VERSION"))));
        assert!(text.contains("Kayak 2.4.0"));
        let told = text.find("Kayak couldn't start").unwrap();
        let log = text.find("[boot] boom").unwrap();
        assert!(told < log);
    }

    /// Answers one request with `response`, returning the URL to fetch it from.
    fn serve_once(response: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 1024];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(response);
        });
        format!("http://127.0.0.1:{port}/api/audio/items/1/file")
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("kayak-launcher-download");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        path
    }

    #[test]
    fn sends_the_access_key_to_kayak_and_nowhere_else() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let read = stream.read(&mut request).unwrap_or(0);
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
            String::from_utf8_lossy(&request[..read]).to_lowercase()
        });
        let path = scratch("with-key.wav");

        save_url(&format!("http://127.0.0.1:{port}/file"), &path, Some("secret")).unwrap();

        assert!(server.join().unwrap().contains("x-kayak-token: secret"));
        assert!(super::is_loopback_url("http://localhost:8000/x"));
        assert!(!super::is_loopback_url("https://example.com/file"));
    }

    #[test]
    fn saves_the_file_where_the_user_chose() {
        let url = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nRIFF!");
        let path = scratch("saved.wav");

        save_url(&url, &path, None).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"RIFF!");
    }

    #[test]
    fn a_refused_download_creates_nothing() {
        let url = serve_once(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
        let path = scratch("missing.wav");

        let error = save_url(&url, &path, None).unwrap_err();

        assert!(error.contains("404"), "{error}");
        assert!(!path.exists());
    }

    #[test]
    fn a_download_cut_short_leaves_no_partial_file() {
        // The server promises more than it sends, then hangs up.
        let url = serve_once(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nRIFF");
        let path = scratch("partial.wav");

        assert!(save_url(&url, &path, None).is_err());
        assert!(!path.exists());
    }
}
