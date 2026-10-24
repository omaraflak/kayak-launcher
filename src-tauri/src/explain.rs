//! Turns what went wrong into something a person can act on.
//!
//! Kayak is for people who have never opened a terminal, and the launcher's
//! failures arrive as Docker's stderr, container logs and Rust error text. The
//! failure screen used to print those verbatim. This module is the one place
//! that decides what the user is told instead: what happened, in everyday
//! words, and the one thing to do next. The raw text is never thrown away --
//! it goes to the launcher log, which "Copy details" and Help › Show Kayak Log
//! both reach.
//!
//! Failures the launcher itself detects carry one of the phrases below, so the
//! classifier recognises them by the same constant that produced them rather
//! than by a copy of their wording.

/// The Kayak server's container exited while the launcher waited for it.
pub const SERVER_EXITED: &str = "The Kayak server stopped right after starting";

/// The Kayak server kept running but never answered its health check.
pub const HEALTH_TIMEOUT: &str = "The Kayak server did not finish starting in time";

/// Kayak is serving, but the window that shows it could not be created.
pub const WINDOW_FAILED: &str = "The Kayak window could not be opened";

/// A Docker command was still running when its time limit ran out.
pub const DOCKER_UNRESPONSIVE: &str = "Docker Desktop did not respond in time";

/// Docker Desktop stopped running while the launcher was using it.
pub const DOCKER_STOPPED: &str = "Docker Desktop stopped running";

/// macOS could not be asked to open Docker Desktop at all.
pub const DOCKER_WONT_OPEN: &str = "Docker Desktop could not be opened";

/// Docker Desktop was opened but never started answering.
pub const DOCKER_SLOW_TO_START: &str = "Docker Desktop did not finish starting";

/// Work abandoned because the app is quitting. Never shown to anyone.
pub const SHUTTING_DOWN: &str = "Kayak is quitting";

/// What kind of failure something was, which decides both what the user is
/// told and what "Try again" does first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    Offline,
    DiskFull,
    RateLimited,
    DockerStopped,
    DockerUnresponsive,
    DockerWontOpen,
    DockerSlowToStart,
    ServerExited,
    /// The server stopped while Kayak was in use, rather than while starting.
    ServerStopped,
    HealthTimeout,
    WindowFailed,
    Unknown,
}

impl Problem {
    /// What happened, as the headline of the failure screen.
    pub fn headline(self) -> &'static str {
        match self {
            Self::Offline => "Couldn't download Kayak",
            Self::DiskFull => "Your Mac is out of space",
            Self::RateLimited => "The download service is busy",
            Self::DockerStopped => "Docker Desktop stopped",
            Self::DockerUnresponsive => "Docker Desktop isn't responding",
            Self::DockerWontOpen => "Couldn't open Docker Desktop",
            Self::DockerSlowToStart => "Docker Desktop didn't finish starting",
            Self::ServerStopped => "Kayak stopped unexpectedly",
            Self::HealthTimeout => "Kayak is taking longer than usual to start",
            Self::WindowFailed => "Kayak is running, but its window didn't open",
            Self::ServerExited | Self::Unknown => "Kayak couldn't start",
        }
    }

    /// The one thing to do next.
    pub fn body(self) -> &'static str {
        match self {
            Self::Offline => "Check your internet connection, then click Try again.",
            Self::DiskFull => {
                "Free up some space on your Mac, or raise the disk limit in Docker Desktop's \
                 settings, then click Try again."
            }
            Self::RateLimited => "Wait a few minutes, then click Try again.",
            Self::DockerStopped => "Open Docker Desktop again, then click Try again.",
            Self::DockerUnresponsive => {
                "Quit Docker Desktop from its icon in the menu bar, open it again, then click \
                 Try again."
            }
            Self::DockerWontOpen => {
                "Open Docker Desktop from your Applications folder, then click Try again."
            }
            Self::DockerSlowToStart => {
                "Open Docker Desktop, wait until it says it's running, then click Try again."
            }
            Self::ServerStopped => "Your work is saved. Click Try again to start Kayak again.",
            Self::HealthTimeout => "Your Mac may be busy. Click Try again to keep waiting.",
            Self::WindowFailed => "Click Try again.",
            Self::ServerExited | Self::Unknown => {
                "Click Try again. If it keeps happening, click Copy details and share them when \
                 you ask for help."
            }
        }
    }
}

