//! The key that lets the person using Kayak -- and nothing else -- use Kayak's
//! server.
//!
//! The server is published on the Mac's loopback only, but that is not the same as
//! being private to the user: every container on Docker Desktop can reach the Mac's
//! loopback through `host.docker.internal`. Code that ships with a model, or a
//! sandbox an agent was tricked into running something in, could otherwise read the
//! user's conversations, start agents on their paid keys, or approve its own tool
//! calls. Containers keep their internet access; only Kayak itself is closed to
//! them.
//!
//! Nothing here asks anything of the user. The key is made the first time it is
//! needed and handed to the server when its container is created. It never goes
//! to the browser: the launcher trades it for a one-time code, and opens the
//! browser on an address carrying only that code, which the server answers with
//! a session cookie and a redirect. An address is not a safe place for a secret
//! -- `open` passes it on a command line any process can read, and browsers keep
//! it in their history and sync it to other devices.
//!
//! The file sits beside the data folder, not in it: parts of the data folder are
//! mounted into containers, and nothing but the launcher and the server needs the
//! key.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;
use tauri::Url;

use crate::{control, logs, paths};

/// The variable the server reads its key from.
pub const ENV_NAME: &str = "KAYAK_AUTH_TOKEN";

/// The header the server accepts the key in, for requests the launcher makes itself.
pub const HEADER_NAME: &str = "X-Kayak-Token";

const FILE_NAME: &str = "access-key";

/// Bytes of randomness in a key; written out as twice as many hex characters.
const KEY_BYTES: usize = 32;

/// The first Kayak release whose server has a sign-in address at all. An older
/// server given a key would refuse the user, so it is given none.
const FIRST_SUPPORTING: (u64, u64, u64) = (1, 5, 0);

/// How long the server is given to hand out a sign-in code.
const CODE_TIMEOUT: Duration = Duration::from_secs(5);

/// This Mac's key, made on first use and the same at every launch after.
///
/// None only if it can be neither read nor written, in which case Kayak runs as
/// it did before keys existed rather than not at all.
pub fn key() -> Option<String> {
    static KEY: OnceLock<Option<String>> = OnceLock::new();
    KEY.get_or_init(|| match load_or_create(&key_path()) {
        Ok(key) => Some(key),
        Err(error) => {
            logs::record("access", &format!("running without an access key: {error}"));
            None
        }
    })
    .clone()
}

/// `~/Library/Application Support/Kayak/access-key`.
fn key_path() -> PathBuf {
    paths::beside_data(FILE_NAME)
}

/// Reads the key at `path`, or makes one there if there is none worth reading.
fn load_or_create(path: &Path) -> Result<String, String> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        let existing = existing.trim();
        if is_valid(existing) {
            return Ok(existing.to_string());
        }
    }
    let key = generate()?;
    // Owner-only: anyone who can read it can use Kayak as the user.
    control::replace_file(path, key.as_bytes(), 0o600)?;
    Ok(key)
}

fn is_valid(key: &str) -> bool {
    key.len() == KEY_BYTES * 2 && key.chars().all(|character| character.is_ascii_hexdigit())
}

fn generate() -> Result<String, String> {
    let mut bytes = [0u8; KEY_BYTES];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|err| format!("Could not make an access key: {err}"))?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Whether a Kayak image, by the version it was labelled with, understands keys.
///
/// An unlabelled image -- one built by hand -- is treated as too old: giving a key
/// to a server that cannot sign anyone in locks the user out, while not giving
/// one only leaves things as they were.
pub fn image_supports(version: Option<&str>) -> bool {
    let Some(version) = version else {
        return false;
    };
    let mut parts = version.trim().trim_start_matches('v').split('.');
    let mut next = || parts.next().and_then(|part| part.parse::<u64>().ok());
    match (next(), next(), next()) {
        (Some(major), Some(minor), Some(patch)) => (major, minor, patch) >= FIRST_SUPPORTING,
        _ => false,
    }
}

/// The address that opens Kayak, served at `base`, signed in.
///
/// `key` is the key the server holds, or None when it holds none -- an image
/// from before keys -- in which case Kayak needs no signing in at all. Makes a
/// request to the server, so it is never called on the main thread.
pub fn sign_in_address(base: &str, key: Option<&str>) -> Result<Url, String> {
    let Some(key) = key else {
        return plain_address(base);
    };
    match request_code(base, key)? {
        Some(code) => entry_address(base, "code", &code),
        // A release from before codes signs in with the key itself. The key is
        // in the address then, as it always was with those releases.
        None => entry_address(base, "token", key),
    }
}

/// Kayak's start page, without signing in: for a server that needs none, and as
/// the last resort when signing in failed -- the browser may still hold a
/// session from before, and if not, the page says how to get one.
pub fn plain_address(base: &str) -> Result<Url, String> {
    parse(&format!("{base}/"))
}

/// Asks the server for a one-time sign-in code, single use and short-lived.
///
/// None when the server answers that it has no such thing, which is how a
/// release from before codes is told apart from one that failed. Those
/// releases answer 405 rather than 404: the path matches the catch-all that
/// serves their pages, which only accepts GET.
fn request_code(base: &str, key: &str) -> Result<Option<String>, String> {
    #[derive(Deserialize)]
    struct Issued {
        code: String,
    }

    let response = ureq::post(&format!("{base}/api/auth/codes"))
        .set(HEADER_NAME, key)
        .timeout(CODE_TIMEOUT)
        .send_json(serde_json::json!({}));
    match response {
        Ok(response) => {
            let issued: Issued = response
                .into_json()
                .map_err(|err| format!("The sign-in code could not be read: {err}"))?;
            if issued.code.is_empty() {
                return Err("The server handed out an empty sign-in code".to_string());
            }
            Ok(Some(issued.code))
        }
        Err(ureq::Error::Status(404 | 405, _)) => Ok(None),
        Err(ureq::Error::Status(status, _)) => {
            Err(format!("The server refused a sign-in code ({status})"))
        }
        Err(error) => Err(format!("Could not ask for a sign-in code: {error}")),
    }
}

