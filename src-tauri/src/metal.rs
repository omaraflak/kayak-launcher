//! Metal inference on Apple Silicon.
//!
//! Kayak normally runs vLLM as a Docker container, which cannot work here: on
//! macOS the Docker daemon lives in a Linux VM and there is no GPU passthrough
//! for Metal, so a container has no route to the GPU at all. The inference
//! server therefore has to be a native macOS process, and the launcher is the
//! only part of Kayak already running on the host to start one.
//!
//! Installation is delegated to the upstream `install.sh`, which resolves the
//! arm64 Python 3.12 and the macOS wheels itself. Reimplementing that here
//! would mean tracking a wheel URL that upstream expects to change.
//!
//! Everything here reports failure as a [`Failure`]: one sentence Kayak shows
//! the user as it is, plus what the process printed, which Kayak keeps for its
//! logs panel. Users of Kayak have never heard of vLLM, a virtualenv or Metal,
//! and the installer's stderr used to be the whole explanation they got.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::lock;
use crate::process::{self, Group, RunError};

/// Upstream installer, pinned to a release tag.
///
/// Fetched from a tag rather than `main` so a change upstream cannot break
/// every new install the moment it lands. The script still picks the wheels
/// itself -- see [`INSTALL_CHANNEL`] -- so what the tag pins is the installation
/// logic, not the version of vLLM-Metal installed.
const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/vllm-project/vllm-metal/v0.32.0/install.sh";

/// Asks the installer for the newest tagged release instead of its default,
/// the development build cut from `main` every day. A tagged release is what
/// someone who cannot debug a broken nightly should be given.
const INSTALL_CHANNEL: &str = "--stable";

/// Where `install.sh` places the virtualenv when run without a checkout.
const VENV_DIR: &str = ".venv-vllm-metal";

/// Written into the virtualenv once an install has finished and been checked.
///
/// The CLI existing is not proof of a working install: the installer puts the
/// vLLM core in place before the Metal plugin, so an install interrupted in
/// between left a `vllm` that could never serve a model -- and because the CLI
/// was there, it was never installed again.
const COMPLETE_MARKER: &str = ".kayak-complete";

/// Written into the virtualenv when the server shows it is broken in a way the
/// install checks cannot see -- a module it needs will not import -- so the
/// next install repairs it instead of accepting it again.
const REPAIR_MARKER: &str = ".kayak-repair";

/// Name of the file, beside Kayak's data folder, that records that Kayak
/// itself created the virtualenv and has not finished installing it. See
/// [`created_by_kayak`].
const OWNERSHIP_FILE: &str = "gpu-environment-created-by-kayak";

/// How long the GPU server is given to exit cleanly before it is killed.
const STOP_GRACE: Duration = Duration::from_secs(10);

/// Lines of a process's output kept in memory, for explaining a failure.
const OUTPUT_LINES: usize = 200;

/// Lines of that output passed on to Kayak with a failure.
const DETAIL_LINES: usize = 50;

/// How long a server that has been ready may go without answering a health
/// check before it is reported as not responding.
///
/// Measured in time rather than missed checks, and long, because the cost of
/// being wrong is high: Kayak releases a model reported as not responding,
/// and a Mac that is swapping can leave a perfectly healthy server unable to
/// answer for a minute at a time.
const UNRESPONSIVE_AFTER: Duration = Duration::from_secs(120);

pub const NOT_FOR_MAC: &str =
    "This model can't run on your Mac's graphics chip. Pick one made for Mac.";
pub const NEEDS_APPLE_BUILD: &str = "You have the Intel version of Kayak, but this Mac has an \
     Apple chip. Download the Apple-chip version of Kayak to use your Mac's graphics chip.";
const INSTALL_OFFLINE: &str = "Couldn't download what Kayak needs to use your Mac's graphics \
     chip. Check your internet connection and try again.";
const SETUP_FAILED: &str =
    "Couldn't set up your Mac's graphics chip for Kayak. Try again in a few minutes.";
const OUT_OF_STORAGE: &str = "Your Mac is out of storage space. Free up some space and try again.";
const OUT_OF_MEMORY: &str = "This model needs more memory than your Mac has free. Close other \
     apps or pick a smaller model.";
const UNSUPPORTED_MODEL: &str =
    "This model can't run on your Mac's graphics chip yet. Pick a different model.";
const MODEL_UNAVAILABLE: &str = "This model couldn't be downloaded. It may have been removed or \
     need permission. Pick a different model.";
const CRASHED_LOADING: &str =
    "The model stopped while it was loading. Try again, or pick a different model.";
const CRASHED_SERVING: &str =
    "The model stopped unexpectedly. Try again, or pick a different model.";
pub const UNRESPONSIVE: &str =
    "The model stopped responding. Try again, or pick a different model.";

/// What this machine and this process can do about Metal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Silicon {
    /// Apple Silicon, running natively. Metal inference can work here.
    Native,
    /// Apple Silicon hardware, but this process is x86 under Rosetta.
    ///
    /// Kept distinct from `Native` because vllm-metal needs a native arm64
    /// Python and its installer refuses outright when `uname -m` reports
    /// x86_64. The hardware is capable; this build of the launcher is not.
    Translated,
    /// Not Apple Silicon.
    None,
}