/// Works out what kind of failure a piece of raw error text describes.
///
/// Order matters. A full disk is checked before the launcher's own phrases
/// because a server that stopped right after starting usually says why in the
/// logs carried with it, and "out of space" is the one cause there the user can
/// fix. The launcher's phrases come before the network patterns because a
/// container's logs can mention a network hiccup that is not why it stopped.
pub fn classify(raw: &str) -> Problem {
    let text = raw.to_ascii_lowercase();
    let has = |needle: &str| text.contains(&needle.to_ascii_lowercase());

    if has(DOCKER_UNRESPONSIVE) {
        Problem::DockerUnresponsive
    } else if has("no space left on device") || has("disk is full") {
        Problem::DiskFull
    } else if has(SERVER_EXITED) {
        Problem::ServerExited
    } else if has(HEALTH_TIMEOUT) {
        Problem::HealthTimeout
    } else if has(WINDOW_FAILED) {
        Problem::WindowFailed
    } else if has(DOCKER_WONT_OPEN) {
        Problem::DockerWontOpen
    } else if has(DOCKER_SLOW_TO_START) {
        Problem::DockerSlowToStart
    } else if has("toomanyrequests") {
        Problem::RateLimited
    } else if has(DOCKER_STOPPED)
        || has("cannot connect to the docker daemon")
        || has("is the docker daemon running")
        || has("docker.sock: connect")
    {
        Problem::DockerStopped
    } else if [
        "no such host",
        "dial tcp",
        "i/o timeout",
        "tls handshake",
        "network is unreachable",
        "connection reset by peer",
        "could not reach docker hub",
    ]
    .iter()
    .any(|needle| has(needle))
    {
        Problem::Offline
    } else {
        Problem::Unknown
    }
}

/// Title of every "the update did not go through" message.
pub const UPDATE_FAILED: &str = "Couldn't update Kayak";

/// What to say after a Kayak update failed and the previous version was put
/// back.
///
/// The reassurance comes first because it is the thing the user most needs to
/// know. The advice that follows is specific only where there is something the
/// user can do about the cause; anything else is a problem with the release or
/// the machine, for which trying later is the honest answer.
pub fn update_failure(raw: &str) -> String {
    let advice = match classify(raw) {
        problem @ (Problem::Offline
        | Problem::DiskFull
        | Problem::RateLimited
        | Problem::DockerStopped
        | Problem::DockerUnresponsive) => problem.body(),
        _ => "You can try again later.",
    };
    format!("Kayak is still on the version you had. {advice}")
}

