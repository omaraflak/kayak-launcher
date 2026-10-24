//! Launcher for Kayak.
//!
//! Kayak ships as a Docker image. This app exists so that running it never
//! requires a terminal: it finds Docker, downloads the images, starts the
//! server, opens Kayak in the browser, and keeps both itself and Kayak up to
//! date without anyone having to press a button.

mod access;
mod config;
mod control;
mod docker;
mod explain;
mod images;
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
use tauri::{AppHandle, Emitter, Manager, RunEvent, WindowEvent};
use tauri_plugin_opener::OpenerExt;
use tauri_plugin_updater::UpdaterExt;

use docker::{ContainerState, Daemon, DesktopLaunch};
use explain::Problem;

const LAUNCHER_WINDOW: &str = "launcher";

/// Gap between background update checks.
const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// Limit on asking GitHub whether a newer app exists, when the user asked.
const UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(30);

/// The same limit at launch, where every second of it is a second before Kayak
/// opens. Offline, the check fails at once and the launch carries on.
const LAUNCH_UPDATE_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the app's own update download may receive nothing before it is
/// abandoned, so a stalled download ends instead of holding up Kayak for good.
const LAUNCHER_DOWNLOAD_STALL: Duration = Duration::from_secs(60);

/// The longest the app's own update may hold up a launch, however steadily it
/// is downloading. Past it, Kayak starts on the app already installed, and the
/// update is tried again at the next launch.
const LAUNCH_APP_UPDATE_LIMIT: Duration = Duration::from_secs(2 * 60);

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

/// How long "Kayak is up to date." stays in the launcher window.
const NOTE_SECONDS: u64 = 4;

/// The tab left open across a restart into an app update. See
/// [`remember_open_tab`].
const OPEN_TAB_FILE: &str = "open-tab-port";

/// The app update last installed, by version. See [`retry_launcher_update`].
const LAUNCHER_ATTEMPT_FILE: &str = "app-update-attempt";

const MENU_OPEN: &str = "open-kayak";
const MENU_CHECK_UPDATES: &str = "check-for-updates";
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
    /// Kayak, or this app, is being updated.
    Updating {
        message: String,
        percent: i32,
    },
    /// Kayak is serving and has been opened in the browser. `notice` replaces
    /// the usual words when there is something to say -- an update that did
    /// not go through, or a browser that would not open.
    Ready {
        notice: Option<Notice>,
    },
    Failed {
        headline: String,
        body: String,
    },
    Stopping,
}

/// Something worth saying on the Ready screen instead of its usual words.
#[derive(Debug, Clone, Serialize)]
pub struct Notice {
    headline: String,
    body: String,
}

impl Notice {
    fn new(headline: &str, body: impl Into<String>) -> Self {
        Self {
            headline: headline.to_string(),
            body: body.into(),
        }
    }

    /// A Kayak update failed, and the previous version is serving again.
    fn update_failed(error: &str) -> Self {
        Self::new(explain::UPDATE_FAILED, explain::update_failure(error))
    }

    /// Kayak is serving again where its open tab expects it. Said rather than
    /// "open in your browser", because the tab may have been closed since.
    fn reconnecting() -> Self {
        Self::new(
            "Kayak is running again",
            "Its tab in your browser reconnects by itself. If you closed it, click Open Kayak.",
        )
    }

    /// Kayak is serving, but the browser could not be sent to it.
    fn browser_failed() -> Self {
        Self::new(
            "Couldn't open Kayak in your browser",
            "Click Open Kayak to try again.",
        )
    }
}

/// What is known about the installed Kayak versus the published one.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateInfo {
    pub available: bool,
    pub checking: bool,
    pub installed: Option<String>,
}

#[derive(Serialize)]
pub struct Snapshot {
    stage: Stage,
    update: UpdateInfo,
}

/// The browser tab Kayak was opened in, as far as the launcher can know it.
///
/// A server that comes back on the same port is left to the tab already
/// showing it, which reconnects by itself; opening another would only leave a
/// duplicate. But only a tab that is still signed in: one signed in with a key
/// the server no longer has, or opened before the server had one, would be
/// left on a page asking to sign in.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenTab {
    port: u16,
    /// What the tab was signed in with, by [`fingerprint`]; None when the
    /// server needed no signing in.
    signed_in_as: Option<String>,
}

/// Enough of a key to tell it apart from another, without writing the key
/// itself anywhere it is not already.
fn fingerprint(key: &str) -> String {
    key.chars().take(12).collect()
}

/// Whether the tab already open will reconnect, signed in, to a server on
/// `port` that requires `serving` -- a key fingerprint, or None for none.
fn tab_reconnects(tab: Option<&OpenTab>, port: u16, serving: Option<&str>) -> bool {
    tab.is_some_and(|tab| tab.port == port && tab.signed_in_as.as_deref() == serving)
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
    /// The key the running server requires, as it was when the launcher
    /// started it or checked it. Remembered rather than asked of Docker again
    /// at every opening, because a Docker call that fails for a moment would
    /// then open Kayak signed out.
    serving_key: Mutex<Option<String>>,
    /// The tab Kayak was last opened in. See [`OpenTab`].
    opened_tab: Mutex<Option<OpenTab>>,
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
    /// What the last failed start was, which decides what Try again does first.
    last_problem: Mutex<Option<Problem>>,
    /// Set when the running server must be replaced rather than reconnected
    /// to at the next start: it stopped answering, or it is the old version.
    recreate_next: AtomicBool,
    /// Set once the app has begun quitting. Every loop and sequence checks it,
    /// so nothing new is started underneath the shutdown.
    shutting_down: AtomicBool,
    /// Set once everything has been stopped, so it happens only once whichever
    /// way the app is quitting.
    stopped: AtomicBool,
    /// Guards the start and update sequences so that two of them -- a click
    /// and a background check, say -- cannot run `docker run` against the same
    /// container name at once.
    busy: AtomicBool,
    /// Set while the busy flag is held only for a background download, which
    /// gives way to anything the user asks for. See [`claim`].
    prefetching: AtomicBool,
    /// Stops a background download: raised by quitting, and by [`claim`].
    prefetch_cancel: AtomicBool,
    /// Whether this run has been through its launch: the app's own update is
    /// applied only then, and a newer Kayak is fetched before the first start.
    launched: AtomicBool,
    /// Whether the GPU control channel has been set up for this launch.
    bridge_started: AtomicBool,
}