/// Reads a sysctl value, or `None` when the key does not exist.
fn sysctl(key: &str) -> Option<String> {
    let output = Command::new("/usr/sbin/sysctl")
        .args(["-n", key])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Works out whether Metal inference is possible on this machine.
///
/// Three signals rather than one, because the obvious check is wrong in the
/// case that matters most. Rosetta presents a translated process with an x86
/// machine, masking `hw.optional.arm64`, so the Intel build of this launcher
/// running on an Apple Silicon Mac reported "not Apple Silicon" and hid GPU
/// support from exactly the users who had the hardware for it.
///
/// `sysctl.proc_translated` exists only inside a translated process, so its
/// presence is itself proof the host is Apple Silicon, and the CPU brand string
/// is a further fallback for either case.
fn detect_silicon() -> Silicon {
    let translated = sysctl("sysctl.proc_translated").as_deref() == Some("1");
    let native_arm = sysctl("hw.optional.arm64").as_deref() == Some("1");
    let apple_cpu = sysctl("machdep.cpu.brand_string")
        .is_some_and(|brand| brand.starts_with("Apple"));

    match (translated, native_arm || apple_cpu) {
        (true, _) => Silicon::Translated,
        (false, true) => Silicon::Native,
        (false, false) => Silicon::None,
    }
}

/// [`detect_silicon`], worked out once. Neither the hardware nor the build can
/// change while the app runs, and the reconcile loop asks every two seconds.
pub fn silicon() -> Silicon {
    static SILICON: OnceLock<Silicon> = OnceLock::new();
    *SILICON.get_or_init(detect_silicon)
}

/// Virtualenv the Metal server runs from.
pub fn venv_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(VENV_DIR))
}

/// The `vllm` CLI inside that virtualenv.
pub fn cli_path() -> Option<PathBuf> {
    venv_dir().map(|venv| venv.join("bin").join("vllm"))
}

fn marker_path() -> Option<PathBuf> {
    venv_dir().map(|venv| venv.join(COMPLETE_MARKER))
}

/// Reports whether the Metal environment is installed, complete and checked.
pub fn is_installed() -> bool {
    cli_path().is_some_and(|cli| cli.is_file()) && marker_path().is_some_and(|marker| marker.is_file())
}

/// Marks the environment as needing repair before its next use.
///
/// Called when the server proves the environment broken -- a module it needs
/// cannot be imported -- so the next attempt repairs it instead of failing the
/// same way again.
fn forget_install() {
    let Some(venv) = venv_dir() else {
        return;
    };
    let _ = std::fs::write(venv.join(REPAIR_MARKER), "");
    if std::fs::remove_file(venv.join(COMPLETE_MARKER)).is_ok() {
        crate::logs::record("metal", "the GPU environment is broken; it will be repaired");
    }
}

fn ownership_file() -> PathBuf {
    crate::paths::data_dir().with_file_name(OWNERSHIP_FILE)
}

/// Reports whether the virtualenv is an install Kayak started and never
/// finished, and so may delete.
///
/// `~/.venv-vllm-metal` is also where upstream's installer puts an environment
/// when someone runs it by hand, and an environment Kayak did not create is
/// never deleted, however broken it looks: it is repaired in place instead.
/// The record is written before the installer runs, because the installer is
/// what creates the folder and an install can be stopped part-way; it is
/// cleared once the install is complete, after which a broken environment is
/// repaired in place like any other.
fn created_by_kayak() -> bool {
    ownership_file().is_file()
}

/// Records that Kayak is about to create the virtualenv.
fn claim_install() {
    let file = ownership_file();
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let created = venv_dir().map(|venv| venv.display().to_string()).unwrap_or_default();
    let _ = std::fs::write(file, created);
}

/// Deletes the virtualenv, so the next install starts from nothing -- but only
/// one Kayak created.
fn remove_install() {
    let Some(venv) = venv_dir() else {
        return;
    };
    if !created_by_kayak() || !venv.exists() {
        return;
    }
    crate::logs::record("metal", "removing a GPU environment Kayak could not finish");
    let _ = std::fs::remove_dir_all(venv);
    let _ = std::fs::remove_file(ownership_file());
}

/// Directories the installer and the server need on their PATH.
///
/// The upstream installer provisions `uv` into the user's home and then invokes
/// it by name, and the virtualenv's `vllm` shells out to tools of its own. An
/// app opened from Finder inherits `/usr/bin:/bin:/usr/sbin:/sbin` and none of
/// those locations, so the install fails the moment it tries to run what it
/// just installed. This is the same environment gap that made the Docker CLI
/// and its credential helper unreachable.
fn tool_search_path() -> std::ffi::OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(home) = dirs::home_dir() {
        // Where uv installs itself, in preference order.
        dirs.push(home.join(".local").join("bin"));
        dirs.push(home.join(".cargo").join("bin"));
        dirs.push(home.join(VENV_DIR).join("bin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin"));
    dirs.push(PathBuf::from("/usr/local/bin"));

    if let Some(inherited) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&inherited));
    }

    let mut seen = std::collections::HashSet::new();
    dirs.retain(|dir| !dir.as_os_str().is_empty() && seen.insert(dir.clone()));
    std::env::join_paths(dirs).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// The shell script that installs the Metal environment.
///
/// The upstream script is downloaded to a file and then run, rather than
/// piped from `curl` into `bash`. Piped, a failed download fed bash an empty
/// script, which bash ran successfully -- so a Mac with no internet reported
/// the install as done. `pipefail` and `-e` make any failing step fail the
/// whole install, and the TERM trap lets the temporary folder be cleaned up
/// when quitting stops an install part-way.
fn install_script() -> String {
    format!(
        "set -euo pipefail\n\
         workdir=\"$(mktemp -d)\"\n\
         trap 'rm -rf \"$workdir\"' EXIT\n\
         trap 'exit 143' TERM\n\
         curl -fsSL --retry 3 --connect-timeout 30 -o \"$workdir/install.sh\" {INSTALL_SCRIPT_URL}\n\
         bash \"$workdir/install.sh\" {INSTALL_CHANNEL}\n"
    )
}

/// Builds the command that installs the Metal environment.
///
/// Started as the leader of its own process group, so stopping it reaches the
/// `curl`, `uv` and Python processes it starts, not just the shell.
fn install_command() -> Command {
    let mut command = Command::new("/bin/bash");
    command
        .env("PATH", tool_search_path())
        .arg("-c")
        .arg(install_script())
        .stdin(Stdio::null())
        .process_group(0);
    command
}