/// What to say when the app's own update could not be downloaded or installed.
///
/// A read-only file system is the one failure with a specific fix: macOS runs
/// an app opened straight from the downloaded disk image, or one it has
/// quarantined, from a read-only location the update cannot be written into.
pub fn launcher_update_failure(raw: &str) -> &'static str {
    let text = raw.to_ascii_lowercase();
    if text.contains("read-only file system") || text.contains("os error 30") {
        "Move Kayak to your Applications folder, open it from there, then try again."
    } else {
        "Couldn't download the update. Check your internet connection and try again."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_failures_ask_for_the_internet_connection() {
        for raw in [
            "Error response from daemon: Get \"https://registry-1.docker.io/v2/\": dial tcp: \
             lookup registry-1.docker.io: no such host",
            "net/http: TLS handshake timeout",
            "read tcp 192.168.65.3:4242->1.2.3.4:443: i/o timeout",
            "connect: network is unreachable",
        ] {
            assert_eq!(classify(raw), Problem::Offline, "{raw}");
        }
        let problem = classify("dial tcp: lookup registry-1.docker.io: no such host");
        assert_eq!(problem.headline(), "Couldn't download Kayak");
        assert_eq!(
            problem.body(),
            "Check your internet connection, then click Try again."
        );
    }

    #[test]
    fn a_full_disk_says_so_even_inside_container_logs() {
        let raw = format!("{SERVER_EXITED}:\nsqlite3.OperationalError: No space left on device");
        assert_eq!(classify(&raw), Problem::DiskFull);
        assert_eq!(
            classify("write /var/lib/docker/tmp/x: no space left on device"),
            Problem::DiskFull
        );
    }

    #[test]
    fn rate_limits_are_not_mistaken_for_being_offline() {
        assert_eq!(
            classify("toomanyrequests: You have reached your pull rate limit. dial tcp ok"),
            Problem::RateLimited
        );
    }

    #[test]
    fn a_stopped_docker_is_recognised() {
        assert_eq!(
            classify(&format!("{DOCKER_STOPPED} during the download")),
            Problem::DockerStopped
        );
        assert_eq!(
            classify(
                "Cannot connect to the Docker daemon at unix:///var/run/docker.sock. \
                 Is the docker daemon running?"
            ),
            Problem::DockerStopped
        );
    }

    #[test]
    fn the_launchers_own_failures_are_recognised_by_their_phrases() {
        assert_eq!(
            classify(&format!("{SERVER_EXITED}:\nTraceback ...")),
            Problem::ServerExited
        );
        assert_eq!(classify(HEALTH_TIMEOUT), Problem::HealthTimeout);
        assert_eq!(
            classify(&format!("{WINDOW_FAILED}: boom")),
            Problem::WindowFailed
        );
        assert_eq!(classify(DOCKER_SLOW_TO_START), Problem::DockerSlowToStart);
        assert_eq!(
            classify(&format!("{DOCKER_WONT_OPEN}: no open")),
            Problem::DockerWontOpen
        );
        assert_eq!(
            classify(&format!("{DOCKER_UNRESPONSIVE} (docker inspect)")),
            Problem::DockerUnresponsive
        );
    }

    #[test]
    fn a_server_that_exited_is_not_blamed_on_the_network() {
        // Kayak logs a failed reach for a model list at startup; that is not
        // why it stopped, and "check your internet" would send the user the
        // wrong way.
        let raw = format!("{SERVER_EXITED}:\nWARNING could not refresh catalog: dial tcp");
        assert_eq!(classify(&raw), Problem::ServerExited);
    }

    #[test]
    fn anything_else_gets_the_general_message() {
        assert_eq!(
            classify("No free port between 8000 and 8020"),
            Problem::Unknown
        );
        assert_eq!(
            classify("Could not create /Users/someone/Library: Permission denied").headline(),
            "Kayak couldn't start"
        );
    }

    #[test]
    fn no_explanation_contains_technical_vocabulary() {
        let problems = [
            Problem::Offline,
            Problem::DiskFull,
            Problem::RateLimited,
            Problem::DockerStopped,
            Problem::DockerUnresponsive,
            Problem::DockerWontOpen,
            Problem::DockerSlowToStart,
            Problem::ServerExited,
            Problem::ServerStopped,
            Problem::HealthTimeout,
            Problem::WindowFailed,
            Problem::Unknown,
        ];
        for problem in problems {
            let words = format!("{} {}", problem.headline(), problem.body()).to_lowercase();
            for banned in [
                "container",
                "image",
                "daemon",
                "socket",
                "pull",
                "port",
                "vllm",
                "metal",
            ] {
                assert!(
                    !words.contains(banned),
                    "{problem:?} says {banned}: {words}"
                );
            }
        }
    }

    #[test]
    fn a_failed_update_reassures_first_and_advises_only_when_it_can() {
        assert_eq!(
            update_failure("dial tcp: no such host"),
            "Kayak is still on the version you had. Check your internet connection, then click \
             Try again."
        );
        assert_eq!(
            update_failure(&format!("{SERVER_EXITED}:\nImportError")),
            "Kayak is still on the version you had. You can try again later."
        );
    }

    #[test]
    fn a_launcher_running_from_a_read_only_place_is_told_where_to_move() {
        assert!(
            launcher_update_failure("Read-only file system (os error 30)")
                .starts_with("Move Kayak to your Applications folder")
        );
        assert!(launcher_update_failure("error sending request for url")
            .starts_with("Couldn't download the update"));
    }
}
