#!/usr/bin/env bash
# Guard: no test reaches a live registry.
#
# Contract: every publisher resolves its endpoint from config and falls back to
# a compiled-in default host when the field is unset. A test that drives a
# publish path without setting that field therefore sends a real request to the
# real registry — which happened once, a POST to the Chocolatey community feed
# from a fixture whose `source_repo` was simply absent. The request failed only
# because the fixture's api_key was the literal "dummy".
#
# The rule the tree already follows everywhere else: a test that drives a
# network entry point binds a local responder and points the config at it
# (`format!("http://{addr}/api/v2/package")`). Naming a default host is fine —
# a string-equality assertion on the constant reaches nothing. REACHING one is
# the defect.
#
# This audit fails (exit 1) on either count:
#
#   1. A default-host constant in production code that no entry point in the
#      table below claims. A new publisher's default host is a new way to
#      reach a live registry, so it must be registered here with the function
#      that sends the request.
#   2. A test function that calls one of those entry points without naming a
#      loopback endpoint anywhere in its body.
#
# Test context = a `tests.rs` / `<name>_tests.rs` file, a `crates/*/tests/**`
# integration file, or a `#[cfg(test)]` region inside any other file — the
# shared lib/test-regions.awk definition.
#
# A test whose entry point DERIVES its URL from the host it is handed — so a
# loopback argument would exercise a different branch than the one under test —
# carries a `// live-host-ok: <why>` marker in its body saying what else keeps
# the run offline.
set -euo pipefail

LIB_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/lib"
source "$LIB_DIR/require-bash.sh"
source "$LIB_DIR/scan.sh"
ROOT="${1:-$(git rev-parse --show-toplevel 2>/dev/null || pwd)}"
cd "$ROOT"

# The population, one row per compiled-in default host: the constant, and the
# function that turns it into a request. `-` marks a constant that no request
# path reads — it is rendered into a generated file (an install script's base
# URL, the attribution footer), passed to a signing subprocess, or compared as
# an identity string — so no test can reach a host through it.
#
# Keep the left column exactly in step with the constants the tree declares;
# scan 1 fails when it drifts.
declare -A HOST_CONST_ENTRY=(
    [ANODIZER_BUILDER_ID]='-'
    [ANODIZER_BUILD_TYPE]='-'
    [ANODIZER_URL]='-'
    [API_BASE]='send_linkedin publish_to_gemfury'
    [CLOUDSMITH_API_BASE]='publish_to_cloudsmith'
    [COMMUNITY_PUSH_SOURCE]='publish_to_chocolatey'
    [CURRENT_SCHEMA_URL]='-'
    [DEFAULT_BASE_URL]='-'
    [DEFAULT_GITEA_INSTANCE]='run_gitea_backend'
    [DEFAULT_GITHUB_SERVER_URL]='-'
    [DEFAULT_KREW_RELEASE_BOT_WEBHOOK_URL]='publish_to_krew'
    [DEFAULT_PDS_URL]='send_bluesky'
    [DEFAULT_REGISTRY]='publish_with_registry'
    [DEFAULT_REGISTRY_URL]='publish_to_mcp'
    [DEFAULT_REPOSITORY]='publish_to_pypi'
    [DEFAULT_TIMESTAMP_URL]='-'
    [GITHUB_OIDC_ISSUER]='-'
    [GRAPHQL_URL]='do_mutation'
    [MINT_URL]='mint_trusted_publishing_token revoke_trusted_publishing_token'
    [PUSH_BASE]='publish_to_gemfury'
    [REDDIT_OAUTH_BASE]='send_reddit'
    [REDDIT_TOKEN_BASE]='send_reddit'
    [SNAP_INFO_BASE]='snap_version_in_channel_map'
    [TELEGRAM_API_BASE]='send_telegram'
    [TWITTER_TWEETS_URL]='send_twitter'
)

# The GitHub API has no constant of its own — octocrab compiles its host in —
# so its entry points are named directly.
EXTRA_ENTRIES='close_milestones delete_version publish_to_artifactory publish_to_cargo run_github_backend version_already_published'

# Scan 1: every non-loopback absolute-URL constant declared in production must
# have a row above. The URL is read off the CODE half, so a URL inside a
# comment is prose rather than a declaration.
collect_files CONST_FILES -rlE --include='*.rs' --exclude-dir=target \
    -- 'const[[:space:]]+[A-Z][A-Z0-9_]*[[:space:]]*:[[:space:]]*&' crates