/// Which vLLM tool-call parser understands each model family's output format.
///
/// Ordered, first match wins, so more specific names must precede the family
/// catch-all ("deepseek-v3.1" before "deepseek"). Matched against the
/// lowercased model id. Parser names must exist in vLLM's registry
/// (vllm/tool_parsers) — an unknown name stops the server from starting.
///
/// Kayak's backend carries the same table for the containerised path
/// (`backend/app/vllm/manager.py`); the two must agree so a model behaves the
/// same whichever backend serves it.
const TOOL_PARSER_RULES: &[(&str, &str)] = &[
    ("qwen3-coder", "qwen3_coder"),
    ("gpt-oss", "openai"),
    // R1 distills keep their base model's chat template, not DeepSeek's.
    ("deepseek-r1-distill-llama", "llama3_json"),
    ("deepseek-r1-distill", "hermes"),
    ("deepseek-v3.2", "deepseek_v32"),
    ("deepseek-v3.1", "deepseek_v31"),
    ("deepseek-v4", "deepseek_v4"),
    ("deepseek", "deepseek_v3"),
    ("glm-4.7", "glm47"),
    ("glm", "glm45"),
    ("granite-20b-fc", "granite-20b-fc"),
    ("granite-4", "granite4"),
    ("granite", "granite"),
    ("phi-4-mini", "phi4_mini_json"),
    ("phi4-mini", "phi4_mini_json"),
    ("internlm", "internlm"),
    ("kimi-k3", "kimi_k3"),
    ("kimi", "kimi_k2"),
    ("llama-4", "llama4_pythonic"),
    ("llama4", "llama4_pythonic"),
    ("llama", "llama3_json"),
    ("mistral", "mistral"),
    ("jamba", "jamba"),
    ("gemma-4", "gemma4"),
    ("seed-oss", "seed_oss"),
    ("hunyuan", "hunyuan_a13b"),
    ("minimax-m3", "minimax_m3"),
    ("minimax", "minimax_m2"),
    ("ernie", "ernie45"),
    ("olmo-3", "olmo3"),
    ("olmo3", "olmo3"),
];

/// The vLLM tool-call parser for a model, by family.
///
/// Falls back to "hermes", which matches the format Qwen and most other open
/// models emit. A mismatched parser does not crash the server; it just leaves
/// tool calls unrecognised in the plain text of the reply.
fn tool_call_parser(model: &str) -> &'static str {
    let model = model.to_ascii_lowercase();
    TOOL_PARSER_RULES
        .iter()
        .find(|(needle, _)| model.contains(needle))
        .map(|(_, parser)| *parser)
        .unwrap_or("hermes")
}

/// Arguments that start the OpenAI-compatible server for a model.
///
/// Bound to loopback: this endpoint runs arbitrary generation on the user's
/// machine and, unlike the containerised path, has no VM boundary around it.
///
/// Tool calling must be enabled explicitly: Kayak's agents attach tools to
/// every chat request, and without these flags vLLM rejects such requests with
/// a 400 ("auto" tool choice requires --enable-auto-tool-choice) the moment
/// the user sends a message.
pub fn serve_args(model: &str, port: u16) -> Vec<String> {
    vec![
        "serve".to_string(),
        model.to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "--enable-auto-tool-choice".to_string(),
        "--tool-call-parser".to_string(),
        tool_call_parser(model).to_string(),
    ]
}

/// Reports whether a model identifier is one the Metal backend can serve.
///
/// vllm-metal runs MLX-format weights, which on Hugging Face are published
/// under the `mlx-community` organisation. Pointing it at an ordinary
/// repository fails several minutes into a download, so it is worth refusing
/// up front.
pub fn is_mlx_model(model: &str) -> bool {
    model
        .split_once('/')
        .is_some_and(|(org, name)| org.eq_ignore_ascii_case("mlx-community") && !name.is_empty())
}


/// Why an install or a server failed, in the two forms Kayak needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// One sentence for the person using Kayak: what happened, and what to do.
    pub reason: &'static str,
    /// What the process printed before it failed, for Kayak's logs panel.
    pub detail: Option<String>,
}

impl Failure {
    /// The environment could not be set up, for a reason the installer's own
    /// output does not cover.
    pub fn setup(detail: impl Into<String>) -> Self {
        Self::new(SETUP_FAILED, detail)
    }

    fn new(reason: &'static str, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        Self {
            reason,
            detail: (!detail.trim().is_empty()).then_some(detail),
        }
    }
}

/// The most recent lines a process printed.
///
/// Kept per process so a failure is explained from what that process said,
/// rather than from the last lines of the whole launcher log, which by the
/// time anyone looked could be about something else entirely.
#[derive(Clone, Default)]
struct Output(Arc<Mutex<VecDeque<String>>>);

impl Output {
    fn push(&self, line: &str) {
        let mut lines = lock(&self.0);
        if lines.len() == OUTPUT_LINES {
            lines.pop_front();
        }
        lines.push_back(line.trim_end().to_string());
    }

    /// The last `count` lines, oldest first, as one block of text.
    fn last(&self, count: usize) -> String {
        let lines = lock(&self.0);
        let skip = lines.len().saturating_sub(count);
        lines.iter().skip(skip).cloned().collect::<Vec<_>>().join("\n")
    }
}

/// Reads a child stream to exhaustion on its own thread, recording each line in
/// the launcher log and in the process's own output.
///
/// The reader outlives this call and keeps the pipe drained until the process
/// closes it. The handle is for callers that need every line before they look
/// at the output; the server's readers are left to run.
fn drain<R: std::io::Read + Send + 'static>(
    stream: Option<R>,
    source: &'static str,
    output: Output,
) -> Option<std::thread::JoinHandle<()>> {
    let stream = stream?;
    Some(std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            crate::logs::record(source, &line);
            output.push(&line);
        }
    }))
}

/// Lowercased text, searched for any of `needles`.
fn mentions(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| text.contains(needle))
}

const NO_SPACE: &[&str] = &["no space left", "errno 28", "disk quota exceeded"];

