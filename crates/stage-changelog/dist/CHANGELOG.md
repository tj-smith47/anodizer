## Changelog

* 4ce2f9040ea9ee0160849ab30f512690969025c7 sweep pre-Session-B vocabulary out of publisher + packaging guides
* c496fe8fd039d7754abff3a5adfebced6d915a52 lift Command::new sites out of cli/commands per module-boundaries.md + update allow-list
* 891c4a9535278e3bda8925b0c8c5d3d2803322f5 wire defaults.env into per-crate env resolution
* 98429c93b12f1a3a32a799d124f986517ffd2e66 close validation gaps in defaults block (format_overrides + deny_unknown_fields)
* 01c7926050434362f5b8861848ff51c220157f52 hoist tokio runtime out of loop to eliminate Option-init dance
* 4f2733016d2c9d780a2d5593193af0c535d02e1b route stage warnings through StageLogger / tracing instead of eprintln
* f4d41c85d81b714f3e4ccdb8801f98d7788ed51a drop residual journal comments + GR-historical vocab in dogfooding matrix
* c49b1550c5ef6230bf2aeba180f4a3971d2f4305 skip default-inherited builds for library crates with no bin target
* 43fa1b4cbe55685b6e842f4e5a6181b7fdeec57e collapse resolve_repo_owner_name to single arg + drop dead Result wrapper
* 13958767306d39f28f4d1c2063cddb8622d80c6f DEC-5 hard-break — drop GR back-compat aliases + deprecation surface
* 08a2abc6b2758982d2c6f7f591e419f9f4bc8a4d rephrase WAVE 6 archive-collision row
* a8103a67690fc0cab9d07a31995798667b9f47a6 WAVE 6 (anodizer) — migrate .anodizer.yaml to final Session B schema with defaults block
* 2232e1194c65425357e9b0e344686d4bf922d517 WAVE 5 mop-up — close 3 critical + 6 important + 3 minor review findings
* 52be5cdf637f9b63d4421534e4377db0c8792771 WAVE 5 follow-up — Notarize deny_unknown + Logins doc fixes
* 206aeb3cbe545d6ca4fcf0e88bbf4b165af240f7 WAVE 5.7 — behavior-toggle field (SCH-26)
* e2a7eb8d817791b0e96547e9ac7c510ecc2bfb51 WAVE 5.6 — alias batch (SCH-5/11/34)
* c203edfd8c759643cae0dd5e3e010b430f8de02e WAVE 5.5 — hard-break legacy-field batch (SCH-4/13/16/21/30, DEC-12/13)
* 1d0228280c87fe11be0f36d3aa3eb452ce5c4ec7 WAVE 5.4 — DRY-merge batch (SCH-8/9)
* 2968fcf22cca995cebc375d75dd7bb15234eb5a8 WAVE 5.3 — field-add batch (SCH-12/17/24)
* 336dee7d5159c62e66d2adc009cc9f6b83cf88f0 WAVE 5.2 — type-constraint batch (SCH-27/31)
* 0188e45098867ac7da12251484b099e5c3d83fe1 WAVE 5.1 — type-coercion batch (SCH-1/3/7/15/25/29)
* 9e01bcfaeb807f9e8f7104e71009d3c7be0e88c3 WAVE 4 follow-up — enforce cask url-template precedence + nested-merge test
* 9163691b8dad1237e34568db168f184dd573f1d0 WAVE 4 — unify HomebrewCaskConfig (collapse top-level + per-crate types)
* 9f7bf4f90ac7bf89e08382aa8dd0cc059e730f4f WAVE 3 mop-up — dispatcher DRY + Linux test fix + docs sync
* 2e13b61074f7c87c6304907ba1222abb68f4058c WAVE 3 follow-up — wire FOLL-1 publisher skip names + parse rejection tests
* 19c4d2ec3dd4e22b488258fc338f578f8629db56 WAVE 3 — cargo publisher rename + flag expansion (DEC-1/10, ITEM-3, FOLL-1)
* 9fa0648345deb6ba373d3f6e21736c31fc6f6158 WAVE 2 mop-up — broader skip suppression + path-mirror cleanup
* 28c3832ae4183f02a6272f3f5ad0afe385dacb00 WAVE 2 follow-up — wire PublishDefaults.cargo (defers rename to WAVE 3)
* 08c8a3623f39dd42cc68d69d448628b1ca50bdc8 WAVE 2 — defaults system foundation (path-mirror inheritance)
* 77501c43aab12fd1b9bf32c1f9a9b80c9b0570b4 WAVE 1.5 mop-up — final vocabulary + helper consolidation
* 488fd625e67d2284426d36dfb44d1081cf985cc4 WAVE 1.5 follow-up — drain remaining log strings + helper migrations
* 5142346f66bee630e98eeef9bb14130004a70212 WAVE 1.5 — vocabulary closure (skip rename + env helper)
* ddaf840ddc3704620d9ef314f52a827f3b0fcd6b WAVE 1 — DEC-6 skip rename + DEC-7 env Vec<String> migration
* 894620033b7df4a8c356eccfc0f87e4588f86483 close out 2026-04 config-gaps Session A INDEX
* 4d1ddd8175a3cc10553b500e5de6f18365e63c5a extract release_body module from lib.rs
* d2a65d22489a918d77c65d889225a9d0af3e9981 split monolith into helpers/process modules
* 96d08db3926ea2bbf612be2c2f4a28d8f79e643e split monolith into formats/entries/file_specs modules
* 3b03710a972ff82c45cf1342f88b18d2cb814b38 extract shared HTTP credential cascade into http_upload module
* 8dcc0b3ab5d0cc7d1aa18492317fc7affdcbf9a0 lift env passthrough + binary-like dedup helpers to core (B8/B10)
* 91c72af83227fceccd919737d9c1056507764828 close L+B-series parity gaps with validation, dedup, and preflight
* 07041581a972a05a052af545bb5c4316b128ae91 close P-series parity gaps across publishers
* cd8319c2e70b6a8408676aef1c420d504924350b plug audit-1 gaps for register_artifact name, dry-run validation, missing-file ratio, duplicate universal output
* 66976f7e39bbf575558d5ae4a7f7b5d028d83c41 AN-series parity sweep across 13 providers
* 72e6a15a193c8a6148560eb6e45ef9a3b81a8cb0 make timestamp_url overridable, lift defaults to consts, refuse macos+macos_native conflict, broaden redact list
* 4a4e5c17d244adc46c15fb66e5ae24d07e9ee36b cache default_sign_cmd, refuse unknown docker_signs.artifacts, lossy file_name fallbacks
* a3615fea5e4728db8a0e4f42b266001a10886166 dedup extra files across crates, refuse split-overwrite, use UploadableFile kind
* 2e411728f3fce86c5cb3f8e0b2ff11ea1c83d113 single-pass repo resolve, log not-found, drop title from Gitea PATCH
* c4628a95285e8705aeb42b7e98a8caad96814a3e tolerate bad regex, surface errors, validate github-native, write release-notes in dry-run
* 009c949771f019b700ffd2849992b61f609c8733 strip audit-tag and session-note comments from publish/blob
* c6db73156c631ed6ea3d113f007e1c5c2b28afc6 render owner/name templates, expose ReleaseURL, validate skip_upload, paragraph-pad release body
* f0f9908d6e8bec64cce4421777e64268218334ba per-publisher second wave (artifactory/upload/dockerhub)
* 189397f640585e24f4df07acef89b2bc17947717 reuse one tokio runtime across all blob upload jobs
* 087ff7278318180774b859abdcdd85834768c6a4 migrate all is_disabled/evaluates_to_true callers to fallible try_* variants
* cb551d19f5aed164e3257cd545927630bc205419 add Session A INDEX.md for 2026-04 config gaps
* 27c58cf968dc59eca8a42ae63b0a24cba9405070 reuse one tokio runtime across all close calls
* d9ce3a59b286872339bc407f72f4f9674b5a12fb tighten artifactory/dockerhub/upload validation
* 2697cef81f5f706b7bf0af1819e12fd7fb1d4b6a migrate stage-notarize + stage-sbom to try_is_disabled
* 00f99576e7cc89f31cbf6bb00803e3568acbe067 propagate template render errors from is_disabled
* 513fc3050e3a765301be12da917b2844af6d16c9 config-first password cascade + installers in release-uploadable
* 13844dae96f0bbcfb76e816919a3f071edddae68 propagate render errors instead of falling back to raw template
* 4ddd45609d72e309fe706c7b0fa4c42630a3a7b7 fail fast on bad provider/cert/key/subreddit/token
* f43ce2f152d31812d97f91c9d6891c38fd8226ea scope default-extra-files glob to crate dir, not CWD
* 95056867dd0a735e000fafb9ef0a5c75dc08d2bb land 2026-04-config-gaps audits + batch 10/1/5 fixes
* a1ece19a059936e00b96ac92a84f3790f50ce8b3 generate per-crate README.md so crates.io renders pages
* 7b550f6b490d0031d629acaa574ac981090f45be bin .exe on windows + per-archive binary name + hard fail on no archives
* d408d75325a46d1a5834c7d98a965913f305b25e surface moderation queue + fail on no-windows-artifact + retry edge 403
* 2433cd6ace35c8bf81cbfe2421e6882048e06fe1 align publisher keying with GoReleaser conventions
* 9eb8e9d0a96818b79fca797bf010a0a26046a2a5 apply workspace version sync when no --crate is given
* 4a1e330eb98a71ea380bd3b4b80c956e93ee7c53 print help and exit 0 when no subcommand is given
* 6f24986373cc893c780d8c8a0f23b3b3644285f3 detect same-version drift on immutable registries #none

