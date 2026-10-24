//! The key that lets Kayak's own window -- and nothing else -- use Kayak's server.
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
//! needed, handed to the server when its container is created, and given to the
//! Kayak window through a one-time sign-in address that the server answers with a
//! session cookie and a redirect.
//!
//! The file sits beside the data folder, not in it: parts of the data folder are
//! mounted into containers, and nothing but the launcher and the server needs the
//! key.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tauri::Url;

use crate::{control, logs, paths};

/// The variable the server reads its key from.
pub const ENV_NAME: &str = "KAYAK_AUTH_TOKEN";

/// The header the server accepts the key in, for requests the launcher makes itself.
pub const HEADER_NAME: &str = "X-Kayak-Token";

const FILE_NAME: &str = "access-key";

/// Bytes of randomness in a key; written out as twice as many hex characters.
const KEY_BYTES: usize = 32;

/// The first Kayak release whose server has the sign-in address the window uses.
/// An older server given a key would refuse its own window, so it is given none.
const FIRST_SUPPORTING: (u64, u64, u64) = (1, 5, 0);

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
    let data = paths::data_dir();
    data.parent().unwrap_or(&data).join(FILE_NAME)
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
/// to a server that cannot sign its window in locks the user out, while not
/// giving one only leaves things as they were.
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

/// The address that signs the window in with `key`, then shows `page`.
///
/// `page` is a path on the same server, such as `/?fresh=…`; the server refuses
/// to send the window anywhere else.
pub fn entry_url(base: &str, page: &str, key: &str) -> Result<Url, String> {
    let mut url: Url = format!("{base}/api/auth/enter")
        .parse()
        .map_err(|err| format!("Could not parse {base}: {err}"))?;
    url.query_pairs_mut().append_pair("token", key).append_pair("next", page);
    Ok(url)
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

    #[test]
    fn the_sign_in_address_carries_the_page_to_show_afterwards() {
        let url = entry_url("http://127.0.0.1:8000", "/?fresh=12-0", "abc").unwrap();

        assert_eq!(url.path(), "/api/auth/enter");
        let pairs: Vec<(String, String)> = url
            .query_pairs()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        assert_eq!(
            pairs,
            vec![
                ("token".to_string(), "abc".to_string()),
                ("next".to_string(), "/?fresh=12-0".to_string()),
            ]
        );
    }
}