/// Explains why the installer failed, from what it printed.
fn explain_install(output: &str) -> &'static str {
    let text = output.to_ascii_lowercase();
    if mentions(&text, NO_SPACE) {
        OUT_OF_STORAGE
    } else if mentions(
        &text,
        &[
            "could not resolve host",
            "failed to connect",
            "connection timed out",
            "operation timed out",
            "network is unreachable",
            "temporary failure in name resolution",
            "check your internet connection",
            "failed to fetch",
            "error sending request",
            "dns error",
            "connection reset",
            "curl: (6)",
            "curl: (7)",
            "curl: (28)",
            "curl: (35)",
            "curl: (56)",
        ],
    ) {
        INSTALL_OFFLINE
    } else {
        SETUP_FAILED
    }
}

/// Explains why the server exited, and whether its environment needs
/// reinstalling before the next attempt.
///
/// The output is checked in order of what the user can act on. Errors that
/// name a cause -- a gated repository, an unsupported architecture -- are only
/// believed on lines that read as errors, because vLLM prints harmless
/// warnings containing the same words ("... is not supported on this
/// platform") while loading perfectly good models.
fn explain_exit(output: &str, signal: Option<i32>, served: bool) -> (&'static str, bool) {
    let text = output.to_ascii_lowercase();
    let error_lines: String = text
        .lines()
        .filter(|line| line.contains("error") || line.contains("exception"))
        .collect::<Vec<_>>()
        .join("\n");

    if mentions(&text, NO_SPACE) {
        (OUT_OF_STORAGE, false)
    } else if signal == Some(libc::SIGKILL)
        || mentions(
            &text,
            &[
                "insufficient memory",
                "out of memory",
                "outofmemory",
                "memoryerror",
                "exitcode=-9",
                "sigkill",
            ],
        )
    {
        // macOS stops a process that exhausts memory with SIGKILL, and nothing
        // else in this setup sends one to a running server.
        (OUT_OF_MEMORY, false)
    } else if mentions(&error_lines, &["modulenotfounderror", "importerror"]) {
        (SETUP_FAILED, true)
    } else if mentions(
        &error_lines,
        &[
            "repositorynotfound",
            "gatedrepo",
            "repository not found",
            "401 client error",
            "404 client error",
            "unauthorized for url",
        ],
    ) {
        (MODEL_UNAVAILABLE, false)
    } else if mentions(&error_lines, &["not supported", "model type", "notimplementederror"]) {
        (UNSUPPORTED_MODEL, false)
    } else if served {
        (CRASHED_SERVING, false)
    } else {
        (CRASHED_LOADING, false)
    }
}

/// Why an environment did not pass its checks.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Unverified {
    /// A check failed: the environment is incomplete or broken.
    Broken(String),
    /// A check did not finish in time. Proves nothing: the first start of a
    /// fresh environment compiles a great deal of Python, and a busy Mac can
    /// take minutes over it.
    TimedOut(String),
}

impl Unverified {
    fn into_detail(self) -> String {
        match self {
            Self::Broken(detail) | Self::TimedOut(detail) => detail,
        }
    }
}

