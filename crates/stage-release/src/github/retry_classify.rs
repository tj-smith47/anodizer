//! Tests for [`super::retry_call::classify_retriability`], the one rule every
//! GitHub-backend retry site applies to an `octocrab::Error`: a transport
//! failure retries with status `0`, and a status-bearing `GitHub` error
//! retries exactly when `status_is_retriable` says so.
//!
//! octocrab's error builders are private, so every error here comes out of a
//! real client: a connect to the refusing address for the transport arm, and
//! a scripted response for the `GitHub` arm.

#[cfg(test)]
mod tests {
    use crate::github::retry_call::classify_retriability;
    use crate::test_support::{build_test_octocrab, synth_github_error};
    use anodizer_core::retry::status_is_retriable;

    #[tokio::test]
    async fn a_transport_error_is_retriable_with_no_status() {
        let octo = build_test_octocrab(anodizer_core::test_helpers::refusing_addr::refusing_addr());
        let err = octo
            .get::<serde_json::Value, _, ()>("/", None::<&()>)
            .await
            .expect_err("a refused connect must fail");
        assert!(
            crate::github::is_octocrab_transport_error(&err),
            "expected a transport-class octocrab error, got: {err:?}"
        );
        assert_eq!(classify_retriability(&err), (0, true));
    }

    #[tokio::test]
    async fn a_github_error_carries_its_status_and_the_status_rule() {
        let body = r#"{"message":"synthetic","documentation_url":"https://example/synthetic"}"#;
        for (status, retriable) in [(500, true), (429, true), (422, false)] {
            let err = synth_github_error(status, body).await;
            assert!(
                matches!(err, octocrab::Error::GitHub { .. }),
                "status {status} must surface as Error::GitHub, got: {err:?}"
            );
            assert_eq!(status_is_retriable(status), retriable);
            assert_eq!(classify_retriability(&err), (status, retriable));
        }
    }
}
