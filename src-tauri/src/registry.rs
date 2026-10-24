//! Asks Docker Hub what the published version of an image currently is.
//!
//! Update detection compares digests rather than version strings. The digest of
//! a tag is what `docker pull` resolves that tag to, so comparing the published
//! digest against the locally pulled one answers exactly the question that
//! matters -- "would pulling again get me something different?" -- without
//! depending on the image carrying a version label at all.

use std::time::Duration;

use serde::Deserialize;

const HUB_API: &str = "https://hub.docker.com/v2/repositories";
const TIMEOUT: Duration = Duration::from_secs(20);

/// What Docker Hub reports for a tag.
#[derive(Debug, Clone)]
pub struct RemoteTag {
    pub digest: String,
}

#[derive(Deserialize)]
struct TagResponse {
    digest: Option<String>,
    images: Option<Vec<TagImage>>,
}

#[derive(Deserialize)]
struct TagImage {
    digest: Option<String>,
}

/// Fetches the currently published digest for `repo:tag`.
pub fn latest_tag(repo: &str, tag: &str) -> Result<RemoteTag, String> {
    let url = format!("{HUB_API}/{repo}/tags/{tag}");
    let response = ureq::get(&url)
        .timeout(TIMEOUT)
        .call()
        .map_err(|err| match err {
            ureq::Error::Status(404, _) => {
                format!("{repo}:{tag} was not found on Docker Hub")
            }
            other => format!("Could not reach Docker Hub: {other}"),
        })?;

    let body: TagResponse = response
        .into_json()
        .map_err(|err| format!("Docker Hub sent a response we could not read: {err}"))?;

    Ok(RemoteTag {
        digest: resolve_digest(&body)?,
    })
}

/// Picks the digest to compare against the local one.
///
/// Multi-architecture tags carry a top-level manifest-list digest, which is what
/// Docker records locally. Single-architecture tags may omit it, so the first
/// per-image digest is used instead.
fn resolve_digest(body: &TagResponse) -> Result<String, String> {
    if let Some(digest) = body.digest.as_ref().filter(|value| !value.is_empty()) {
        return Ok(digest.clone());
    }
    body.images
        .as_ref()
        .and_then(|images| images.iter().find_map(|image| image.digest.clone()))
        .filter(|digest| !digest.is_empty())
        .ok_or_else(|| "Docker Hub did not report a digest for this tag".to_string())
}

/// Shortens a digest for display, turning `sha256:1a2b3c...` into `1a2b3c4`.
pub fn short_digest(digest: &str) -> String {
    let hex = digest.split_once(':').map_or(digest, |(_, hex)| hex);
    hex.chars().take(7).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(digest: Option<&str>, image_digests: &[&str]) -> TagResponse {
        TagResponse {
            digest: digest.map(str::to_string),
            images: Some(
                image_digests
                    .iter()
                    .map(|value| TagImage {
                        digest: Some((*value).to_string()),
                    })
                    .collect(),
            ),
        }
    }

    #[test]
    fn prefers_the_manifest_list_digest() {
        let resolved = resolve_digest(&body(Some("sha256:list"), &["sha256:amd64"]));
        assert_eq!(resolved.unwrap(), "sha256:list");
    }

    #[test]
    fn falls_back_to_a_per_image_digest() {
        let resolved = resolve_digest(&body(None, &["sha256:amd64"]));
        assert_eq!(resolved.unwrap(), "sha256:amd64");
    }

    #[test]
    fn treats_an_empty_digest_as_absent() {
        let resolved = resolve_digest(&body(Some(""), &["sha256:amd64"]));
        assert_eq!(resolved.unwrap(), "sha256:amd64");
    }

    #[test]
    fn errors_when_no_digest_is_reported() {
        assert!(resolve_digest(&body(None, &[])).is_err());
    }

    #[test]
    fn shortens_digests_for_display() {
        assert_eq!(short_digest("sha256:1a2b3c4d5e6f"), "1a2b3c4");
        assert_eq!(short_digest("1a2b3c4d5e6f"), "1a2b3c4");
    }
}