/// Checks that an install finished and works.
///
/// `find_spec` asks whether the Metal plugin is installed without importing
/// it, which is the half-finished case the completion marker exists for;
/// `vllm --version` then proves the core can actually start. Each check runs
/// in a process group reported to `on_group`, so quitting can stop it.
fn verify_install(on_group: &dyn Fn(Option<Group>)) -> Result<(), Unverified> {
    let venv = venv_dir().ok_or_else(|| Unverified::Broken("no home folder".to_string()))?;
    let python = venv.join("bin").join("python");
    let cli = venv.join("bin").join("vllm");

    let checks: [(&Path, &[&str], u64); 2] = [
        (
            &python,
            &[
                "-c",
                "import importlib.util, sys; \
                 sys.exit(0 if importlib.util.find_spec('vllm_metal') else 1)",
            ],
            60,
        ),
        (&cli, &["--version"], 180),
    ];
    for (program, args, seconds) in checks {
        let output = process::output_within_group(
            Command::new(program).args(args).env("PATH", tool_search_path()),
            Duration::from_secs(seconds),
            on_group,
        );
        let command = format!("{} {}", program.display(), args.join(" "));
        match output {
            Ok(output) if output.status.success() => {}
            Ok(output) => {
                return Err(Unverified::Broken(format!(
                    "{command} failed: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )))
            }
            Err(RunError::TimedOut) => {
                return Err(Unverified::TimedOut(format!(
                    "{command} did not finish within {seconds} seconds"
                )))
            }
            Err(RunError::Spawn(error)) => {
                return Err(Unverified::Broken(format!("could not run {command}: {error}")))
            }
        }
    }
    Ok(())
}

/// Records that the environment is complete.
fn mark_installed() -> Result<(), String> {
    let venv = venv_dir().ok_or("no home folder")?;
    let _ = std::fs::remove_file(venv.join(REPAIR_MARKER));
    std::fs::write(venv.join(COMPLETE_MARKER), env!("CARGO_PKG_VERSION"))
        .map_err(|error| format!("could not write the completion marker: {error}"))?;
    // Complete, so nothing about it needs undoing any more.
    let _ = std::fs::remove_file(ownership_file());
    Ok(())
}

/// What to do with a virtualenv that is there but not known to be complete.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Existing {
    /// It works: an install from before the completion marker existed, or one
    /// made by hand. Kept rather than downloaded again.
    Keep,
    /// Kayak made it and it is broken: start again from nothing.
    Replace,
    /// Someone else made it and it is broken: run the installer over it,
    /// which installs what is missing without deleting anything.
    Repair,
    /// Its checks did not finish, which proves nothing; try again later.
    Undecided(String),
}

fn decide(checked: Result<(), Unverified>, ours: bool) -> Existing {
    match checked {
        Ok(()) => Existing::Keep,
        Err(Unverified::TimedOut(detail)) => Existing::Undecided(detail),
        Err(Unverified::Broken(_)) if ours => Existing::Replace,
        Err(Unverified::Broken(_)) => Existing::Repair,
    }
}

/// Installs the Metal environment.
///
/// This takes minutes and downloads gigabytes, so the caller is expected to be
/// on a thread that can block. Every process it starts -- the installer and
/// the checks around it -- leads a process group reported to `on_group` while
/// it runs, so quitting can stop it, and is reported gone once it has ended.
///
/// A virtualenv left without the completion marker is dealt with first. One
/// that passes the checks is kept. One that fails them is replaced only if
/// Kayak created it; anything else is repaired in place, because it may be
/// someone's own install.
pub fn run_install(on_group: &dyn Fn(Option<Group>)) -> Result<(), Failure> {
    let venv = venv_dir().ok_or_else(|| Failure::setup("no home folder"))?;

    if venv.exists() {
        // Marked by a server that could not import what it needed: broken in
        // a way the checks below would not notice.
        let checked = if venv.join(REPAIR_MARKER).exists() {
            Err(Unverified::Broken("the GPU server found it broken".to_string()))
        } else {
            verify_install(on_group)
        };
        match decide(checked, created_by_kayak()) {
            Existing::Keep => {
                crate::logs::record("metal", "found a complete GPU environment; keeping it");
                return mark_installed().map_err(Failure::setup);
            }
            Existing::Undecided(detail) => {
                crate::logs::record("metal", &format!("could not check the GPU environment: {detail}"));
                return Err(Failure::setup(detail));
            }
            Existing::Replace => remove_install(),
            Existing::Repair => crate::logs::record(
                "metal",
                "the GPU environment is incomplete and Kayak did not create it; repairing it in place",
            ),
        }
    }
    if !venv.exists() {
        claim_install();
    }

    let mut child = install_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| Failure::setup(format!("Could not start the installer: {error}")))?;
    on_group(Some(Group::of(&child)));

    let output = Output::default();
    let readers = [
        drain(child.stdout.take(), "metal-install", output.clone()),
        drain(child.stderr.take(), "metal-install", output.clone()),
    ];
    let status = child.wait();
    // Reaped, so its ID may be handed to another process from here on.
    on_group(None);
    // Every line is in before the output is read for an explanation.
    for reader in readers.into_iter().flatten() {
        let _ = reader.join();
    }
    let printed = output.last(DETAIL_LINES);

    match status {
        Ok(status) if status.success() => {}
        Ok(status) => {
            crate::logs::record("metal", &format!("the installer failed: {status}"));
            return Err(Failure::new(explain_install(&printed), printed));
        }
        Err(error) => return Err(Failure::setup(format!("The installer did not finish: {error}"))),
    }

    match verify_install(on_group) {
        Ok(()) => mark_installed().map_err(Failure::setup)?,
        Err(problem) => {
            let broken = matches!(problem, Unverified::Broken(_));
            let detail = problem.into_detail();
            crate::logs::record("metal", &format!("the installed environment failed its checks: {detail}"));
            if broken {
                remove_install();
            }
            return Err(Failure::setup(format!("{printed}\n{detail}")));
        }
    }
    crate::logs::record("metal", "the GPU environment is installed");
    Ok(())
}

/// How a server's health checks add up over time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Health {
    /// Loading: it has never answered with the model listed.
    Starting,
    Ready,
    /// It has served the model, but has stopped answering.
    Unresponsive,
}

/// The memory behind [`Health`]: whether a server has ever been ready, and
/// since when it has been failing its checks.
#[derive(Debug, Default)]
struct HealthWatch {
    served: bool,
    failing_since: Option<Instant>,
}

impl HealthWatch {
    /// Folds in one health check, made at `now`.
    ///
    /// A server that has been ready never goes back to "starting": one slow
    /// answer used to do exactly that, and Kayak read it as the model loading
    /// all over again. It stays ready until it has failed every check for
    /// [`UNRESPONSIVE_AFTER`], and is ready again the moment it answers.
    fn observe(&mut self, answered: bool, now: Instant) -> Health {
        if answered {
            self.served = true;
            self.failing_since = None;
            return Health::Ready;
        }
        if !self.served {
            return Health::Starting;
        }
        let since = *self.failing_since.get_or_insert(now);
        if now.duration_since(since) >= UNRESPONSIVE_AFTER {
            Health::Unresponsive
        } else {
            Health::Ready
        }
    }
}

/// A running Metal inference server.
pub struct Server {
    child: Child,
    group: Group,
    pub model: String,
    /// Chosen when the server starts, so it can never collide with Kayak's own
    /// port or anything else already listening.
    pub port: u16,
    output: Output,
    exit: Option<ExitStatus>,
    health: HealthWatch,
    /// Where the process group is recorded while the server runs.
    pid_file: Option<PathBuf>,
    stopped: bool,
}

impl Server {
    /// Reports whether the process is still alive, reaping it if it has exited.
    pub fn is_alive(&mut self) -> bool {
        if self.exit.is_some() {
            return false;
        }
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                self.exit = Some(status);
                false
            }
            Err(_) => false,
        }
    }

    /// Folds in one health check of this server.
    pub fn observe(&mut self, answered: bool) -> Health {
        self.health.observe(answered, Instant::now())
    }

    /// Explains why a server that is no longer alive stopped.
    ///
    /// A server that died because its environment is broken has the
    /// environment marked for reinstalling as a side effect, so the next
    /// attempt repairs it.
    pub fn failure(&self) -> Failure {
        let printed = self.output.last(DETAIL_LINES);
        let signal = self.exit.and_then(|status| status.signal());
        let (reason, reinstall) = explain_exit(&printed, signal, self.health.served);
        if reinstall {
            forget_install();
        }
        let status = self
            .exit
            .map(|status| status.to_string())
            .unwrap_or_else(|| "an unknown status".to_string());
        crate::logs::record("metal", &format!("the GPU server exited with {status}"));
        Failure::new(reason, printed)
    }

    /// Stops the server and everything it started. Takes up to the grace
    /// period; calling it again afterwards does nothing.
    pub fn stop(&mut self) {
        if self.stopped {
            return;
        }
        self.stopped = true;
        // A server that already exited may still have left its engine process
        // behind, so its group is stopped either way; only a live one is news.
        if self.exit.is_none() {
            crate::logs::record("metal", &format!("stopping the GPU server for {}", self.model));
        }
        let child = &mut self.child;
        self.group.terminate(STOP_GRACE, || {
            let _ = child.try_wait();
        });
        if let Some(pid_file) = &self.pid_file {
            let _ = std::fs::remove_file(pid_file);
        }
    }
}

