//! Launcher for Kayak.
//!
//! Kayak ships as a Docker image. This app exists so that running it never
//! requires a terminal: it finds Docker, downloads the images, starts the
//! server, shows Kayak in a desktop window, and watches Docker Hub so a new
//! release can be installed from a button rather than a `docker pull`.

mod config;
mod control;
mod docker;
mod metal;
mod paths;
mod registry;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::menu::{AboutMetadata, Menu, MenuBuilder, SubmenuBuilder};
use tauri::{
    AppHandle, Emitter, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_updater::UpdaterExt;

const LAUNCHER_WINDOW: &str = "launcher";
const KAYAK_WINDOW: &str = "kayak";

/// Injected into the Kayak window so the launcher can draw an update banner
/// over a page it does not control.
const BANNER_JS: &str = include_str!("banner.js");

/// Prefix for the fake navigations the injected banner uses to call back in.
const ACTION_PREFIX: &str = "/__launcher/";

/// Gap between background update checks.
const UPDATE_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);

/// What the launcher window is currently showing.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Stage {
    Checking,
    /// Docker is not on this machine at all.
    DockerMissing {
        url: String,
    },
    /// Docker is installed but its daemon is not answering.
    DockerAsleep {
        /// Whether the launcher can start Docker itself. On Linux the daemon is
        /// owned by the init system and needs privileges the launcher lacks.
        startable: bool,
    },
    Downloading {
        label: String,
        step: u32,
        steps: u32,
        done: u32,
        total: u32,
    },
    Starting {
        detail: String,
    },
    Ready {
        url: String,
    },
    Failed {
        message: String,
        detail: String,
    },
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

pub struct Launcher {
    stage: Mutex<Stage>,
    update: Mutex<UpdateInfo>,
    port: Mutex<Option<u16>>,
    /// The Metal inference server, when one is running. Native to the host
    /// rather than containerised, because Metal has no passthrough into Docker.
    metal: Mutex<Option<metal::Server>>,
    /// A launcher update waiting to be installed, held so the button in the
    /// Kayak window can apply the one already found rather than checking again.
    launcher_update: Mutex<Option<tauri_plugin_updater::Update>>,
    /// Set once the app has begun stopping, so the close handler runs its
    /// sequence once and the second close request is allowed through.
    shutting_down: AtomicBool,
    /// Guards the boot and update sequences so a double-click cannot run two
    /// `docker run` calls against the same container name.
    busy: AtomicBool,
}