impl Launcher {
    fn new() -> Self {
        Self {
            stage: Mutex::new(Stage::Checking),
            update: Mutex::new(UpdateInfo::default()),
            port: Mutex::new(None),
            serving_key: Mutex::new(None),
            opened_tab: Mutex::new(None),
            metal: Mutex::new(None),
            metal_failure: Mutex::new(None),
            installer: Mutex::new(None),
            status_write: Mutex::new(()),
            last_problem: Mutex::new(None),
            recreate_next: AtomicBool::new(false),
            shutting_down: AtomicBool::new(false),
            stopped: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            prefetching: AtomicBool::new(false),
            prefetch_cancel: AtomicBool::new(false),
            launched: AtomicBool::new(false),
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

/// Claims the busy flag for something that has to happen now, setting aside a
/// background download that holds it.
///
/// A background download only prepares the next launch, and making the user
/// wait for it -- or ignoring their click -- would put it first. It is stopped
/// where it is, to be picked up again later. With `wait`, a start or an update
/// already under way is waited for rather than given up on, so nothing the user
/// asked for is dropped; without, None means one is under way.
fn claim(state: &Arc<Launcher>, wait: bool) -> Option<BusyGuard> {
    loop {
        if let Some(guard) = BusyGuard::acquire(state) {
            return Some(guard);
        }
        if stopping(state) {
            return None;
        }
        if state.prefetching.load(Ordering::SeqCst) {
            if !state.prefetch_cancel.swap(true, Ordering::SeqCst) {
                logs::record("update", "setting the background download aside");
            }
        } else if !wait {
            return None;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

/// Brings the launcher window forward.
///
/// Used whenever Kayak cannot be used, and whenever the launcher has something
/// to show: the browser tab cannot say why Kayak went away, so the launcher
/// window does.
fn surface_launcher(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LAUNCHER_WINDOW) {
        let _ = window.unminimize();
        let _ = window.show();
        let _ = window.set_focus();
    }
}

fn hide_launcher(app: &AppHandle) {
    if let Some(window) = app.get_webview_window(LAUNCHER_WINDOW) {
        let _ = window.hide();
    }
}

/// Shows a short note in the launcher window's footer, such as "Kayak is up to
/// date." The window takes it away again by itself.
fn note(app: &AppHandle, text: &str) {
    let _ = app.emit("note", (text, NOTE_SECONDS));
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

/// Brings Kayak up, from a cold machine to Kayak open in the browser.
///
/// The first run of it at launch also brings things up to date: the app itself
/// before anything else, and Kayak before it is started, so the version that
/// opens is the newest one there is -- with nothing for the user to do.
fn boot(app: AppHandle, state: Arc<Launcher>) {
    // Not waited for: a start already under way is doing exactly this, with
    // its progress on screen.
    let Some(guard) = claim(&state, false) else {
        return;
    };
    if stopping(&state) {
        return;
    }
    let at_launch = !state.launched.swap(true, Ordering::SeqCst);

    logs::record("boot", "starting up");
    set_stage(&app, &state, Stage::Checking);

    // First, and needing no Docker: a release of this app that cannot start
    // Kayak must still be able to replace itself with the one that can.
    if at_launch {
        update_app_at_launch(&app, &state);
        if stopping(&state) {
            return;
        }
    }

    if !docker::is_installed() {
        logs::record("boot", "docker CLI not found on this machine");
        show_docker_missing(&app, &state);
        return;
    }

    if !ensure_docker(&app, &state) {
        return;
    }

    // A launch never waits for a download: a newer Kayak downloaded in the
    // background is simply what starts, and one not downloaded yet is fetched
    // in the background once Kayak is open, for the next launch. Only Try again
    // after a server that stopped right after starting -- most often a broken
    // release, for which the fix is the next release -- fetches it first.
    let retrying_crash = lock(&state.last_problem).take() == Some(Problem::ServerExited);
    if retrying_crash {
        if let Err(error) = refresh_kayak(&app, &state) {
            // Not a reason to stop: the version already installed may yet run.
            logs::record("update", &format!("could not download the newer Kayak: {error}"));
            if stopping(&state) {
                return;
            }
        }
    }

    let (port, rolled_back) = match start_with_rollback(&app, &state) {
        Ok(started) => started,
        Err(error) => {
            if !stopping(&state) {
                fail(&app, &state, &error);
            }
            return;
        }
    };
    *lock(&state.port) = Some(port);
    logs::record("boot", &format!("kayak is serving on port {port}"));

    kayak_is_up(&app, &state, port, rolled_back.as_deref().map(Notice::update_failed));
    drop(guard);

    // Read now that Kayak runs: the version for the About panel, and what Kayak
    // shows as installed. Then, after a launch, the newer Kayak if there is one.
    thread::spawn(move || {
        check_for_updates(app, state.clone());
        if at_launch {
            prefetch_kayak(&state);
        }
    });
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
        let holds_key = access::key().filter(|key| {
            docker::container_has_env(config::CONTAINER_NAME, access::ENV_NAME, key)
        });
        let missing_key = server_key(&config::server_image()).is_some() && holds_key.is_none();
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
                    Ok(()) => {
                        *lock(&state.serving_key) = holds_key;
                        return Ok(port);
                    }
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

/// Says what a start is doing, in the launcher window.
fn show_step(app: &AppHandle, state: &Launcher, headline: &str, hint: &str) {
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
            Ok(()) => {
                *lock(&state.serving_key) = key.clone();
                break port;
            }
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

/// The access key a server started from `image` should require, if any.
///
/// None when this Mac has no key, or when the image predates keys: its server
/// could not sign the browser in, and requiring a key it cannot hand out would
/// lock the user out of Kayak.
fn server_key(image: &str) -> Option<String> {
    let key = access::key()?;
    access::image_supports(docker::image_version(image).as_deref()).then_some(key)
}

/// Kayak is serving on `port`: shows it in the browser, and steps the launcher
/// aside.
///
/// Every path that brings Kayak up ends here, so this is also where the GPU
/// control channel is connected. When that was done only on a normal start, an
/// update brought Kayak up with nothing answering its GPU requests, and every
/// GPU model failed.
fn kayak_is_up(app: &AppHandle, state: &Arc<Launcher>, port: u16, notice: Option<Notice>) {
    connect_gpu_bridge(state);

    let serving = lock(&state.serving_key).as_deref().map(fingerprint);
    let reconnects = tab_reconnects(lock(&state.opened_tab).as_ref(), port, serving.as_deref());
    if reconnects {
        logs::record("boot", "kayak is back where the open tab expects it; it reconnects by itself");
        // The launcher cannot know whether the tab is still open, so it does not
        // claim it is; it says what happens to it, and how to get one if not.
        let notice = notice.unwrap_or_else(Notice::reconnecting);
        set_stage(app, state, Stage::Ready { notice: Some(notice) });
        return;
    }

    set_stage(
        app,
        state,
        Stage::Ready {
            notice: notice.clone(),
        },
    );
    match open_in_browser(app, state, port) {
        // Stepped aside only when there is nothing more to say; a notice stays
        // in view.
        Ok(()) if notice.is_none() => hide_launcher(app),
        Ok(()) => surface_launcher(app),
        Err(error) => {
            logs::record("open", &error);
            set_stage(
                app,
                state,
                Stage::Ready {
                    notice: Some(Notice::browser_failed()),
                },
            );
            surface_launcher(app);
        }
    }
}

/// Opens Kayak, served at `port`, in the default browser, signed in.
///
/// Each call signs in afresh: a sign-in code is good for one use, and asking
/// the server for one is cheap. When signing in fails even so, the start page is
/// opened anyway rather than nothing: the browser may still hold a session from
/// before, and if not, the page says how to get one. Makes a request to the
/// server, so it is never called on the main thread.
fn open_in_browser(app: &AppHandle, state: &Launcher, port: u16) -> Result<(), String> {
    let base = format!("http://127.0.0.1:{port}");
    let key = lock(&state.serving_key).clone();
    let signed_in = access::sign_in_address(&base, key.as_deref()).or_else(|error| {
        // A server that has only just started can be a moment from answering.
        logs::record("open", &format!("{error}; asking again"));
        thread::sleep(Duration::from_secs(1));
        access::sign_in_address(&base, key.as_deref())
    });
    let (address, tab) = match signed_in {
        Ok(address) => {
            let tab = OpenTab {
                port,
                signed_in_as: key.as_deref().map(fingerprint),
            };
            (address, Some(tab))
        }
        Err(error) => {
            logs::record("open", &format!("{error}; opening the start page instead"));
            (access::plain_address(&base)?, None)
        }
    };

    app.opener()
        .open_url(address.as_str(), None::<&str>)
        // The address can carry a sign-in code, which has no business in a log.
        .map_err(|err| {
            let reason = err.to_string().replace(address.as_str(), "the sign-in address");
            format!("Could not open the browser: {reason}")
        })?;
    // A tab that could not be signed in is not one to leave to reconnect.
    *lock(&state.opened_tab) = tab;
    logs::record("open", "opened Kayak in the browser");
    Ok(())
}

/// Opens Kayak in the browser because the user asked -- from the Dock, the menu
/// or the launcher window -- or, if Kayak is not ready, shows why instead.
fn open_kayak_now(app: AppHandle, state: Arc<Launcher>) {
    thread::spawn(move || {
        let ready = matches!(*lock(&state.stage), Stage::Ready { .. });
        let port = *lock(&state.port);
        let (true, Some(port)) = (ready, port) else {
            surface_launcher(&app);
            return;
        };
        match open_in_browser(&app, &state, port) {
            Ok(()) => hide_launcher(&app),
            Err(error) => {
                logs::record("open", &error);
                set_stage(
                    &app,
                    &state,
                    Stage::Ready {
                        notice: Some(Notice::browser_failed()),
                    },
                );
                surface_launcher(&app);
            }
        }
    });
}

/// Shows update progress in the launcher window.
fn report_update(app: &AppHandle, state: &Launcher, message: &str, percent: i32) {
    set_stage(
        app,
        state,
        Stage::Updating {
            message: message.to_string(),
            percent,
        },
    );
}

/// The digest of the published Kayak, when it is not the one installed.
///
/// None with nothing installed -- the first start downloads it anyway, so that
/// is not an update -- and when Docker Hub cannot be reached, which is normal
/// offline and must not hold anything up. A release that would not start
/// before is left alone when `respect_skipped` is set, so it is not downloaded
/// and tried again by itself at every launch.
fn newer_kayak(respect_skipped: bool) -> Option<String> {
    let installed_server = docker::image_digest(&config::server_image())?;
    let installed_sandbox = docker::image_digest(&config::sandbox_image());
    let remote_server = match registry::latest_tag(config::SERVER_REPO, config::TAG) {
        Ok(remote) => remote,
        Err(error) => {
            logs::record("update", &format!("could not reach Docker Hub: {error}"));
            return None;
        }
    };
    let remote_sandbox = registry::latest_tag(config::SANDBOX_REPO, config::TAG).ok();
    let outdated = kayak_outdated(
        &installed_server,
        installed_sandbox.as_deref(),
        &remote_server.digest,
        remote_sandbox.as_ref().map(|remote| remote.digest.as_str()),
    );
    if !outdated {
        return None;
    }
    if respect_skipped && images::is_skipped(&remote_server.digest) {
        logs::record("update", "the published Kayak would not start before; leaving it alone");
        return None;
    }
    Some(remote_server.digest)
}

/// Whether the installed images differ from the published ones.
///
/// Both are compared, not only the server: a download interrupted between the
/// two leaves a new server beside an old sandbox, and comparing the server alone
/// would call that up to date and never finish it. A side whose digest is not
/// known says nothing either way.
fn kayak_outdated(
    installed_server: &str,
    installed_sandbox: Option<&str>,
    published_server: &str,
    published_sandbox: Option<&str>,
) -> bool {
    let sandbox_differs = matches!(
        (installed_sandbox, published_sandbox),
        (Some(installed), Some(published)) if installed != published
    );
    installed_server != published_server || sandbox_differs
}

/// Downloads the published Kayak if it is newer than the installed one.
fn refresh_kayak(app: &AppHandle, state: &Launcher) -> Result<(), String> {
    if newer_kayak(true).is_none() {
        return Ok(());
    }
    logs::record("update", "a newer Kayak is published; downloading it");
    download_kayak(Some(app), state, &state.shutting_down)
}

/// Downloads the published Kayak images, keeping what they replace until Kayak
/// has run from them.
///
/// Shows its progress in the launcher window when given `app`, and downloads
/// silently otherwise.
///
/// What it replaces is recorded before anything is pulled, so a download cut
/// short -- by a crash, say, between the two images -- still leaves a way back.
/// A download that is stopped on purpose, through `cancel`, is left as it is:
/// the next one picks up where it stopped, rather than downloading again what
/// had already arrived. One that fails is undone, so the tags do not point at a
/// mix of versions.
fn download_kayak(app: Option<&AppHandle>, state: &Launcher, cancel: &AtomicBool) -> Result<(), String> {
    let before = images::Previous::note();
    let recorded_here = before.keep();
    let images = [
        (config::server_image(), 0, 50),
        (config::sandbox_image(), 50, 100),
    ];
    let steps = images.len();
    let downloaded = images.iter().enumerate().try_for_each(|(index, (image, from, to))| {
        let label = format!("Downloading the update ({} of {steps})", index + 1);
        if let Some(app) = app {
            report_update(app, state, &label, *from);
        }
        docker::pull(image, cancel, |progress| {
            if let Some(app) = app {
                report_update(app, state, &label, progress_within(*from, *to, progress.done, progress.total));
            }
        })
    });

    if let Err(error) = downloaded {
        if cancel.load(Ordering::SeqCst) {
            return Err(error);
        }
        images::remove_images(&before.retag());
        if recorded_here {
            images::Previous::forget();
        }
        return Err(error);
    }

    drop_legacy_sandbox_tag();
    record_image_identity(&config::server_image(), "downloaded");
    if !before.moved() {
        // Nothing new arrived, so there is nothing to go back from.
        if recorded_here {
            images::Previous::forget();
        }
        return Ok(());
    }
    // A release downloaded earlier and never run is replaced by this one, and
    // nothing would ever use it again.
    images::remove_superseded(&images::replaced_unkept(&before, images::Previous::kept().as_ref()));
    Ok(())
}

/// Starts Kayak, and if newly downloaded images crash on starting, goes back to
/// the ones they replaced.
///
/// Returns the port, and the reason the new version was given up on if it was.
/// Kayak only proves a new version by starting it -- possibly at a launch long
/// after it was downloaded in the background -- so this is where a broken
/// release is undone. Only a crash counts: a server that is merely slow to
/// answer may be a good release on a busy Mac, and is waited for, as any start
/// is. The broken release is remembered, and deleted, only once the version
/// before it has started; if that one will not start either, the new version is
/// put back, and kept for another try.
fn start_with_rollback(app: &AppHandle, state: &Launcher) -> Result<(u16, Option<String>), String> {
    let error = match start_server(app, state) {
        Ok(port) => {
            images::confirm();
            return Ok((port, None));
        }
        Err(error) => error,
    };
    let crashed = explain::classify(&error) == Problem::ServerExited;
    // A kept version the tags still point at -- left by a download stopped
    // before anything arrived -- is not an older one to go back to.
    let Some(previous) = images::Previous::kept()
        .filter(|previous| crashed && !stopping(state) && previous.moved())
    else {
        return Err(error);
    };

    logs::record(
        "update",
        &format!("the new version of Kayak would not start ({error}); going back to the one before"),
    );
    show_step(app, state, "Going back to the version you had", "This can take a minute.");
    let newer = images::Previous::note();
    let failed_digest = docker::image_digest(&config::server_image());
    let downloaded = previous.retag();
    // The new version's server is in the way of the name.
    docker::stop_container(config::CONTAINER_NAME)?;
    docker::remove_container(config::CONTAINER_NAME)?;

    match start_server(app, state) {
        Ok(port) => {
            if let Some(digest) = failed_digest {
                images::skip(&digest);
            }
            images::Previous::forget();
            // Nothing uses them now that the previous version is serving.
            images::remove_images(&downloaded);
            Ok((port, Some(error)))
        }
        Err(previous_error) => {
            logs::record(
                "update",
                &format!("the previous version would not start either: {previous_error}"),
            );
            newer.retag();
            Err(error)
        }
    }
}

/// Whether the server running now is from an older image than the installed
/// one -- a version downloaded in the background, waiting to be used.
fn running_an_older_kayak() -> bool {
    if docker::container_state(config::CONTAINER_NAME) != Ok(ContainerState::Running) {
        return false;
    }
    let running = docker::container_image_id(config::CONTAINER_NAME);
    let current = docker::image_id(&config::server_image());
    !should_reuse(running.as_deref(), current.as_deref())
}

/// Brings everything up to date now, because the user asked, showing what it
/// does in the launcher window.
///
/// Kayak first, then the app: installing the app restarts it, which would
/// abandon a Kayak update halfway.
fn check_for_updates_now(app: AppHandle, state: Arc<Launcher>) {
    surface_launcher(&app);
    // A start or an update already under way is waited for: it shows its own
    // progress meanwhile, and the check follows it.
    if state.busy.load(Ordering::SeqCst) && !state.prefetching.load(Ordering::SeqCst) {
        note(&app, "Kayak will check for updates once it has finished what it is doing.");
    }
    let Some(_guard) = claim(&state, true) else {
        return;
    };
    if stopping(&state) {
        return;
    }

    lock(&state.update).checking = true;
    let _ = app.emit("update", lock(&state.update).clone());
    let app_update = find_app_update(&app, UPDATE_CHECK_TIMEOUT);
    // What was found is downloaded even if it would not start before: the user
    // asked for it, and is told if it fails again.
    let download = newer_kayak(false).is_some();
    let restart = download || running_an_older_kayak();
    lock(&state.update).checking = false;
    let _ = app.emit("update", lock(&state.update).clone());

    if !restart && app_update.is_none() {
        note(&app, "Kayak is up to date.");
        return;
    }

    if restart {
        if download {
            if let Err(error) = download_kayak(Some(&app), &state, &state.shutting_down) {
                logs::record("update", &format!("the update could not be downloaded: {error}"));
                if !stopping(&state) {
                    report_update_failure(&app, &state, explain::update_failure(&error));
                }
                return;
            }
        }
        logs::record("update", "restarting Kayak on the new version");
        // Replaced rather than reconnected to: the running server is the old
        // version.
        state.recreate_next.store(true, Ordering::SeqCst);
        match start_with_rollback(&app, &state) {
            Ok((port, rolled_back)) => {
                *lock(&state.port) = Some(port);
                let failed = rolled_back.is_some();
                kayak_is_up(&app, &state, port, rolled_back.as_deref().map(Notice::update_failed));
                {
                    let (app, state) = (app.clone(), state.clone());
                    thread::spawn(move || check_for_updates(app, state));
                }
                if failed {
                    return;
                }
                if app_update.is_none() {
                    note(&app, "Kayak is up to date.");
                }
            }
            Err(error) => {
                if !stopping(&state) {
                    fail(&app, &state, &error);
                }
                return;
            }
        }
    }

    if let Some(update) = app_update {
        let label = if restart {
            "Finishing the update"
        } else {
            "Downloading the update"
        };
        if let Err(error) = install_app_update(&app, &state, &update, label, None) {
            logs::record("update", &format!("the app could not be updated: {error}"));
            if !stopping(&state) {
                report_update_failure(&app, &state, explain::launcher_update_failure(&error).to_string());
            }
        }
    }
}

/// Says that an update did not go through: on the Ready screen when Kayak is
/// serving, and as a failure when it is not.
fn report_update_failure(app: &AppHandle, state: &Launcher, message: String) {
    let serving = lock(&state.port).is_some_and(kayak_answers);
    let stage = if serving {
        Stage::Ready {
            notice: Some(Notice::new(explain::UPDATE_FAILED, message)),
        }
    } else {
        Stage::Failed {
            headline: explain::UPDATE_FAILED.to_string(),
            body: message,
        }
    };
    set_stage(app, state, stage);
    surface_launcher(app);
}

/// The updater for this app, configured for this Mac.
///
/// The Intel build running on an Apple chip is pointed at the Apple-chip build,
/// and offered it even at the same version: it cannot use the graphics chip,
/// and its users have no other way of finding out they downloaded the wrong
/// one. Once installed, the new build runs natively and this no longer applies.
fn launcher_updater(app: &AppHandle, timeout: Duration) -> Result<tauri_plugin_updater::Updater, String> {
    let mut builder = app.updater_builder().timeout(timeout);
    if metal::silicon() == metal::Silicon::Translated {
        builder = builder
            .target("darwin-aarch64")
            .version_comparator(|current, release| release.version >= current);
    }
    builder.build().map_err(|error| error.to_string())
}

/// Asks GitHub whether a newer app has been published. None when there is no
/// newer one, and when GitHub cannot be reached, which is normal offline.
fn find_app_update(app: &AppHandle, timeout: Duration) -> Option<tauri_plugin_updater::Update> {
    let updater = match launcher_updater(app, timeout) {
        Ok(updater) => updater,
        Err(error) => {
            logs::record("update", &format!("could not set up the app updater: {error}"));
            return None;
        }
    };
    match tauri::async_runtime::block_on(updater.check()) {
        Ok(found) => {
            if let Some(update) = &found {
                logs::record("update", &format!("app version {} is available", update.version));
            }
            found
        }
        Err(error) => {
            logs::record("update", &format!("could not check for a new app: {error}"));
            None
        }
    }
}

/// Installs a newer app, if there is one, before Kayak is started.
///
/// A failure is logged and the launch carries on: the app already installed
/// still runs Kayak. So does a download that is merely slow -- it is given up
/// after [`LAUNCH_APP_UPDATE_LIMIT`] and tried again at the next launch.
fn update_app_at_launch(app: &AppHandle, state: &Arc<Launcher>) {
    let Some(update) = find_app_update(app, LAUNCH_UPDATE_CHECK_TIMEOUT) else {
        return;
    };
    let attempted = std::fs::read_to_string(paths::beside_data(LAUNCHER_ATTEMPT_FILE)).ok();
    if !retry_launcher_update(attempted.as_deref(), &update.version) {
        logs::record(
            "update",
            "that app update was installed before and did not take; leaving it to Check for Updates",
        );
        return;
    }
    let installed = install_app_update(
        app,
        state,
        &update,
        "Downloading the update",
        Some(LAUNCH_APP_UPDATE_LIMIT),
    );
    if let Err(error) = installed {
        logs::record("update", &format!("the app could not be updated: {error}"));
    }
}

/// Whether to install `offered` by itself at launch, given the version last
/// installed that way.
///
/// An update installed before and offered again did not take -- a release whose
/// manifest names a version its download does not contain, or the Apple-chip
/// build offered at the same version to an Intel build it did not replace.
/// Installing it again at every launch would restart the app forever.
fn retry_launcher_update(attempted: Option<&str>, offered: &str) -> bool {
    attempted.map(str::trim) != Some(offered)
}

/// Downloads and installs an app update, then restarts into it. Returns only if
/// it failed -- or, given a `limit`, took longer than that.
fn install_app_update(
    app: &AppHandle,
    state: &Arc<Launcher>,
    update: &tauri_plugin_updater::Update,
    label: &str,
    limit: Option<Duration>,
) -> Result<(), String> {
    surface_launcher(app);
    report_update(app, state, label, -1);
    download_launcher_update(app, state, update, label, limit)?;

    logs::record("update", "the new app is installed; restarting");
    // Written before restarting, or the guard against restarting forever is
    // not there when it is needed.
    if let Err(error) = paths::write_beside_data(LAUNCHER_ATTEMPT_FILE, update.version.as_bytes()) {
        logs::record("update", &format!("could not note the update: {error}"));
    }
    remember_open_tab(state);
    report_update(app, state, "Restarting Kayak…", 100);
    // Everything this process started is stopped first: the restart replaces
    // the process, an orphaned container would refuse the name on the way back
    // up, and an orphaned GPU server would keep its model in memory with
    // nothing left to stop it.
    stop_everything(state);
    app.restart();
}

/// Notes the open Kayak tab before restarting into an app update, so the
/// restarted app leaves that tab to reconnect rather than opening a second one.
fn remember_open_tab(state: &Launcher) {
    if let Some(tab) = lock(&state.opened_tab).as_ref() {
        let _ = paths::write_beside_data(OPEN_TAB_FILE, encode_open_tab(tab).as_bytes());
    }
}

/// The tab left open by the app this one restarted from, if it restarted just
/// now.
fn recall_open_tab() -> Option<OpenTab> {
    let path = paths::beside_data(OPEN_TAB_FILE);
    let written = std::fs::metadata(&path).and_then(|metadata| metadata.modified()).ok();
    let body = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    let age = written.and_then(|written| written.elapsed().ok())?;
    decode_open_tab(&body?, age)
}

fn encode_open_tab(tab: &OpenTab) -> String {
    format!("{}\n{}", tab.port, tab.signed_in_as.as_deref().unwrap_or_default())
}

/// Reads a remembered tab, ignoring one too old to be from the restart just
/// made: a tab from some earlier run is long gone.
fn decode_open_tab(written: &str, age: Duration) -> Option<OpenTab> {
    if age > Duration::from_secs(10 * 60) {
        return None;
    }
    let mut lines = written.lines();
    let port = lines.next()?.trim().parse().ok().filter(|port| *port != 0)?;
    let signed_in_as = lines
        .next()
        .map(str::trim)
        .filter(|fingerprint| !fingerprint.is_empty())
        .map(str::to_string);
    Some(OpenTab { port, signed_in_as })
}

/// Downloads a newer Kayak in the background, so the next launch starts it
/// straight away.
///
/// Leaves the running Kayak alone: pulling only moves the tags, and the server
/// keeps running from the image it started from until the next start replaces
/// it. Gives way to anything the user asks for in the meantime.
fn prefetch_kayak(state: &Arc<Launcher>) {
    if newer_kayak(true).is_none() {
        return;
    }
    // Announced before the busy flag is taken, so that [`claim`] never takes a
    // background download for a start and gives up on what the user asked for.
    state.prefetching.store(true, Ordering::SeqCst);
    let Some(_guard) = BusyGuard::acquire(state) else {
        state.prefetching.store(false, Ordering::SeqCst);
        return;
    };
    state.prefetch_cancel.store(false, Ordering::SeqCst);
    // Checked after lowering the flag, so a quit that raised it a moment ago
    // is not missed.
    if stopping(state) {
        state.prefetching.store(false, Ordering::SeqCst);
        return;
    }
    logs::record("update", "downloading the newer Kayak in the background, for the next launch");
    let downloaded = download_kayak(None, state, &state.prefetch_cancel);
    state.prefetching.store(false, Ordering::SeqCst);
    match downloaded {
        Ok(()) => logs::record("update", "the newer Kayak is ready for the next launch"),
        Err(error) => logs::record("update", &format!("the background download stopped: {error}")),
    }
}

/// Checks on a long interval, for the life of the app, and downloads a newer
/// Kayak ahead of the next launch. The app's own update is left for launch,
/// where restarting interrupts nothing.
fn spawn_update_watcher(app: AppHandle, state: Arc<Launcher>) {
    thread::spawn(move || loop {
        thread::sleep(UPDATE_INTERVAL);
        if stopping(&state) {
            return;
        }
        check_for_updates(app.clone(), state.clone());
        prefetch_kayak(&state);
    });
}

/// Downloads and installs the app's update, abandoning it if the download
/// stalls, runs past `limit`, or the app begins quitting.
fn download_launcher_update(
    app: &AppHandle,
    state: &Launcher,
    update: &tauri_plugin_updater::Update,
    label: &str,
    limit: Option<Duration>,
) -> Result<(), String> {
    let began = Instant::now();
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
        if limit.is_some_and(|limit| began.elapsed() >= limit) {
            task.abort();
            return Err("the download was too slow to wait for".to_string());
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
/// Run on its own thread with the launcher window showing what is happening:
/// `docker stop` gives the server time to close its database, and Kayak's tab
/// in the browser cannot say that it is stopping.
///
/// A start or an update under way is given a moment to notice and stop between
/// steps, so the shutdown does not stop a container that the start then
/// recreates. And whatever happens, the app exits within [`QUIT_DEADLINE`].
fn begin_shutdown(app: AppHandle, state: Arc<Launcher>) {
    if state.shutting_down.swap(true, Ordering::SeqCst) {
        return;
    }
    state.prefetch_cancel.store(true, Ordering::SeqCst);
    logs::record("shutdown", "quitting");
    set_stage(&app, &state, Stage::Stopping);
    surface_launcher(&app);

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
    state.prefetch_cancel.store(true, Ordering::SeqCst);
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

/// Closing the launcher window puts it away while Kayak is in use, and quits
/// otherwise.
///
/// Once Kayak is open in the browser, the tab depends on it, so closing this
/// window must not stop it: Quit is what stops Kayak. Before that -- setting up,
/// or a failure before Kayak ever opened -- the window is the app, and closing
/// it quits as it always has.
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
            if stopping(&state) {
                // Left up to say that Kayak is stopping.
                return;
            }
            let in_use = lock(&state.opened_tab).is_some()
                || matches!(*lock(&state.stage), Stage::Ready { .. });
            if in_use {
                hide_launcher(&handle);
            } else {
                begin_shutdown(handle.clone(), state.inner().clone());
            }
        }
    });
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

    // A way back to Kayak once its tab is closed, besides the Dock icon.
    let open = MenuItemBuilder::with_id(MENU_OPEN, "Open Kayak")
        .accelerator("CmdOrCtrl+O")
        .build(app)?;
    let check = MenuItemBuilder::with_id(MENU_CHECK_UPDATES, "Check for Updates…").build(app)?;

    let application = SubmenuBuilder::new(app, "Kayak")
        .about(Some(about))
        .separator()
        .item(&open)
        .item(&check)
        .separator()
        .services()
        .separator()
        .hide()
        .hide_others()
        .show_all()
        .separator()
        .quit_with_text("Quit Kayak")
        .build()?;

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
        .items(&[&application, &window, &help])
        .build()
}

/// Puts the Kayak version into the About panel once it is known.
fn refresh_menu(app: &AppHandle, kayak_version: Option<&str>) {
    if let Ok(menu) = build_menu(app, kayak_version) {
        let _ = app.set_menu(menu);
    }
}

fn handle_menu(app: &AppHandle, id: &str) {
    let Some(state) = app.try_state::<Arc<Launcher>>() else {
        return;
    };
    let state = state.inner().clone();
    match id {
        MENU_OPEN => open_kayak_now(app.clone(), state),
        MENU_CHECK_UPDATES => {
            let app = app.clone();
            thread::spawn(move || check_for_updates_now(app, state));
        }
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

    // Being offline is normal and must not look like something is broken, so
    // an unreachable Docker Hub only means nothing is known to be newer. With
    // nothing installed there is nothing to compare either: the first start
    // downloads Kayak anyway, so that is not an "update".
    let available = match (&result, &installed) {
        (Ok(remote), Some(local)) => local != &remote.digest,
        _ => false,
    };
    let info = UpdateInfo {
        available,
        checking: false,
        installed: display,
    };

    // Read from the result rather than from the stored state, which still holds
    // the previous check at this point and would leave the menu a check behind.
    refresh_menu(&app, info.installed.as_deref());

    set_update(&app, &state, info);
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
/// left Kayak's tab on a dead page with no explanation and no way back short of
/// quitting. Quiet whenever Kayak is not meant to be running, or a start or an
/// update is replacing it on purpose; a background download is neither.
fn spawn_health_watch(app: AppHandle, state: Arc<Launcher>) {
    thread::spawn(move || {
        let mut misses = 0;
        while !stopping(&state) {
            thread::sleep(WATCH_INTERVAL);
            let replacing =
                state.busy.load(Ordering::SeqCst) && !state.prefetching.load(Ordering::SeqCst);
            let watching = !replacing && matches!(*lock(&state.stage), Stage::Ready { .. });
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
    // Not waited for: a start or an update under way is replacing Kayak on
    // purpose, and will notice anything wrong itself.
    let Some(guard) = claim(state, false) else {
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

/// Opens Kayak in the browser, from the launcher window's Ready screen.
#[tauri::command]
fn open_kayak(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) {
    open_kayak_now(app, state.inner().clone());
}

/// Quits, stopping Kayak and everything it started.
#[tauri::command]
fn quit(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) {
    begin_shutdown(app, state.inner().clone());
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
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Arc::new(Launcher::new()))
        .invoke_handler(tauri::generate_handler![
            snapshot,
            start,
            open_docker,
            open_kayak,
            quit,
            copy_details,
        ])
        .on_menu_event(|app, event| handle_menu(app, event.id().as_ref()))
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
            *lock(&state.opened_tab) = recall_open_tab();
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
            // Clicking the Dock icon, or opening the app again from Finder,
            // means "show me Kayak": in the browser when it is ready, otherwise
            // the launcher window, which says what is happening.
            RunEvent::Reopen { .. } => {
                if let Some(state) = app.try_state::<Arc<Launcher>>() {
                    open_kayak_now(app.clone(), state.inner().clone());
                }
            }
            // Quitting from the menu, the Dock or Cmd+Q arrives here, and the
            // stop takes seconds: doing it inline would freeze the app.
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
            // keep serving an app that is no longer there to stop it.
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

    #[test]
    fn an_app_update_is_installed_by_itself_once() {
        assert!(retry_launcher_update(None, "1.4.0"));
        assert!(retry_launcher_update(Some("1.3.5"), "1.4.0"));
        // Installed before and offered again: it did not take. That includes
        // the Apple-chip build offered to an Intel build at the same version,
        // which would otherwise be installed again at every launch.
        assert!(!retry_launcher_update(Some("1.4.0\n"), "1.4.0"));
    }

    #[test]
    fn a_tab_is_remembered_across_a_restart_and_only_that_restart() {
        let tab = OpenTab {
            port: 8000,
            signed_in_as: Some("0123456789ab".to_string()),
        };
        let written = encode_open_tab(&tab);

        assert_eq!(decode_open_tab(&written, Duration::from_secs(20)), Some(tab));
        assert_eq!(
            decode_open_tab("8001\n", Duration::ZERO),
            Some(OpenTab {
                port: 8001,
                signed_in_as: None
            })
        );
        // Long enough ago that it is not the restart just made.
        assert_eq!(decode_open_tab(&written, Duration::from_secs(3600)), None);
        assert_eq!(decode_open_tab("garbage", Duration::ZERO), None);
        assert_eq!(decode_open_tab("0\n", Duration::ZERO), None);
    }

    #[test]
    fn a_tab_is_left_to_reconnect_only_while_it_is_still_signed_in() {
        let key = fingerprint("0123456789abcdef0123456789abcdef");
        let tab = OpenTab {
            port: 8000,
            signed_in_as: Some(key.clone()),
        };

        assert!(tab_reconnects(Some(&tab), 8000, Some(&key)));
        // Another port, another key, or a server that only now has one: the tab
        // cannot sign itself in, so a new one is opened.
        assert!(!tab_reconnects(Some(&tab), 8001, Some(&key)));
        assert!(!tab_reconnects(Some(&tab), 8000, Some("fedcba987654")));
        let unsigned = OpenTab {
            port: 8000,
            signed_in_as: None,
        };
        assert!(!tab_reconnects(Some(&unsigned), 8000, Some(&key)));
        assert!(tab_reconnects(Some(&unsigned), 8000, None));
        // A tab that could not be signed in is not remembered at all.
        assert!(!tab_reconnects(None, 8000, None));
    }

    #[test]
    fn a_fingerprint_does_not_hold_the_key() {
        let key = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(fingerprint(key).len(), 12);
        assert!(key.starts_with(&fingerprint(key)));
    }

    #[test]
    fn a_half_finished_download_still_counts_as_out_of_date() {
        assert!(!kayak_outdated("sha256:s1", Some("sha256:b1"), "sha256:s1", Some("sha256:b1")));
        assert!(kayak_outdated("sha256:s1", Some("sha256:b1"), "sha256:s2", Some("sha256:b2")));
        // The server arrived, the sandbox did not: not up to date.
        assert!(kayak_outdated("sha256:s2", Some("sha256:b1"), "sha256:s2", Some("sha256:b2")));
        // A sandbox digest that is not known says nothing either way.
        assert!(!kayak_outdated("sha256:s2", None, "sha256:s2", Some("sha256:b2")));
        assert!(!kayak_outdated("sha256:s2", Some("sha256:b1"), "sha256:s2", None));
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

}
