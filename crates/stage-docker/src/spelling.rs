use anodizer_core::artifact::{Artifact, ArtifactKind};

// ---------------------------------------------------------------------------
// levenshtein_distance
// ---------------------------------------------------------------------------

/// Compute Levenshtein edit distance between two strings.
pub(crate) fn levenshtein_distance(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let a_len = a_chars.len();
    let b_len = b_chars.len();
    if a_len == 0 {
        return b_len;
    }
    if b_len == 0 {
        return a_len;
    }

    let mut prev: Vec<usize> = (0..=b_len).collect();
    let mut curr = vec![0usize; b_len + 1];

    for (i, ca) in a_chars.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b_chars.iter().enumerate() {
            let cost = if ca == cb { 0 } else { 1 };
            curr[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(curr[j] + 1);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b_len]
}

// ---------------------------------------------------------------------------
// find_image_digest
// ---------------------------------------------------------------------------

/// Look up the digest for a docker image tag from the list of artifacts.
///
/// Searches for a `DockerImage` artifact whose `tag` metadata matches the given
/// image reference and returns its `digest` metadata value (e.g.,
/// `sha256:abc123...`).  The digest may be stored as the full
/// `registry/repo@sha256:...` string (from `docker inspect`), so only the
/// `sha256:...` portion is returned when present.
pub(crate) fn find_image_digest(artifacts: &[Artifact], image: &str) -> Option<String> {
    for a in artifacts {
        if a.kind != ArtifactKind::DockerImage && a.kind != ArtifactKind::DockerImageV2 {
            continue;
        }
        let tag = match a.metadata.get("tag") {
            Some(t) => t,
            None => continue,
        };
        if tag != image {
            continue;
        }
        if let Some(digest) = a.metadata.get("digest") {
            if digest.is_empty() {
                return None;
            }
            // docker inspect returns "registry/repo@sha256:abc..." — extract
            // just the "sha256:..." part for use in manifest references.
            if let Some(at_pos) = digest.find('@') {
                return Some(digest[at_pos + 1..].to_string());
            }
            // Already a bare digest (sha256:...)
            return Some(digest.clone());
        }
    }
    None
}