/// A server is never let go of while still running: replacing or dropping one
/// used to leave its process, and the model in memory, behind.
impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Starts the Metal inference server for a model.
///
/// The process group is written to `pid_file` so that a launcher that dies
/// without stopping it can be followed by one that does.
pub fn spawn_server(
    model: &str,
    port: u16,
    hf_token: Option<&str>,
    pid_file: Option<&Path>,
) -> Result<Server, Failure> {
    let Some(cli) = cli_path().filter(|cli| cli.is_file()) else {
        forget_install();
        return Err(Failure::new(SETUP_FAILED, "The vllm CLI is missing from the virtualenv"));
    };
    let token = hf_token.map(str::trim).filter(|token| !token.is_empty());

    // The token is a secret: the log says only whether one was given.
    crate::logs::record(
        "metal",
        &format!(
            "starting {model} on port {port} ({})",
            if token.is_some() { "with a Hugging Face token" } else { "without a Hugging Face token" }
        ),
    );

    let mut command = Command::new(&cli);
    command
        .args(serve_args(model, port))
        .env("PATH", tool_search_path())
        // Weights land in the Hugging Face cache, which lives in the user's
        // home directory rather than Kayak's data directory, because the Metal
        // server runs on the host and shares nothing with the container.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own group, so stopping it reaches the engine process vLLM starts
        // to run the model, which otherwise outlived both server and launcher.
        .process_group(0);
    if let Some(token) = token {
        command.env("HF_TOKEN", token);
    }
    let mut child = command.spawn().map_err(|error| {
        Failure::new(CRASHED_LOADING, format!("Could not start the GPU server: {error}"))
    })?;

    let group = Group::of(&child);
    if let Some(pid_file) = pid_file {
        group.record(pid_file);
    }

    // Both streams must be read for the lifetime of the process, not merely
    // captured. vLLM writes steadily while it loads a model, and a pipe nothing
    // drains fills after about 64KB, at which point the server blocks on its own
    // logging and never finishes starting -- with no output to explain why.
    let output = Output::default();
    drain(child.stdout.take(), "metal", output.clone());
    drain(child.stderr.take(), "metal", output.clone());

    Ok(Server {
        child,
        group,
        model: model.to_string(),
        port,
        output,
        exit: None,
        health: HealthWatch::default(),
        pid_file: pid_file.map(Path::to_path_buf),
        stopped: false,
    })
}

/// Reports whether a server is answering on `port` *and* serving `model`.
///
/// Something answering is not enough: the port is chosen by probing, and
/// another program -- or a previous server on its way out -- can be the one
/// that replies.
pub fn serves(port: u16, model: &str) -> bool {
    ureq::get(&format!("http://127.0.0.1:{port}/v1/models"))
        .timeout(Duration::from_secs(3))
        .call()
        .ok()
        .and_then(|response| response.into_json::<serde_json::Value>().ok())
        .is_some_and(|body| lists_model(&body, model))
}

/// Reports whether an OpenAI-style model list includes `model`.
fn lists_model(body: &serde_json::Value, model: &str) -> bool {
    body.get("data")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|models| {
            models
                .iter()
                .any(|entry| entry.get("id").and_then(serde_json::Value::as_str) == Some(model))
        })
}

/// Reports whether a command line belongs to the GPU server or its installer.
///
/// Both run from, or install into, the dedicated virtualenv, and the installer
/// is fetched from the vllm-metal repository; nothing else on a Mac mentions
/// either.
fn is_ours(command: &str) -> bool {
    command.contains(VENV_DIR) || command.contains("vllm-project/vllm-metal")
}

