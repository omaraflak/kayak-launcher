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
    /// ISO-8601 publish time, used to tell the user how new the update is.
    pub published: Option<String>,
}

#[derive(Deserialize)]
struct TagResponse {
    digest: Option<String>,
    last_updated: Option<String>,
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
        published: body.last_updated,
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

/// Renders an ISO-8601 timestamp as a plain date.
///
/// Non-technical users read "2 August 2026" more easily than a digest, and the
/// publish date is the only version-like fact Docker Hub always provides.
pub fn friendly_date(timestamp: &str) -> Option<String> {
    const MONTHS: [&str; 12] = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ];

    let date = timestamp.split('T').next()?;
    let mut parts = date.split('-');
    let year: u32 = parts.next()?.parse().ok()?;
    let month: usize = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let name = MONTHS.get(month.checked_sub(1)?)?;
    Some(format!("{day} {name} {year}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(digest: Option<&str>, image_digests: &[&str]) -> TagResponse {
        TagResponse {
            digest: digest.map(str::to_string),
            last_updated: None,
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

    #[test]
    fn formats_publish_dates() {
        assert_eq!(
            friendly_date("2026-08-02T11:04:22.123456Z").unwrap(),
            "2 August 2026"
        );
    }

    #[test]
    fn rejects_timestamps_it_cannot_parse() {
        assert!(friendly_date("not-a-date").is_none());
        assert!(friendly_date("2026-13-01T00:00:00Z").is_none());
    }
}
