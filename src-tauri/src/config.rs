//! Names and defaults that describe the Kayak deployment this launcher manages.

/// Docker Hub repository holding the Kayak server image.
pub const SERVER_REPO: &str = "omaraflak/kayak";

/// Docker Hub repository holding the image agents get sandboxed into.
pub const SANDBOX_REPO: &str = "omaraflak/kayak-sandbox";

/// Tag both repositories are published under.
pub const TAG: &str = "latest";

/// The server reads `KAYAK_SANDBOX_IMAGE` to decide which image to start
/// sandboxes from, and defaults to this unqualified name. The pulled sandbox
/// image is retagged to it so the server finds it without extra configuration.
pub const SANDBOX_LOCAL_TAG: &str = "kayak-sandbox:latest";

/// Name of the container the launcher owns. A fixed name is what lets a second
/// launch reattach to an already-running Kayak instead of starting a duplicate.
///
/// Deliberately not `kayak-server`, which is the name the project's
/// docker-compose service uses: the launcher removes and recreates whatever
/// holds this name, and sharing it would destroy a developer's compose
/// container the first time they opened the app.
pub const CONTAINER_NAME: &str = "kayak-app";

/// Port the server listens on inside the container.
pub const INTERNAL_PORT: u16 = 8000;

/// First host port tried. Later ports are probed if this one is taken.
pub const PREFERRED_PORT: u16 = 8000;

/// How many ports to try before giving up.
pub const PORT_SCAN_RANGE: u16 = 20;

/// Docker socket, mounted so the server can start sandbox containers as
/// siblings on the host daemon rather than nesting a daemon inside itself.
pub const DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// How long to wait for the server to answer its health endpoint before
/// treating the start as failed.
pub const HEALTH_TIMEOUT_SECS: u64 = 180;

/// Where users are sent when Docker is missing.
pub const DOCKER_INSTALL_URL: &str = "https://www.docker.com/products/docker-desktop/";

pub fn server_image() -> String {
    format!("{SERVER_REPO}:{TAG}")
}

pub fn sandbox_image() -> String {
    format!("{SANDBOX_REPO}:{TAG}")
}