impl Launcher {
    fn new() -> Self {
        Self {
            stage: Mutex::new(Stage::Checking),
            update: Mutex::new(UpdateInfo::default()),
            port: Mutex::new(None),
            metal: Mutex::new(None),
            launcher_update: Mutex::new(None),
            shutting_down: AtomicBool::new(false),
            busy: AtomicBool::new(false),
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

fn set_stage(app: &AppHandle, state: &Launcher, stage: Stage) {
    *state.stage.lock().unwrap() = stage.clone();
    let _ = app.emit("stage", stage);
}

fn set_update(app: &AppHandle, state: &Launcher, update: UpdateInfo) {
    *state.update.lock().unwrap() = update.clone();
    let _ = app.emit("update", update);
}

/// Waits for the Kayak server to answer its health endpoint.
///
/// The container is polled alongside the endpoint so that a server which exits
/// on startup -- a bad image, a port clash inside the container -- surfaces its
/// logs immediately instead of after the full timeout.
fn wait_for_health(port: u16, timeout: Duration) -> Result<(), String> {
    let url = format!("http://127.0.0.1:{port}/api/health");
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if ureq::get(&url)
            .timeout(Duration::from_secs(3))
            .call()
            .is_ok()
        {
            return Ok(());
        }

        if docker::container_state(config::CONTAINER_NAME) != docker::ContainerState::Running {
            let logs = docker::container_logs(config::CONTAINER_NAME, 20);
            return Err(if logs.is_empty() {
                "The Kayak server stopped right after starting.".to_string()
            } else {
                format!("The Kayak server stopped right after starting:\n{logs}")
            });
        }

        thread::sleep(Duration::from_millis(600));
    }

    Err("The Kayak server did not finish starting in time.".to_string())
}

/// Ensures both images are present locally, downloading whichever are missing.
fn ensure_images(app: &AppHandle, state: &Launcher, force: bool) -> Result<(), String> {
    let server = config::server_image();
    let sandbox = config::sandbox_image();

    let mut wanted: Vec<(&str, &str)> = Vec::new();
    if force || !docker::has_image(&server) {
        wanted.push((server.as_str(), "Kayak"));
    }
    // The server starts sandboxes from an unqualified local tag, so a missing
    // retag is as bad as a missing image.
    if force || !docker::has_image(&sandbox) || !docker::has_image(config::SANDBOX_LOCAL_TAG) {
        wanted.push((sandbox.as_str(), "Agent sandbox"));
    }

    let steps = wanted.len() as u32;
    for (index, (image, label)) in wanted.iter().enumerate() {
        let step = index as u32 + 1;
        set_stage(
            app,
            state,
            Stage::Downloading {
                label: (*label).to_string(),
                step,
                steps,
                done: 0,
                total: 0,
            },
        );

        docker::pull(image, |progress| {
            set_stage(
                app,
                state,
                Stage::Downloading {
                    label: (*label).to_string(),
                    step,
                    steps,
                    done: progress.done,
                    total: progress.total,
                },
            );
        })?;
    }

    // Point the name the server looks for at whatever was just pulled. Done
    // unconditionally so an update repoints the tag as well.
    docker::tag(&sandbox, config::SANDBOX_LOCAL_TAG)?;
    Ok(())
}

/// Brings Kayak up, from a cold machine to a served UI.
fn boot(app: AppHandle, state: Arc<Launcher>) {
    let Some(_guard) = BusyGuard::acquire(&state) else {
        return;
    };

    set_stage(&app, &state, Stage::Checking);

    if !docker::is_installed() {
        set_stage(
            &app,
            &state,
            Stage::DockerMissing {
                url: config::DOCKER_INSTALL_URL.to_string(),
            },
        );
        return;
    }

    if !docker::is_daemon_running() {
        set_stage(
            &app,
            &state,
            Stage::DockerAsleep {
                startable: cfg!(any(target_os = "macos", target_os = "windows")),
            },
        );
        return;
    }

    if let Err(error) = start_server(&app, &state) {
        set_stage(
            &app,
            &state,
            Stage::Failed {
                message: "Kayak could not be started".to_string(),
                detail: error,
            },
        );
        return;
    }

    let port = state.port.lock().unwrap().unwrap_or(config::PREFERRED_PORT);
    let url = format!("http://127.0.0.1:{port}");
    set_stage(&app, &state, Stage::Ready { url: url.clone() });

    if let Err(error) = show_kayak(&app, &url) {
        set_stage(
            &app,
            &state,
            Stage::Failed {
                message: "Kayak is running but its window could not be opened".to_string(),
                detail: error,
            },
        );
        return;
    }

    // The launcher window has done its job; from here the update banner inside
    // the Kayak window is the only launcher surface the user needs.
    if let Some(window) = app.get_webview_window(LAUNCHER_WINDOW) {
        let _ = window.hide();
    }

    // Started only once Kayak is serving: the control channel lives in the
    // data directory, which is not seeded until the container has run.
    if let Ok(data_dir) = paths::ensure_data_dir() {
        spawn_metal_reconciler(state.clone(), data_dir);
    }

    spawn_update_watcher(app, state);
}

/// Reuses a running container, or creates one, and waits until it serves.
fn start_server(app: &AppHandle, state: &Launcher) -> Result<(), String> {
    let data_dir = paths::ensure_data_dir()?;

    // A container left running by a previous launch is reused as-is: recreating
    // it would drop whatever agents are mid-run inside it.
    if docker::container_state(config::CONTAINER_NAME) == docker::ContainerState::Running {
        if let Some(port) = docker::published_port(config::CONTAINER_NAME) {
            set_stage(
                app,
                state,
                Stage::Starting {
                    detail: "Reconnecting to Kayak".to_string(),
                },
            );
            wait_for_health(port, Duration::from_secs(config::HEALTH_TIMEOUT_SECS))?;
            *state.port.lock().unwrap() = Some(port);
            return Ok(());
        }
    }

    ensure_images(app, state, false)?;

    // Must happen before the container runs: the bind mount hides whatever the
    // image ships at /app/data, so the defaults are lifted out of the image
    // first and become the initial contents of the user's own data directory.
    if paths::is_unseeded(&data_dir) {
        set_stage(
            app,
            state,
            Stage::Starting {
                detail: "Setting up your workspace".to_string(),
            },
        );
        docker::copy_out_of_image(&config::server_image(), "/app/data", &data_dir)?;
    }

    set_stage(
        app,
        state,
        Stage::Starting {
            detail: "Starting Kayak".to_string(),
        },
    );

    // A stopped container may have been created with a port that is now taken,
    // or from an image that has since been replaced, so it is rebuilt rather
    // than restarted. All state lives in the mounted data directory, so nothing
    // is lost by doing so.
    docker::remove_container(config::CONTAINER_NAME)?;

    let port = docker::find_free_port(config::PREFERRED_PORT, config::PORT_SCAN_RANGE)?;
    docker::run_container(&docker::RunSpec {
        image: &config::server_image(),
        name: config::CONTAINER_NAME,
        port,
        data_dir: &data_dir,
    })?;

    wait_for_health(port, Duration::from_secs(config::HEALTH_TIMEOUT_SECS))?;
    *state.port.lock().unwrap() = Some(port);
    Ok(())
}

/// Opens (or focuses) the window that shows Kayak itself.
fn show_kayak(app: &AppHandle, url: &str) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(KAYAK_WINDOW) {
        let _ = window.show();
        let _ = window.set_focus();
        return Ok(());
    }

    let parsed = url
        .parse()
        .map_err(|err| format!("Could not parse {url}: {err}"))?;

    let navigation_handle = app.clone();
    let close_handle = app.clone();

    WebviewWindowBuilder::new(app, KAYAK_WINDOW, WebviewUrl::External(parsed))
        .title("Kayak")
        .inner_size(1280.0, 860.0)
        .min_inner_size(900.0, 600.0)
        .center()
        .initialization_script(BANNER_JS)
        .on_navigation(move |url| {
            let Some(action) = url.path().strip_prefix(ACTION_PREFIX) else {
                return true;
            };
            handle_banner_action(&navigation_handle, action.to_string());
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

fn show_banner(app: &AppHandle, update: &UpdateInfo) {
    eval_in_kayak(
        app,
        &format!(
            "window.__kayakLauncher && window.__kayakLauncher.showUpdate({{date: {}}})",
            js_literal(&update.date)
        ),
    );
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

    let window = SubmenuBuilder::new(app, "Window")
        .minimize()
        .separator()
        .close_window()
        .build()?;

    MenuBuilder::new(app)
        .items(&[&application, &edit, &window])
        .build()
}

/// Puts the Kayak version into the About panel once it is known.
fn refresh_menu(app: &AppHandle, kayak_version: Option<&str>) {
    if let Ok(menu) = build_menu(app, kayak_version) {
        let _ = app.set_menu(menu);
    }
}

/// Draws whatever the user should currently be told, into the Kayak window.
///
/// Called both when something changes and when the page announces itself, so a
/// banner survives a reload and no longer depends on the webview happening to
/// have a document at the moment a check finished.
fn push_banner_state(app: &AppHandle, state: &Launcher) {
    // A launcher update wins when both are pending: installing it restarts the
    // app, which would abandon a Kayak update halfway.
    if let Some(update) = state.launcher_update.lock().unwrap().as_ref() {
        eval_in_kayak(
            app,
            &format!(
                "window.__kayakLauncher && window.__kayakLauncher.showUpdate({{kind: \"launcher\", version: {}}})",
                js_literal(&update.version)
            ),
        );
        return;
    }

    let update = state.update.lock().unwrap().clone();
    if update.available {
        show_banner(app, &update);
    }
}

/// Asks GitHub whether a newer launcher has been published.
///
/// Runs in Rust rather than the launcher window's page because that window is
/// hidden once Kayak opens, so an update found there would never be seen.
fn check_launcher_update(app: AppHandle, state: Arc<Launcher>) {
    let Ok(updater) = app.updater() else {
        return;
    };
    let found = match tauri::async_runtime::block_on(updater.check()) {
        Ok(found) => found,
        // Offline, or no release published yet. Neither is worth reporting.
        Err(_) => return,
    };
    let Some(update) = found else {
        return;
    };

    let version = update.version.clone();
    *state.launcher_update.lock().unwrap() = Some(update);
    let _ = app.emit("launcher-update", version);
    push_banner_state(&app, &state);
}

/// Downloads and installs the launcher update, then restarts into it.
fn apply_launcher_update(app: AppHandle, state: Arc<Launcher>) {
    let Some(update) = state.launcher_update.lock().unwrap().take() else {
        return;
    };

    show_banner_progress(&app, "Downloading the new app", -1);
    let outcome =
        tauri::async_runtime::block_on(update.download_and_install(|_, _| {}, || {}));

    match outcome {
        Ok(()) => {
            // The container is stopped first: the restart replaces this process,
            // and an orphaned container would then refuse the name on the way
            // back up.
            shutdown();
            app.restart();
        }
        Err(error) => {
            eval_in_kayak(
                &app,
                &format!(
                    "window.__kayakLauncher && window.__kayakLauncher.showError({})",
                    js_literal(&format!("The app could not be updated: {error}"))
                ),
            );
        }
    }
}

/// Stops everything the launcher started, then quits.
///
/// Run on its own thread with the window still up: `docker stop` gives the
/// server ten seconds to close its database, and doing that on the main thread
/// froze the window for the duration, which reads as a crash rather than a
/// shutdown.
fn begin_shutdown(app: AppHandle, state: Arc<Launcher>) {
    if state.shutting_down.swap(true, Ordering::SeqCst) {
        return;
    }
    eval_in_kayak(&app, "window.__kayakLauncher && window.__kayakLauncher.showShutdown()");

    thread::spawn(move || {
        if let Some(server) = state.metal.lock().unwrap().take() {
            server.stop();
        }
        shutdown();
        app.exit(0);
    });
}

/// Responds to a button press in the injected banner.
fn handle_banner_action(app: &AppHandle, action: String) {
    let Some(state) = app.try_state::<Arc<Launcher>>() else {
        return;
    };
    let state = state.inner().clone();

    match action.as_str() {
        // The page has a document and can be drawn on. Anything found before
        // this point was pushed into a webview that could not receive it.
        "ready" => push_banner_state(app, &state),
        "update" => {
            let app = app.clone();
            thread::spawn(move || apply_update(app, state));
        }
        "self-update" => {
            let app = app.clone();
            thread::spawn(move || apply_launcher_update(app, state));
        }
        "dismiss" => {
            let mut update = state.update.lock().unwrap();
            update.available = false;
        }
        _ => {}
    }
}

/// Compares the installed image against what Docker Hub currently publishes.
fn check_for_updates(app: AppHandle, state: Arc<Launcher>) {
    {
        let mut update = state.update.lock().unwrap();
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

/// Checks at startup and then on a long interval, for the life of the app.
fn spawn_update_watcher(app: AppHandle, state: Arc<Launcher>) {
    thread::spawn(move || loop {
        check_for_updates(app.clone(), state.clone());
        // Checked on the same schedule so a launcher released while the app is
        // open is offered too, rather than only at the next cold start.
        check_launcher_update(app.clone(), state.clone());
        thread::sleep(UPDATE_INTERVAL);
    });
}

/// Pulls the new images and replaces the running container with them.
fn apply_update(app: AppHandle, state: Arc<Launcher>) {
    let Some(_guard) = BusyGuard::acquire(&state) else {
        return;
    };

    show_banner_progress(&app, "Downloading the new version", -1);

    // Noted before the pull: repointing a tag leaves the image it replaced
    // behind, untagged and several gigabytes large, and after the pull there is
    // no way to tell which of the untagged images used to be ours.
    let superseded: Vec<String> = [config::server_image(), config::sandbox_image()]
        .iter()
        .filter_map(|image| docker::image_id(image))
        .collect();

    let result = (|| -> Result<u16, String> {
        // `force` because the tags are already present locally; the point of the
        // pull is to move them to the newly published digest.
        ensure_images_for_update(&app, true)?;

        show_banner_progress(&app, "Restarting Kayak", 80);
        let data_dir = paths::ensure_data_dir()?;

        // Stopped gracefully rather than killed: the server owns a SQLite
        // database, and a SIGKILL mid-write can leave it corrupt.
        docker::stop_container(config::CONTAINER_NAME)?;
        docker::remove_container(config::CONTAINER_NAME)?;

        let port = docker::find_free_port(config::PREFERRED_PORT, config::PORT_SCAN_RANGE)?;
        docker::run_container(&docker::RunSpec {
            image: &config::server_image(),
            name: config::CONTAINER_NAME,
            port,
            data_dir: &data_dir,
        })?;

        show_banner_progress(&app, "Waiting for Kayak to come back", 92);
        wait_for_health(port, Duration::from_secs(config::HEALTH_TIMEOUT_SECS))?;
        Ok(port)
    })();

    match result {
        Ok(port) => {
            // Only now, with the new container running: Docker refuses to
            // remove an image a container still references, so doing this any
            // earlier would fail on the very image being replaced. Failures are
            // ignored, since anything still in use should stay.
            for id in superseded {
                if docker::image_id(&config::server_image()).as_deref() != Some(id.as_str())
                    && docker::image_id(&config::sandbox_image()).as_deref() != Some(id.as_str())
                {
                    let _ = docker::remove_image(&id);
                }
            }

            *state.port.lock().unwrap() = Some(port);
            let url = format!("http://127.0.0.1:{port}");
            set_update(
                &app,
                &state,
                UpdateInfo {
                    available: false,
                    ..Default::default()
                },
            );
            set_stage(&app, &state, Stage::Ready { url: url.clone() });

            // The port can change across a restart, so the window is pointed at
            // the new address rather than simply reloaded.
            if let Some(window) = app.get_webview_window(KAYAK_WINDOW) {
                if let Ok(parsed) = url.parse() {
                    let _ = window.navigate(parsed);
                } else {
                    let _ = window.eval("location.reload()");
                }
            }
        }
        Err(error) => {
            eval_in_kayak(
                &app,
                &format!(
                    "window.__kayakLauncher && window.__kayakLauncher.showError({})",
                    js_literal(&error)
                ),
            );
        }
    }
}

/// Pull step of an update, without the launcher-window progress reporting.
fn ensure_images_for_update(app: &AppHandle, force: bool) -> Result<(), String> {
    let server = config::server_image();
    let sandbox = config::sandbox_image();

    for (image, label) in [(server.as_str(), "Kayak"), (sandbox.as_str(), "sandbox")] {
        if !force && docker::has_image(image) {
            continue;
        }
        docker::pull(image, |progress| {
            let percent = if progress.total > 0 {
                // Capped below the restart steps, which own the rest of the bar.
                (progress.done * 70 / progress.total.max(1)) as i32
            } else {
                -1
            };
            show_banner_progress(app, &format!("Downloading {label}"), percent);
        })?;
    }

    docker::tag(&sandbox, config::SANDBOX_LOCAL_TAG)?;
    Ok(())
}

/// Port the Metal server listens on.
///
/// Deliberately the same port the containerised vLLM would publish, so Kayak
/// reaches either backend through the one `VLLM_API_BASE` it already has.
const METAL_PORT: u16 = 8001;

/// Drives the Metal server towards whatever Kayak has asked for.
///
/// Runs on its own thread and is allowed to block: installing the environment
/// downloads gigabytes and takes minutes, and doing that inline keeps the whole
/// sequence in one place instead of spread across a state machine.
fn reconcile_metal(state: &Launcher, paths: &control::ControlPaths) {
    let supported = metal::is_apple_silicon();
    let mut status = control::MetalStatus {
        supported,
        installed: metal::is_installed(),
        state: "stopped".to_string(),
        port: METAL_PORT,
        ..Default::default()
    };

    if !supported {
        // Nothing else can be true on this machine, and saying so lets Kayak
        // hide the option rather than offer something that cannot work.
        let _ = control::write_status(&paths.status, &control::Status { metal: status });
        return;
    }

    let desired = control::read_desired(&paths.desired);
    let wanted = desired
        .metal
        .running
        .then(|| desired.metal.model.clone())
        .flatten()
        .filter(|model| control::accepts_model(model));

    let Some(model) = wanted else {
        // Either nothing is wanted or what was asked for is not servable; both
        // mean any running server should come down.
        if let Some(server) = state.metal.lock().unwrap().take() {
            server.stop();
        }
        if desired.metal.running {
            status.error = Some(
                "Metal inference serves MLX models only, published under mlx-community."
                    .to_string(),
            );
            status.state = "error".to_string();
        }
        let _ = control::write_status(&paths.status, &control::Status { metal: status });
        return;
    };

    // A server already serving the right model just needs its health reported.
    {
        let mut running = state.metal.lock().unwrap();
        if let Some(server) = running.as_mut() {
            if server.model == model && server.is_alive() {
                status.model = Some(model);
                status.state = if metal::is_healthy(METAL_PORT) {
                    "ready"
                } else {
                    "starting"
                }
                .to_string();
                let _ = control::write_status(&paths.status, &control::Status { metal: status });
                return;
            }
        }
        // Wrong model, or the process died on its own.
        if let Some(server) = running.take() {
            server.stop();
        }
    }

    if !metal::is_installed() {
        status.state = "installing".to_string();
        status.model = Some(model.clone());
        let _ = control::write_status(&paths.status, &control::Status { metal: status.clone() });

        if let Err(error) = metal::run_install(|_| {}) {
            status.state = "error".to_string();
            status.error = Some(error);
            let _ = control::write_status(&paths.status, &control::Status { metal: status });
            return;
        }
        status.installed = true;
    }

    match metal::spawn_server(&model, METAL_PORT) {
        Ok(server) => {
            *state.metal.lock().unwrap() = Some(server);
            status.state = "starting".to_string();
            status.model = Some(model);
        }
        Err(error) => {
            status.state = "error".to_string();
            status.error = Some(error);
        }
    }
    let _ = control::write_status(&paths.status, &control::Status { metal: status });
}

/// Polls the control channel for as long as the app runs.
fn spawn_metal_reconciler(state: Arc<Launcher>, data_dir: std::path::PathBuf) {
    thread::spawn(move || {
        let paths = control::ControlPaths::under(&data_dir);
        let _ = std::fs::create_dir_all(&paths.dir);
        loop {
            reconcile_metal(&state, &paths);
            thread::sleep(Duration::from_secs(2));
        }
    });
}

/// Stops the container the launcher started.
fn shutdown() {
    let _ = docker::stop_container(config::CONTAINER_NAME);
    let _ = docker::remove_container(config::CONTAINER_NAME);
}

#[tauri::command]
fn snapshot(state: tauri::State<'_, Arc<Launcher>>) -> Snapshot {
    Snapshot {
        stage: state.stage.lock().unwrap().clone(),
        update: state.update.lock().unwrap().clone(),
    }
}

#[tauri::command]
fn start(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) {
    let state = state.inner().clone();
    thread::spawn(move || boot(app, state));
}

/// Starts Docker Desktop, then waits for the daemon before continuing to boot.
#[tauri::command]
fn start_docker(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) {
    let state = state.inner().clone();
    thread::spawn(move || {
        if let Err(error) = docker::start_desktop() {
            set_stage(
                &app,
                &state,
                Stage::Failed {
                    message: "Docker could not be started".to_string(),
                    detail: error,
                },
            );
            return;
        }

        set_stage(
            &app,
            &state,
            Stage::Starting {
                detail: "Waiting for Docker to start".to_string(),
            },
        );

        // Docker Desktop takes a while to come up, and reports nothing until it
        // does, so the only option is to wait for the daemon to start answering.
        let deadline = Instant::now() + Duration::from_secs(120);
        while Instant::now() < deadline {
            if docker::is_daemon_running() {
                boot(app, state);
                return;
            }
            thread::sleep(Duration::from_secs(2));
        }

        set_stage(
            &app,
            &state,
            Stage::Failed {
                message: "Docker did not finish starting".to_string(),
                detail: "Open Docker Desktop manually, then try again.".to_string(),
            },
        );
    });
}

/// Shows the Kayak window, for when the launcher window is the one in focus.
#[tauri::command]
fn open_kayak(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) -> Result<(), String> {
    let port = state
        .port
        .lock()
        .unwrap()
        .ok_or_else(|| "Kayak is not running yet".to_string())?;
    show_kayak(&app, &format!("http://127.0.0.1:{port}"))
}

/// Installs a launcher update already found by the background check.
#[tauri::command]
fn install_launcher_update(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) {
    let state = state.inner().clone();
    thread::spawn(move || apply_launcher_update(app, state));
}

#[tauri::command]
fn check_updates(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) {
    let state = state.inner().clone();
    thread::spawn(move || check_for_updates(app, state));
}

#[tauri::command]
fn install_update(app: AppHandle, state: tauri::State<'_, Arc<Launcher>>) {
    let state = state.inner().clone();
    thread::spawn(move || apply_update(app, state));
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_process::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .manage(Arc::new(Launcher::new()))
        .invoke_handler(tauri::generate_handler![
            snapshot,
            start,
            start_docker,
            open_kayak,
            check_updates,
            install_update,
            install_launcher_update,
        ])
        .setup(|app| {
            let handle = app.handle().clone();
            refresh_menu(&handle, None);
            let state = app.state::<Arc<Launcher>>().inner().clone();
            thread::spawn(move || boot(handle, state));
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("could not start the launcher")
        .run(|app, event| match event {
            // Quitting from the menu, the Dock or Cmd+Q arrives here rather
            // than as a window close, and it needs the same treatment: the
            // stop takes seconds, and doing it inline freezes the window.
            RunEvent::ExitRequested { api, .. } => {
                let Some(state) = app.try_state::<Arc<Launcher>>() else {
                    return;
                };
                if state.shutting_down.load(Ordering::SeqCst) {
                    // Already stopping; this is the exit that sequence asked
                    // for, so let it through.
                    return;
                }
                api.prevent_exit();
                begin_shutdown(app.clone(), state.inner().clone());
            }
            // Last resort. Both the container and the Metal server outlive this
            // process unless they are stopped, and either left running would
            // keep serving an app that no longer has a window.
            RunEvent::Exit => {
                let already = app
                    .try_state::<Arc<Launcher>>()
                    .is_some_and(|state| state.shutting_down.swap(true, Ordering::SeqCst));
                if already {
                    return;
                }
                if let Some(state) = app.try_state::<Arc<Launcher>>() {
                    if let Some(server) = state.metal.lock().unwrap().take() {
                        server.stop();
                    }
                }
                shutdown();
            }
            _ => {}
        });
}