/// The server's sign-in address, carrying `name=value`, after which the
/// browser is sent to the start page.
fn entry_address(base: &str, name: &str, value: &str) -> Result<Url, String> {
    let mut url = parse(&format!("{base}/api/auth/enter"))?;
    url.query_pairs_mut().append_pair(name, value).append_pair("next", "/");
    Ok(url)
}

fn parse(address: &str) -> Result<Url, String> {
    address
        .parse()
        .map_err(|err| format!("Could not parse {address}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kayak-access-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(FILE_NAME)
    }

    #[test]
    fn makes_a_long_random_key_once_and_keeps_it() {
        let path = scratch("keeps");

        let first = load_or_create(&path).unwrap();
        let second = load_or_create(&path).unwrap();

        assert!(is_valid(&first));
        assert_eq!(first, second);
    }

    #[test]
    fn keeps_the_key_readable_only_by_the_user() {
        use std::os::unix::fs::PermissionsExt;
        let path = scratch("private");

        load_or_create(&path).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn replaces_a_file_that_does_not_hold_a_key() {
        let path = scratch("damaged");
        std::fs::write(&path, "not a key").unwrap();

        let key = load_or_create(&path).unwrap();

        assert!(is_valid(&key));
    }

    #[test]
    fn two_keys_are_never_the_same() {
        assert_ne!(generate().unwrap(), generate().unwrap());
    }

    #[test]
    fn only_releases_that_can_sign_in_get_a_key() {
        assert!(!image_supports(Some("1.4.0")));
        assert!(!image_supports(Some("1.4.12")));
        assert!(image_supports(Some("1.5.0")));
        assert!(image_supports(Some("1.10.0")));
        assert!(image_supports(Some("2.0.0")));
        assert!(image_supports(Some("v1.5.1")));
    }

    #[test]
    fn an_image_of_unknown_version_gets_none() {
        assert!(!image_supports(None));
        assert!(!image_supports(Some("")));
        assert!(!image_supports(Some("dev")));
        assert!(!image_supports(Some("1.5")));
    }

    /// Answers one request with `response`, returning the server's address
    /// and, once it has answered, the request it was sent.
    ///
    /// The whole request is read before answering: closing a connection with
    /// part of the request still unread resets it, and the client would see
    /// the reset rather than the answer.
    fn serve_once(response: &'static [u8]) -> (String, std::thread::JoinHandle<String>) {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let served = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !is_complete(&request) {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                }
            }
            let _ = stream.write_all(response);
            String::from_utf8_lossy(&request).to_string()
        });
        (format!("http://127.0.0.1:{port}"), served)
    }

    /// Whether `request` holds its headers and as much body as they announce.
    fn is_complete(request: &[u8]) -> bool {
        let text = String::from_utf8_lossy(request);
        let Some((head, body)) = text.split_once("\r\n\r\n") else {
            return false;
        };
        let length = head
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        body.len() >= length
    }

    fn query(url: &Url) -> Vec<(String, String)> {
        url.query_pairs()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn signs_in_with_a_one_time_code_and_keeps_the_key_out_of_the_address() {
        let (base, served) = serve_once(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 18\r\n\r\n{\"code\": \"once-1\"}",
        );

        let url = sign_in_address(&base, Some("the-secret-key")).unwrap();

        assert_eq!(url.path(), "/api/auth/enter");
        assert_eq!(
            query(&url),
            vec![
                ("code".to_string(), "once-1".to_string()),
                ("next".to_string(), "/".to_string()),
            ]
        );
        assert!(!url.as_str().contains("the-secret-key"));
        // The key went to the server in a header, on a request of its own.
        let request = served.join().unwrap();
        assert!(request.starts_with("POST /api/auth/codes "), "{request}");
        assert!(request.to_lowercase().contains("x-kayak-token: the-secret-key"));
    }

    #[test]
    fn a_release_from_before_codes_signs_in_with_the_key() {
        // 1.5.0 answers 405: its catch-all matches the path but only takes GET.
        // Treating that as a failure locked every one of its users out.
        for refusal in [
            &b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n"[..],
            &b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"[..],
        ] {
            let (base, _served) = serve_once(refusal);

            let url = sign_in_address(&base, Some("abc")).unwrap();

            assert_eq!(
                query(&url),
                vec![
                    ("token".to_string(), "abc".to_string()),
                    ("next".to_string(), "/".to_string()),
                ]
            );
        }
    }

    #[test]
    fn a_server_without_a_key_is_opened_plainly() {
        let url = sign_in_address("http://127.0.0.1:8000", None).unwrap();

        assert_eq!(url.as_str(), "http://127.0.0.1:8000/");
        assert_eq!(plain_address("http://127.0.0.1:8000").unwrap(), url);
    }

    #[test]
    fn a_server_that_fails_to_hand_out_a_code_is_not_given_the_key_in_an_address() {
        // Falling back to the key here would put it in the address for exactly
        // the servers that are meant to keep it out.
        let (base, _served) =
            serve_once(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n");

        assert!(sign_in_address(&base, Some("abc")).is_err());
    }
}
