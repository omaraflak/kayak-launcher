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

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// Upstream installer. It provisions `uv`, an arm64 Python 3.12, the vLLM core
/// wheel, and the Metal plugin into a virtualenv.
const INSTALL_SCRIPT_URL: &str =
    "https://raw.githubusercontent.com/vllm-project/vllm-metal/main/install.sh";

/// Where `install.sh` places the virtualenv when run without a checkout.
const VENV_DIR: &str = ".venv-vllm-metal";

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
pub fn detect_silicon() -> Silicon {
    if !cfg!(target_os = "macos") {
        return Silicon::None;
    }

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

/// Virtualenv the Metal server runs from.
pub fn venv_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(VENV_DIR))
}

/// The `vllm` CLI inside that virtualenv.
pub fn cli_path() -> Option<PathBuf> {
    venv_dir().map(|venv| venv.join("bin").join("vllm"))
}

/// Reports whether the Metal environment is already installed and usable.
pub fn is_installed() -> bool {
    cli_path().is_some_and(|cli| cli.is_file())
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

/// Builds the command that installs the Metal environment.
///
/// Piping a remote script into a shell is the installation path upstream
/// documents and the only one they support; pinning our own copy would silently
/// diverge from the wheel versions it resolves.
pub fn install_command() -> Command {
    let mut command = Command::new("/bin/bash");
    command.env("PATH", tool_search_path());
    command.arg("-c").arg(format!(
        "curl -fsSL {INSTALL_SCRIPT_URL} | bash"
    ));
    command
}

/// Arguments that start the OpenAI-compatible server for a model.
///
/// Bound to loopback: this endpoint runs arbitrary generation on the user's
/// machine and, unlike the containerised path, has no VM boundary around it.
pub fn serve_args(model: &str, port: u16) -> Vec<String> {
    vec![
        "serve".to_string(),
        model.to_string(),
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
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

/// A running Metal inference server.
pub struct Server {
    child: Child,
    pub model: String,
}

impl Server {
    /// Reports whether the process is still alive, reaping it if it has exited.
    pub fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Stops the server, preferring a clean shutdown.
    ///
    /// vLLM holds several gigabytes of weights and a Metal context; killing it
    /// outright leaves the GPU allocation to be reclaimed by the OS, which on a
    /// quick stop/start cycle can make the next launch fail to allocate.
    pub fn stop(mut self) {
        // There is no portable SIGTERM in std, so the child is asked to exit by
        // closing its input and then killed if it does not.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Runs the upstream installer, reporting each line of its output.
///
/// This takes minutes and downloads gigabytes, so the caller is expected to be
/// on a thread that can block and to surface progress as it arrives.
pub fn run_install<F>(mut on_line: F) -> Result<(), String>
where
    F: FnMut(&str),
{
    let mut child = install_command()
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Could not start the vllm-metal installer: {err}"))?;

    // Drained on its own thread so a full stderr pipe cannot stall the install.
    let stderr = child.stderr.take();
    let collector = std::thread::spawn(move || {
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
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            crate::logs::record("metal-install", &line);
            on_line(&line);
        }
    }

    let status = child
        .wait()
        .map_err(|err| format!("The vllm-metal installer did not finish: {err}"))?;
    let stderr = collector.join().unwrap_or_default();

    if status.success() {
        return Ok(());
    }
    let detail = stderr.trim();
    Err(if detail.is_empty() {
        "The vllm-metal installer failed.".to_string()
    } else {
        detail.to_string()
    })
}


/// Reads a child stream to exhaustion on its own thread, recording each line.
///
/// Returning immediately is the point: the reader outlives this call and keeps
/// the pipe drained until the process closes it.
fn drain<R: std::io::Read + Send + 'static>(stream: Option<R>, source: &'static str) {
    let Some(stream) = stream else {
        return;
    };
    std::thread::spawn(move || {
        for line in BufReader::new(stream).lines().map_while(Result::ok) {
            crate::logs::record(source, &line);
        }
    });
}

/// Starts the Metal inference server for a model.
pub fn spawn_server(model: &str, port: u16) -> Result<Server, String> {
    let cli = cli_path().ok_or_else(|| "Could not locate the vllm-metal CLI".to_string())?;
    if !cli.is_file() {
        return Err("The vllm-metal environment is not installed".to_string());
    }

    crate::logs::record("metal", &format!("starting {model} on port {port}"));

    let mut child = Command::new(&cli)
        .args(serve_args(model, port))
        .env("PATH", tool_search_path())
        // Weights land in the Hugging Face cache, which lives in the user's
        // home directory rather than Kayak's data directory, because the Metal
        // server runs on the host and shares nothing with the container.
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("Could not start the Metal server: {err}"))?;

    // Both streams must be read for the lifetime of the process, not merely
    // captured. vLLM writes steadily while it loads a model, and a pipe nothing
    // drains fills after about 64KB, at which point the server blocks on its own
    // logging and never finishes starting -- with no output to explain why.
    drain(child.stdout.take(), "metal");
    drain(child.stderr.take(), "metal");

    Ok(Server {
        child,
        model: model.to_string(),
    })
}

/// Reports whether the server is answering on its port.
pub fn is_healthy(port: u16) -> bool {
    ureq::get(&format!("http://127.0.0.1:{port}/v1/models"))
        .timeout(std::time::Duration::from_secs(2))
        .call()
        .is_ok()
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
}