run_scanner declared_names -f "$LIB_DIR/rust-lex.awk" -f "$LIB_DIR/test-regions.awk" -f - \
    "${CONST_FILES[@]}" <<'AWK'
    FNR == 1 { whole_file_is_test = is_test_file(FILENAME) }
    {
        if (whole_file_is_test || in_test_region) next
        code = strip_code($0)
        if (code !~ /const[ \t]+[A-Z][A-Z0-9_]*[ \t]*:[ \t]*&/) next
        # The literal is elided from the code half, so read the host from the
        # raw line: only a declaration reaches here.
        if ($0 !~ /=[ \t]*"https?:\/\//) next
        if ($0 ~ /"https?:\/\/(localhost|127\.0\.0\.1|\[::1\]|0\.0\.0\.0)/) next
        if ($0 ~ /"https?:\/\/(example\.(com|org)|[A-Za-z0-9.-]*\.invalid)/) next
        # Schema and predicate identifiers name a vocabulary, not a service.
        if ($0 ~ /"https?:\/\/(in-toto\.io|slsa\.dev|schemas\.microsoft\.com|www\.w3\.org|json-schema\.org)/) next
        name = $0
        sub(/^.*const[ \t]+/, "", name)
        sub(/[ \t]*:.*$/, "", name)
        print name
    }
AWK

unregistered=""
while IFS= read -r name; do
    [[ -z "$name" ]] && continue
    [[ -v HOST_CONST_ENTRY["$name"] ]] || unregistered+="  $name"$'\n'
done < <(printf '%s\n' "$declared_names" | sort -u)

if [[ -n "$unregistered" ]]; then
    echo "UNREGISTERED DEFAULT HOST — a test could reach it and nothing would notice."
    echo
    printf '%s' "$unregistered"
    echo
    echo "Each constant above compiles a live host into the binary, and no row in"
    echo "audit-test-live-host.sh's HOST_CONST_ENTRY table says which function turns"
    echo "it into a request."
    echo
    echo "Fix: add a row naming that function, so a test driving it is required to"
    echo "bind a local responder first. If no request path reads the constant (it is"
    echo "rendered into a generated file, handed to a subprocess, or compared as an"
    echo "identity string), register it as '-'."
    exit 1
fi

# Scan 2: a test function that calls a registered entry point must name a
# loopback endpoint somewhere in its body.
entries=""
for value in "${HOST_CONST_ENTRY[@]}"; do
    [[ "$value" == "-" ]] && continue
    entries+=" $value"
done
entries+=" $EXTRA_ENTRIES"
ENTRY_RE="$(printf '%s\n' $entries | sort -u | paste -sd'|' -)"

collect_files FILES -rlE --include='*.rs' --exclude-dir=target \
    -- "\\b($ENTRY_RE)[[:space:]]*\\(" crates

if [[ ${#FILES[@]} -eq 0 ]]; then
    echo "audit-test-live-host: no network entry-point call sites found."
    exit 0
fi

# One function body at a time: a `fn` in test context buffers its lines until
# its brace block closes. Depth is counted on `strip_code` output so a brace
# inside a string literal cannot close a body early.
#
# EVERY test-context `fn` is buffered, not only the `#[test]` ones, because the
# tree's idiom is a fixture helper that builds the config (`publish_ctx`,
# `owning_ctx`, `pr_direct_crate`). The endpoint therefore belongs in the
# helper — fixing it once proves every test that drives it — so a test body is
# judged together with the bodies of the same-file helpers it calls.
run_scanner violations -v ENTRY_RE="$ENTRY_RE" \
    -f "$LIB_DIR/rust-lex.awk" -f "$LIB_DIR/test-regions.awk" -f - "${FILES[@]}" <<'AWK'
    FNR == 1 {
        reset_lex()
        whole_file_is_test = is_test_file(FILENAME)
        fn_open = 0; fn_depth = 0; fn_body = ""; fn_name = ""; fn_line = 0
        fn_cmt = ""; is_test_attr = 0; fn_is_test = 0; n_tests = 0
        delete body_of; delete cmt_of; delete test_name; delete test_line
    }

    {
        line = $0
        code = strip_code(line)
        in_test = (whole_file_is_test || in_test_region)
        # The attribute survives the attribute/comment run between it and the
        # `fn` line.
        if (code ~ /^[ \t]*#\[(test|tokio::test)/) is_test_attr = 1

        if (!fn_open && in_test && code ~ /(^|[^A-Za-z0-9_])fn[ \t]+[A-Za-z_][A-Za-z0-9_]*[ \t]*[(<]/) {
            fn_name = code
            sub(/^.*[^A-Za-z0-9_]fn[ \t]+/, "", fn_name)
            sub(/^fn[ \t]+/, "", fn_name)
            sub(/[^A-Za-z0-9_].*$/, "", fn_name)
            fn_open = 1; fn_depth = 0; fn_body = ""; fn_line = FNR
            fn_is_test = is_test_attr
        }

        if (fn_open) {
            fn_body = fn_body "\n" line
            fn_cmt = fn_cmt "\n" comment_part(line)
            fn_depth += count_char(code, "{") - count_char(code, "}")
            if (fn_depth <= 0 && fn_body ~ /\{/) {
                body_of[fn_name] = fn_body
                cmt_of[fn_name] = fn_cmt
                if (fn_is_test) {
                    n_tests++
                    test_name[n_tests] = fn_name
                    test_line[n_tests] = fn_line
                }
                fn_open = 0; fn_body = ""; fn_cmt = ""
                is_test_attr = 0; fn_is_test = 0
            }
        }
    }

    ENDFILE {
        for (i = 1; i <= n_tests; i++) {
            body = body_of[test_name[i]]
            if (body !~ ("[^A-Za-z0-9_](" ENTRY_RE ")[ \t]*\\(")) continue
            # A body that cannot name a local endpoint at all — the entry point
            # derives its URL from the host it is given, so a loopback argument
            # would test a different code path — carries the reason it stays
            # offline instead.
            if (cmt_of[test_name[i]] ~ /live-host-ok:/) continue
            if (has_local_endpoint(with_helpers(body))) continue
            printf("%s:%d: %s\n", FILENAME, test_line[i], test_name[i])
        }
    }

    # One hop: the body plus the body of every same-file function it calls. A
    # deeper walk would let an unrelated helper three levels down excuse a
    # reach the test itself never bounded.
    function with_helpers(body,   helper, combined) {
        combined = body
        for (helper in body_of) {
            if (helper == "") continue
            if (body ~ ("[^A-Za-z0-9_]" helper "[ \t]*\\("))
                combined = combined "\n" body_of[helper]
        }
        return combined
    }

    # A body proves it stays local by naming a loopback literal, by binding an
    # ephemeral port, or by driving one of the shared local responders whose
    # address it then formats into the config under test.
    #
    # A reserved name proves it too: `.invalid` and `example.com`/`example.org`
    # are guaranteed never to resolve to a real service (RFC 2606).
    #
    # Putting the run in dry-run or snapshot mode also proves it: those modes
    # report the request they WOULD make and never open a socket. A live-mode
    # test has no such proof and must name its endpoint.
    function has_local_endpoint(body) {
        return body ~ /127\.0\.0\.1/ ||
               body ~ /\[::1\]/ ||
               body ~ /localhost/ ||
               body ~ /0\.0\.0\.0/ ||
               body ~ /\{addr\}/ ||
               body ~ /local_addr\(/ ||
               body ~ /responder/ ||
               body ~ /\.invalid/ ||
               body ~ /example\.(com|org)/ ||
               body ~ /dry_run/ ||
               body ~ /dry-run/ ||
               body ~ /DryRun/ ||
               body ~ /snapshot/
    }
AWK

if [[ -n "$violations" ]]; then
    echo "TEST REACHES A LIVE REGISTRY — the default host is one absent config field away."
    echo
    echo "$violations"
    echo
    echo "Each function above drives a network entry point without naming a loopback"
    echo "endpoint. When the config it builds leaves the endpoint field unset, the"
    echo "publisher falls back to the compiled-in default and the test sends a real"
    echo "request to the real registry."
    echo
    echo "Fix: bind a local responder and point the config's endpoint field at it —"
    echo "  let (addr, calls) = spawn_oneshot_http_responder(vec![..]);"
    echo "  cfg.source_repo = Some(format!(\"http://{addr}/api/v2/package\"));"
    echo "A test that must not send anything at all still sets the field, so a later"
    echo "edit to the short-circuit it relies on cannot silently reach the feed."
    exit 1
fi

echo "audit-test-live-host: ${#HOST_CONST_ENTRY[@]} default hosts registered; every test driving a network entry point binds a local endpoint."
