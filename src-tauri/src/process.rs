//! Running child processes the launcher cannot afford to trust.
//!
//! Two problems recur across everything the launcher starts. A command can
//! hang -- Docker Desktop in a bad state accepts a connection and then answers
//! nothing at all -- and a process can outlive the launcher, because killing a
//! child stops only that child and not whatever it started in turn. vLLM is the
//! case that matters: its server runs the model in a separate engine process,
//! and that process kept the model in memory after the launcher had quit.
//!
//! Both answers live here so Docker calls, the GPU installer and the GPU server
//! share one implementation of each.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// How often a running command is checked for having finished.
const POLL: Duration = Duration::from_millis(10);

/// Why a command produced no output.
#[derive(Debug)]
pub enum RunError {
    /// The command could not be started, or waiting on it failed.
    Spawn(std::io::Error),
    /// The command was still running when its time ran out, and was killed.
    TimedOut,
}

/// Runs a command to completion, killing it if it runs longer than `limit`.
///
/// Both streams are read on their own threads while the process runs: waiting
/// first and reading afterwards deadlocks any process that writes more than a
/// pipe holds. On a timeout those readers are left to finish by themselves --
/// a helper the command started can hold the pipe open after the command is
/// killed, and joining them would bring the hang straight back.
pub fn output_within(command: &mut Command, limit: Duration) -> Result<Output, RunError> {
    run_to_end(command, limit, None)
}

/// [`output_within`] for a command that must not outlive the launcher.
///
/// The command leads a process group of its own, which `on_group` is told
/// about as soon as it exists -- with `Some` -- and again with `None` the
/// moment it has finished, so whoever is keeping track can stop it while it
/// runs and stops holding on to its ID once it is gone. A timeout kills the
/// whole group rather than only the command.
pub fn output_within_group(
    command: &mut Command,
    limit: Duration,
    on_group: &dyn Fn(Option<Group>),
) -> Result<Output, RunError> {
    command.process_group(0);
    run_to_end(command, limit, Some(on_group))
}

fn run_to_end(
    command: &mut Command,
    limit: Duration,
    on_group: Option<&dyn Fn(Option<Group>)>,
) -> Result<Output, RunError> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(RunError::Spawn)?;
    let group = on_group.map(|on_group| {
        let group = Group::of(&child);
        on_group(Some(group));
        group
    });
    // Reports the group gone however this returns.
    let finished = || {
        if let Some(on_group) = on_group {
            on_group(None);
        }
    };

    let stdout = read_in_background(child.stdout.take());
    let stderr = read_in_background(child.stderr.take());

    let deadline = Instant::now() + limit;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                match group {
                    Some(group) => group.kill(),
                    None => {
                        let _ = child.kill();
                    }
                }
                let _ = child.wait();
                finished();
                return Err(RunError::TimedOut);
            }
            Ok(None) => std::thread::sleep(POLL),
            Err(error) => {
                finished();
                return Err(RunError::Spawn(error));
            }
        }
    };
    finished();

    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn read_in_background<R: Read + Send + 'static>(
    stream: Option<R>,
) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut collected = Vec::new();
        if let Some(mut stream) = stream {
            let _ = stream.read_to_end(&mut collected);
        }
        collected
    })
}

/// A process group: a child started as the leader of a group of its own, plus
/// everything it starts in turn.
///
/// Signalling the group rather than the child is what reaches the processes
/// the child spawned, which a plain `kill` leaves running. Children opt in by
/// being spawned with `CommandExt::process_group(0)`, which makes the group ID
/// equal to the child's PID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Group(libc::pid_t);

impl Group {
    /// The group a child leads. Only meaningful for a child spawned with
    /// `process_group(0)`.
    pub fn of(child: &Child) -> Self {
        Self(child.id() as libc::pid_t)
    }

