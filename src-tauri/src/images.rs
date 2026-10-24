//! Which Kayak images are installed, what they replaced, and which release to
//! leave alone.
//!
//! A new Kayak is downloaded ahead of being run -- in the background while the
//! current one is in use, or at launch -- and only proves itself when its server
//! starts. Until then the images it replaced are kept, and recorded on disk, so
//! a release that will not start can be undone even when it is first run at a
//! later launch than the one that downloaded it. A release undone that way is
//! remembered too, so it is not downloaded and tried again at every launch.

use serde::{Deserialize, Serialize};

use crate::{config, docker, logs, paths};

/// The images Kayak ran from before the newest download, until Kayak has run
/// from the new ones.
const PREVIOUS_FILE: &str = "previous-kayak.json";

/// The release whose server would not start, by its digest.
const SKIPPED_FILE: &str = "skipped-kayak-update";

/// The image each of Kayak's tags pointed at, at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Previous {
    images: Vec<Image>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Image {
    tag: String,
    id: Option<String>,
}

impl Previous {
    /// What the tags point at now.
    ///
    /// Taken before a download: repointing a tag leaves the image it replaced
    /// behind, untagged and several gigabytes large, and after the download
    /// there is no way to tell which of the untagged images used to be ours.
    pub fn note() -> Self {
        Self {
            images: [config::server_image(), config::sandbox_image()]
                .into_iter()
                .map(|tag| {
                    let id = docker::image_id(&tag);
                    Image { tag, id }
                })
                .collect(),
        }
    }

    pub fn ids(&self) -> Vec<String> {
        self.images.iter().filter_map(|image| image.id.clone()).collect()
    }

    /// Reports whether any tag points somewhere else now.
    pub fn moved(&self) -> bool {
        self.images
            .iter()
            .any(|image| image.id.is_some() && docker::image_id(&image.tag) != image.id)
    }

    /// Points each tag back at the image it named, and returns the images that
    /// were downloaded in their place.
    ///
    /// The downloaded images are only returned, not deleted: a server started
    /// from one may still be using it, and Docker refuses to delete an image a
    /// container uses.
    pub fn retag(&self) -> Vec<String> {
        let mut downloaded = Vec::new();
        for Image { tag, id } in &self.images {
            let Some(previous) = id else {
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

    /// Keeps this as the version to go back to, unless one is kept already.
    /// Returns whether this call kept it.
    ///
    /// The one already kept wins: it is the last version known to start, and a
    /// second download before the first was ever run has proved nothing yet.
    pub fn keep(&self) -> bool {
        if Self::kept().is_some() {
            return false;
        }
        let written = serde_json::to_vec(self)
            .map_err(|error| error.to_string())
            .and_then(|body| paths::write_beside_data(PREVIOUS_FILE, &body));
        match written {
            Ok(()) => true,
            Err(error) => {
                logs::record("update", &format!("could not remember the previous version: {error}"));
                false
            }
        }
    }

    /// The version kept to go back to, if Kayak has not yet run from the
    /// images that replaced it.
    pub fn kept() -> Option<Self> {
        let body = std::fs::read(paths::beside_data(PREVIOUS_FILE)).ok()?;
        serde_json::from_slice(&body).ok()
    }

    /// Forgets the version kept to go back to.
    pub fn forget() {
        let _ = std::fs::remove_file(paths::beside_data(PREVIOUS_FILE));
    }
}

/// Kayak has run from its current images, so the ones they replaced are no
/// longer needed.
///
/// Only now: Docker refuses to remove an image a container still references,
/// and until the new server has started there is nothing to say the old images
/// will not be needed again.
pub fn confirm() {
    if let Some(previous) = Previous::kept() {
        remove_superseded(&previous.ids());
        Previous::forget();
    }
}

/// Remembers a release whose server would not start, so it is not downloaded
/// and tried again by itself. A newer release, with another digest, is.
pub fn skip(digest: &str) {
    logs::record("update", &format!("leaving the release {digest} alone from now on"));
    if let Err(error) = paths::write_beside_data(SKIPPED_FILE, digest.as_bytes()) {
        logs::record("update", &format!("could not remember that: {error}"));
    }
}

/// Whether the release with this digest is one to leave alone.
pub fn is_skipped(digest: &str) -> bool {
    std::fs::read_to_string(paths::beside_data(SKIPPED_FILE))
        .is_ok_and(|skipped| skipped.trim() == digest)
}

/// After a download, the images it replaced that are not the version kept to go
/// back to.
///
/// Those are a release downloaded earlier and never run -- two releases between
/// launches -- which nothing will ever use again, and which nothing else would
/// ever delete: it is neither tagged nor recorded, and several gigabytes.
///
/// Nothing, when no version is kept: then the images replaced are the only way
/// back, unrecorded or not.
pub fn replaced_unkept(before: &Previous, kept: Option<&Previous>) -> Vec<String> {
    let Some(kept) = kept else {
        return Vec::new();
    };
    let keep = kept.ids();
    before.ids().into_iter().filter(|id| !keep.contains(id)).collect()
}

/// Deletes images by ID. Failures are ignored: an image something still uses
/// is one that should stay.
pub fn remove_images(ids: &[String]) {
    for id in ids {
        let _ = docker::remove_image(id);
    }
}

/// Deletes images that have been replaced.
pub fn remove_superseded(ids: &[String]) {
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

#[cfg(test)]
mod tests {
    use super::*;

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

    fn snapshot(server: &str, sandbox: &str) -> Previous {
        Previous {
            images: vec![
                Image {
                    tag: "omaraflak/kayak:latest".to_string(),
                    id: Some(server.to_string()),
                },
                Image {
                    tag: "omaraflak/kayak-sandbox:latest".to_string(),
                    id: Some(sandbox.to_string()),
                },
            ],
        }
    }

    #[test]
    fn a_release_downloaded_and_never_run_is_not_kept_forever() {
        // A is the version that last ran, kept to go back to; B was downloaded
        // later and never run; now C replaces B. B must go.
        let kept = snapshot("sha256:server-a", "sha256:sandbox-a");
        let before = snapshot("sha256:server-b", "sha256:sandbox-a");

        assert_eq!(
            replaced_unkept(&before, Some(&kept)),
            vec!["sha256:server-b".to_string()]
        );
    }

    #[test]
    fn the_version_kept_to_go_back_to_is_never_among_them() {
        let kept = snapshot("sha256:server-a", "sha256:sandbox-a");

        assert!(replaced_unkept(&kept, Some(&kept)).is_empty());
        // Unrecorded, what was replaced is the only way back.
        assert!(replaced_unkept(&kept, None).is_empty());
    }

    #[test]
    fn the_version_to_go_back_to_survives_a_restart_of_the_app() {
        // Written as the file is, read back as the next launch would.
        let previous = Previous {
            images: vec![
                Image {
                    tag: "omaraflak/kayak:latest".to_string(),
                    id: Some("sha256:old".to_string()),
                },
                Image {
                    tag: "omaraflak/kayak-sandbox:latest".to_string(),
                    id: None,
                },
            ],
        };

        let body = serde_json::to_vec(&previous).unwrap();
        let read: Previous = serde_json::from_slice(&body).unwrap();

        assert_eq!(read, previous);
        assert_eq!(read.ids(), vec!["sha256:old".to_string()]);
    }
}
