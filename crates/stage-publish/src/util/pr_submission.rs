//! What a pull-request transport reports back: the publisher outcome to
//! record and the pull request's URL when one was named.

use anodizer_core::PublisherOutcome;

/// What a pull-request submission came to: the outcome override the caller
/// forwards to `Context::record_publisher_outcome` (`None` is a clean
/// success), and the pull request's URL when the transport reported one —
/// the newly opened PR, or the already-open one `gh` names when it refuses
/// to open a second.
#[derive(Debug, Default, PartialEq, Eq)]
#[must_use = "the outcome override must be forwarded to \
              Context::record_publisher_outcome — dropping it silently \
              misreports a PR-already-exists skip or a PR-creation failure \
              as `succeeded`"]
pub(crate) struct PrSubmission {
    pub outcome: Option<PublisherOutcome>,
    pub url: Option<String>,
}

impl PrSubmission {
    pub(super) fn opened(url: Option<String>) -> Self {
        Self { outcome: None, url }
    }

    pub(super) fn failed(msg: String) -> Self {
        Self {
            outcome: Some(PublisherOutcome::Failed(msg)),
            url: None,
        }
    }
}

/// The last pull-request URL in `gh`'s output: `gh pr create` prints the new
/// PR's URL as its final stdout line, and its "already exists" refusal names
/// the open PR's URL on stderr.
pub(crate) fn pull_request_url_in(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rfind(|l| {
            l.starts_with("https://")
                && l.contains("/pull/")
                && l.rsplit('/')
                    .next()
                    .is_some_and(|n| n.parse::<u64>().is_ok())
        })
        .map(str::to_string)
}