    /// A group recorded earlier, such as one read back from a PID file.
    ///
    /// Zero and one are refused rather than trusted: `killpg(0)` signals the
    /// launcher's own group, and group 1 is launchd's. Either arriving from a
    /// damaged file must not turn a cleanup into killing the wrong thing.
    pub fn from_id(id: i64) -> Option<Self> {
        libc::pid_t::try_from(id)
            .ok()
            .filter(|id| *id > 1)
            .map(Self)
    }

    pub fn id(self) -> i64 {
        i64::from(self.0)
    }

    fn signal(self, signal: libc::c_int) -> bool {
        // SAFETY: killpg has no memory-safety preconditions; `from_id` and
        // `of` guarantee the ID is a real group rather than 0 or 1.
        unsafe { libc::killpg(self.0, signal) == 0 }
    }

    /// Kills every process in the group at once.
    pub fn kill(self) {
        self.signal(libc::SIGKILL);
    }

    /// Reports whether no process is left in the group.
    ///
    /// An exited leader that has not been reaped still counts, so callers that
    /// own the leader reap it before asking.
    pub fn is_empty(self) -> bool {
        // SAFETY: as in `signal`; signal 0 only checks for existence.
        let alive = unsafe { libc::killpg(self.0, 0) } == 0;
        !alive && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    /// Asks every process in the group to exit, and kills whatever is still
    /// running once `grace` has passed.
    ///
    /// SIGTERM first because vLLM holds gigabytes of model weights and a GPU
    /// context, and given the chance it releases them properly; killing it
    /// outright leaves the OS to reclaim them, which on a quick stop and start
    /// can make the next launch fail to allocate. `reap` is called while
    /// waiting so a caller that owns the leader can collect it, which an
    /// exited-but-unreaped leader needs before the group can read as empty.
    pub fn terminate(self, grace: Duration, mut reap: impl FnMut()) {
        if !self.signal(libc::SIGTERM) {
            reap();
            return;
        }

        let deadline = Instant::now() + grace;
        loop {
            reap();
            if self.is_empty() {
                return;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        crate::logs::record(
            "process",
            &format!("group {} ignored the request to stop; killing it", self.0),
        );
        self.signal(libc::SIGKILL);
        // SIGKILL cannot be refused, but delivery and exit are not instant;
        // a short wait lets the leader be reaped rather than left a zombie.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            reap();
            if self.is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Writes the group ID to a file, so a later launch can find the group if
    /// this one ends without stopping it.
    pub fn record(self, path: &Path) {
        if let Err(error) = std::fs::write(path, self.0.to_string()) {
            crate::logs::record(
                "process",
                &format!("could not record process group {}: {error}", self.0),
            );
        }
    }

    /// Reads back a group written by [`Group::record`].
    pub fn recorded(path: &Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        Self::from_id(raw.trim().parse().ok()?)
    }

    /// The command lines of every process currently in the group.
    pub fn commands(self) -> Vec<String> {
        let listing = output_within(
            Command::new("/bin/ps").args(["-axo", "pgid=,command="]),
            Duration::from_secs(5),
        );
        match listing {
            Ok(output) => group_commands(&String::from_utf8_lossy(&output.stdout), self.0),
            Err(_) => Vec::new(),
        }
    }
}

/// Picks the command lines belonging to one group out of a `ps` listing of
/// `pgid command` lines.
fn group_commands(listing: &str, group: libc::pid_t) -> Vec<String> {
    listing
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (id, command) = line.split_once(char::is_whitespace)?;
            (id.parse::<libc::pid_t>().ok()? == group).then(|| command.trim().to_string())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn returns_what_a_quick_command_printed() {
        let output = output_within(
            Command::new("/bin/echo").arg("hello"),
            Duration::from_secs(5),
        )
        .unwrap();

        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hello\n");
    }

    #[test]
    fn a_command_that_hangs_is_killed_at_its_limit() {
        // The Docker Desktop failure this exists for: a call that never answers.
        let started = Instant::now();
        let result = output_within(
            Command::new("/bin/sleep").arg("30"),
            Duration::from_millis(200),
        );

        assert!(matches!(result, Err(RunError::TimedOut)));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_grouped_command_is_reported_while_it_runs_and_killed_with_its_children() {
        use std::sync::Mutex;
        let seen: Mutex<Vec<Option<Group>>> = Mutex::new(Vec::new());

        let result = output_within_group(
            Command::new("/bin/sh").args(["-c", "sleep 30 & sleep 30"]),
            Duration::from_millis(300),
            &|group| seen.lock().unwrap().push(group),
        );

        assert!(matches!(result, Err(RunError::TimedOut)));
        let seen = seen.into_inner().unwrap();
        // Told about the group, then told it is gone -- so nothing holds on to
        // an ID the system may hand to someone else.
        assert_eq!(seen.len(), 2);
        let group = seen[0].expect("the group is reported first");
        assert_eq!(seen[1], None);
        std::thread::sleep(Duration::from_millis(200));
        assert!(
            group.is_empty(),
            "the background sleep outlived the timeout"
        );
    }

    #[test]
    fn a_missing_program_is_a_spawn_error_not_a_timeout() {
        let result = output_within(
            &mut Command::new("/nonexistent/kayak-test-binary"),
            Duration::from_secs(5),
        );

        assert!(matches!(result, Err(RunError::Spawn(_))));
    }

    #[test]
    fn group_ids_that_would_signal_the_wrong_processes_are_refused() {
        assert_eq!(Group::from_id(0), None);
        assert_eq!(Group::from_id(1), None);
        assert_eq!(Group::from_id(-42), None);
        assert_eq!(Group::from_id(i64::MAX), None);
        assert_eq!(Group::from_id(4242).map(Group::id), Some(4242));
    }

    #[test]
    fn terminating_a_group_stops_the_processes_its_leader_started() {
        // The leader starts a background process and waits on another. Killing
        // only the leader is exactly what left vLLM's engine running.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "sleep 30 & sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Group::of(&child);
        std::thread::sleep(Duration::from_millis(200));
        assert!(!group.is_empty());

        group.terminate(Duration::from_secs(5), || {
            let _ = child.try_wait();
        });

        assert!(group.is_empty());
    }

    #[test]
    fn a_group_that_ignores_the_request_to_stop_is_killed() {
        // An ignored SIGTERM is inherited across exec, so the sleep ignores it
        // too, and only the SIGKILL that follows the grace period stops it.
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; sleep 30"])
            .process_group(0)
            .spawn()
            .unwrap();
        let group = Group::of(&child);
        std::thread::sleep(Duration::from_millis(200));

        let started = Instant::now();
        group.terminate(Duration::from_millis(300), || {
            let _ = child.try_wait();
        });

        assert!(group.is_empty());
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_recorded_group_reads_back() {
        let dir = std::env::temp_dir().join("kayak-launcher-pidfile");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metal.pid");

        Group::from_id(31337).unwrap().record(&path);
        assert_eq!(Group::recorded(&path), Group::from_id(31337));

        // A damaged file must not become a group to signal.
        std::fs::write(&path, "0\n").unwrap();
        assert_eq!(Group::recorded(&path), None);
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(Group::recorded(&path), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn picks_one_groups_commands_out_of_a_listing() {
        let listing = "    1 /sbin/launchd\n\
                       4242 /Users/me/.venv-vllm-metal/bin/python3 /Users/me/.venv-vllm-metal/bin/vllm serve x\n\
                        4242 /Users/me/.venv-vllm-metal/bin/python3 -c from multiprocessing.spawn import spawn_main\n\
                       42420 /usr/bin/unrelated\n";

        let commands = group_commands(listing, 4242);

        assert_eq!(commands.len(), 2);
        assert!(commands[0].ends_with("vllm serve x"));
        assert!(commands[1].contains("multiprocessing"));
    }
}