/// Stops a GPU server or installer that a previous launch left running.
///
/// A launcher that crashed or was force-quit never stopped what it started,
/// and the model it was serving stayed in memory with nothing left to use or
/// stop it. The recorded group is only signalled when a process in it is
/// recognisably ours, because the ID may since have been reused.
pub fn stop_left_behind(pid_file: &Path) {
    let Some(group) = Group::recorded(pid_file) else {
        let _ = std::fs::remove_file(pid_file);
        return;
    };
    let commands = group.commands();
    if commands.iter().any(|command| is_ours(command)) {
        crate::logs::record(
            "metal",
            &format!("stopping GPU processes left running by a previous launch (group {})", group.id()),
        );
        group.terminate(STOP_GRACE, || {});
    }
    let _ = std::fs::remove_file(pid_file);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_mlx_community_models() {
        assert!(is_mlx_model("mlx-community/Qwen3.8-27B-8bit"));
        // The organisation is matched case-insensitively because Hugging Face
        // resolves repository owners that way.
        assert!(is_mlx_model("MLX-Community/Llama-3.2-3B-Instruct-4bit"));
    }

    #[test]
    fn rejects_ordinary_repositories() {
        assert!(!is_mlx_model("Qwen/Qwen2.5-Coder-7B-Instruct"));
        assert!(!is_mlx_model("meta-llama/Llama-3.1-8B"));
    }

    #[test]
    fn rejects_malformed_identifiers() {
        assert!(!is_mlx_model("mlx-community"));
        assert!(!is_mlx_model("mlx-community/"));
        assert!(!is_mlx_model(""));
    }

    #[test]
    fn serve_args_bind_to_loopback() {
        let args = serve_args("mlx-community/Qwen3.8-27B-8bit", 8001);

        assert_eq!(args[0], "serve");
        assert_eq!(args[1], "mlx-community/Qwen3.8-27B-8bit");
        // A generation endpoint reachable from the network would be a remote
        // code-execution surface with no container around it.
        assert!(args.contains(&"127.0.0.1".to_string()));
        assert!(args.contains(&"8001".to_string()));
    }

    #[test]
    fn serve_args_enable_tool_calling() {
        // Kayak's agents send tools with every request; a server started
        // without these flags 400s on the first message.
        let args = serve_args("mlx-community/Qwen3-0.6B-4bit", 8001);

        assert!(args.contains(&"--enable-auto-tool-choice".to_string()));
        assert!(args.contains(&"--tool-call-parser".to_string()));
        assert!(args.contains(&"hermes".to_string()));
    }

    #[test]
    fn tool_call_parser_matches_model_family() {
        assert_eq!(tool_call_parser("mlx-community/Qwen3-0.6B-4bit"), "hermes");
        assert_eq!(
            tool_call_parser("mlx-community/Llama-3.2-3B-Instruct-4bit"),
            "llama3_json"
        );
        assert_eq!(
            tool_call_parser("mlx-community/Mistral-7B-Instruct-v0.3-4bit"),
            "mistral"
        );
        assert_eq!(
            tool_call_parser("mlx-community/Qwen3-Coder-30B-A3B-Instruct-4bit"),
            "qwen3_coder"
        );
        assert_eq!(tool_call_parser("mlx-community/gpt-oss-20b-MXFP4-Q8"), "openai");
        assert_eq!(tool_call_parser("mlx-community/GLM-4.5-Air-4bit"), "glm45");
        assert_eq!(
            tool_call_parser("mlx-community/Phi-4-mini-instruct-4bit"),
            "phi4_mini_json"
        );
        // Unknown families fall back to hermes rather than erroring.
        assert_eq!(tool_call_parser("some-org/some-model"), "hermes");
    }

    #[test]
    fn tool_call_parser_precedence_favours_specific_rules() {
        // Llama 4 emits pythonic tool calls, not the Llama 3 JSON format.
        assert_eq!(
            tool_call_parser("mlx-community/Llama-4-Scout-17B-16E-Instruct-4bit"),
            "llama4_pythonic"
        );
        assert_eq!(tool_call_parser("mlx-community/DeepSeek-V3-0324-4bit"), "deepseek_v3");
        assert_eq!(tool_call_parser("deepseek-ai/DeepSeek-V3.1-Base"), "deepseek_v31");
        // R1 distills keep the template of the model they were distilled into.
        assert_eq!(
            tool_call_parser("mlx-community/DeepSeek-R1-Distill-Qwen-7B-4bit"),
            "hermes"
        );
        assert_eq!(
            tool_call_parser("mlx-community/DeepSeek-R1-Distill-Llama-8B-4bit"),
            "llama3_json"
        );
        assert_eq!(tool_call_parser("ibm-granite/granite-4.0-h-small"), "granite4");
        assert_eq!(tool_call_parser("ibm-granite/granite-3.3-8b-instruct"), "granite");
    }

    #[test]
    fn intel_hardware_reports_no_silicon() {
        // Guards against the probe reporting Native everywhere, which would
        // offer GPU inference on machines that cannot run it.
        if sysctl("machdep.cpu.brand_string").is_some_and(|b| b.starts_with("Intel"))
            && sysctl("sysctl.proc_translated").is_none()
        {
            assert_eq!(detect_silicon(), Silicon::None);
        }
    }

    #[test]
    fn translated_is_not_treated_as_native() {
        // The distinction is the whole point: the hardware can serve Metal but
        // this process cannot, and the two need different messages.
        assert_ne!(Silicon::Translated, Silicon::Native);
        assert!(matches!(detect_silicon(), Silicon::Native | Silicon::Translated | Silicon::None));
    }

    #[test]
    fn the_installer_is_pinned_and_asks_for_a_stable_release() {
        let script = install_script();

        assert!(script.contains("/vllm-metal/v0.32.0/install.sh"), "{script}");
        assert!(!script.contains("/main/install.sh"));
        assert!(script.contains("--stable"));
        // Piping curl into bash is what let an offline install "succeed".
        assert!(!script.contains("| bash"));
        assert!(script.contains("set -euo pipefail"));
    }

    #[test]
    fn an_install_that_cannot_download_fails() {
        // Port 9 refuses connections, standing in for a Mac with no internet.
        let script = install_script()
            .replace(INSTALL_SCRIPT_URL, "http://127.0.0.1:9/install.sh");

        let status = Command::new("/bin/bash")
            .arg("-c")
            .arg(script)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();

        assert!(!status.success());
    }

    #[test]
    fn install_failures_are_told_apart() {
        assert_eq!(
            explain_install("curl: (6) Could not resolve host: raw.githubusercontent.com"),
            INSTALL_OFFLINE
        );
        assert_eq!(
            explain_install("error: Failed to fetch: `https://files.pythonhosted.org/...`"),
            INSTALL_OFFLINE
        );
        assert_eq!(explain_install("OSError: [Errno 28] No space left on device"), OUT_OF_STORAGE);
        assert_eq!(
            explain_install("error: vllm-metal requires Apple Silicon arm64."),
            SETUP_FAILED
        );
    }

    #[test]
    fn a_server_killed_by_the_system_ran_out_of_memory() {
        let (reason, reinstall) = explain_exit("INFO loading weights", Some(libc::SIGKILL), false);
        assert_eq!(reason, OUT_OF_MEMORY);
        assert!(!reinstall);
        assert_eq!(
            explain_exit("RuntimeError: [metal::malloc] Insufficient Memory", None, false).0,
            OUT_OF_MEMORY
        );
    }

    #[test]
    fn a_broken_environment_is_marked_for_reinstalling() {
        let (reason, reinstall) = explain_exit(
            "Traceback (most recent call last):\nModuleNotFoundError: No module named 'vllm_metal'",
            None,
            false,
        );
        assert_eq!(reason, SETUP_FAILED);
        assert!(reinstall);
    }

    #[test]
    fn a_model_that_cannot_be_downloaded_says_so() {
        for output in [
            "huggingface_hub.errors.GatedRepoError: 401 Client Error.",
            "huggingface_hub.errors.RepositoryNotFoundError: 404 Client Error. Repository Not Found",
        ] {
            assert_eq!(explain_exit(output, None, false).0, MODEL_UNAVAILABLE, "{output}");
        }
    }

    #[test]
    fn an_unsupported_model_is_recognised_only_from_an_error() {
        assert_eq!(
            explain_exit("ValueError: Model type qwen9_next not supported.", None, false).0,
            UNSUPPORTED_MODEL
        );
        // vLLM prints warnings like this while loading models that work fine;
        // a crash for some other reason must not be blamed on them.
        assert_eq!(
            explain_exit("WARNING pin_memory is not supported on this platform\nSegfault", None, false).0,
            CRASHED_LOADING
        );
    }

    #[test]
    fn a_crash_after_serving_is_not_called_a_loading_failure() {
        assert_eq!(explain_exit("Segmentation fault", None, true).0, CRASHED_SERVING);
        assert_eq!(explain_exit("Segmentation fault", None, false).0, CRASHED_LOADING);
    }

    #[test]
    fn no_message_mentions_the_machinery() {
        for message in [
            NOT_FOR_MAC,
            NEEDS_APPLE_BUILD,
            INSTALL_OFFLINE,
            SETUP_FAILED,
            OUT_OF_STORAGE,
            OUT_OF_MEMORY,
            UNSUPPORTED_MODEL,
            MODEL_UNAVAILABLE,
            CRASHED_LOADING,
            CRASHED_SERVING,
            UNRESPONSIVE,
        ] {
            let lower = message.to_lowercase();
            for banned in ["vllm", "metal", "venv", "container", "docker", "port ", "mlx", "gpu"] {
                assert!(!lower.contains(banned), "{message}");
            }
        }
    }

    #[test]
    fn a_ready_server_stays_ready_through_slow_answers() {
        let start = Instant::now();
        let at = |seconds: u64| start + Duration::from_secs(seconds);
        let mut watch = HealthWatch::default();

        assert_eq!(watch.observe(false, at(0)), Health::Starting);
        assert_eq!(watch.observe(true, at(2)), Health::Ready);
        // A minute and more of missed checks on a swapping Mac is not a dead
        // server, and reporting it as one made Kayak release a healthy model.
        for second in (5..120).step_by(5) {
            assert_eq!(watch.observe(false, at(second)), Health::Ready, "at {second}s");
        }
        assert_eq!(watch.observe(false, at(125)), Health::Unresponsive);
        // Answering again puts it straight back, and starts the clock afresh.
        assert_eq!(watch.observe(true, at(130)), Health::Ready);
        assert_eq!(watch.observe(false, at(135)), Health::Ready);
        assert_eq!(watch.observe(false, at(250)), Health::Ready);
    }

    #[test]
    fn a_server_still_loading_is_starting_however_long_it_takes() {
        let start = Instant::now();
        let mut watch = HealthWatch::default();

        assert_eq!(
            watch.observe(false, start + Duration::from_secs(600)),
            Health::Starting
        );
    }

    #[test]
    fn an_environment_is_only_replaced_if_kayak_made_it() {
        let broken = || Err(Unverified::Broken("no vllm_metal".to_string()));

        assert_eq!(decide(Ok(()), false), Existing::Keep);
        assert_eq!(decide(Ok(()), true), Existing::Keep);
        assert_eq!(decide(broken(), true), Existing::Replace);
        // Someone's own install at upstream's default location is repaired in
        // place, never deleted.
        assert_eq!(decide(broken(), false), Existing::Repair);
    }

    #[test]
    fn a_check_that_ran_out_of_time_condemns_nothing() {
        // A cold first `vllm --version` can take minutes; that is not broken.
        for ours in [true, false] {
            assert!(matches!(
                decide(Err(Unverified::TimedOut("slow".to_string())), ours),
                Existing::Undecided(_)
            ));
        }
    }

    #[test]
    fn health_needs_the_requested_model_listed() {
        let body = serde_json::json!({
            "object": "list",
            "data": [{"id": "mlx-community/Qwen3-0.6B-4bit", "object": "model"}]
        });

        assert!(lists_model(&body, "mlx-community/Qwen3-0.6B-4bit"));
        // Something else answering on the port is not the server we started.
        assert!(!lists_model(&body, "mlx-community/Llama-3.2-3B-Instruct-4bit"));
        assert!(!lists_model(&serde_json::json!({"status": "ok"}), "x"));
    }

    #[test]
    fn recognises_its_own_processes_and_nothing_else() {
        assert!(is_ours(
            "/Users/me/.venv-vllm-metal/bin/python3 /Users/me/.venv-vllm-metal/bin/vllm serve x"
        ));
        assert!(is_ours(
            "/bin/bash -c curl -o /tmp/x https://raw.githubusercontent.com/vllm-project/vllm-metal/v1/install.sh"
        ));
        assert!(!is_ours("/usr/bin/python3 -m http.server 8100"));
        assert!(!is_ours("/Applications/Safari.app/Contents/MacOS/Safari"));
    }

    #[test]
    fn output_keeps_only_the_most_recent_lines() {
        let output = Output::default();
        for index in 0..(OUTPUT_LINES + 10) {
            output.push(&format!("line {index}\r"));
        }

        let last = output.last(2);
        assert_eq!(last, format!("line {}\nline {}", OUTPUT_LINES + 8, OUTPUT_LINES + 9));
    }

    #[test]
    fn a_failure_with_nothing_printed_carries_no_detail() {
        assert_eq!(Failure::new(SETUP_FAILED, "  \n").detail, None);
        assert_eq!(Failure::new(SETUP_FAILED, "boom").detail.as_deref(), Some("boom"));
    }
}
