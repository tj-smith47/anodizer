#!/usr/bin/env bash
set -euo pipefail

mode="${1:?expected prepare, preflight, positive, or negative}"
root="$(pwd)"
fixture="$root/.ota/pressure/anodizer-fixture"
evidence="$root/.ota/pressure/evidence"
anodizer="$root/target/debug/anodizer"

case "$mode" in
  prepare)
    test ! -e "$fixture"
    test -x "$anodizer"
    mkdir -p "$fixture/src" "$evidence"
    host="$(rustc -vV | awk '/^host: / {print $2}')"
    test -n "$host"
    printf '%s\n' '[package]' 'name = "ota-anodizer-fixture"' 'version = "0.1.0"' \
      'edition = "2021"' '[[bin]]' 'name = "ota-anodizer-fixture"' \
      'path = "src/main.rs"' > "$fixture/Cargo.toml"
    printf '%s\n' 'fn main() {}' > "$fixture/src/main.rs"
    printf 'crates:\n  - name: ota-anodizer-fixture\n    path: .\n    builds:\n      - id: ota-anodizer-fixture\n        binary: ota-anodizer-fixture\n        targets:\n          - %s\n' "$host" > "$fixture/.anodizer.yaml"
    git -C "$fixture" init -q -b master
    git -C "$fixture" -c user.name='Ota fixture' -c user.email='fixture@invalid.example' \
      -c commit.gpgsign=false add -A
    git -C "$fixture" -c user.name='Ota fixture' -c user.email='fixture@invalid.example' \
      -c commit.gpgsign=false commit -q -m 'disposable fixture'
    git -C "$fixture" rev-parse HEAD > "$evidence/fixture-revision.txt"
    ;;
  preflight)
    test -f "$fixture/.anodizer.yaml"
    (cd "$fixture" && "$anodizer" check config) > "$evidence/preflight.log" 2>&1
    ;;
  positive)
    test -f "$evidence/preflight.log"
    jq --version > "$evidence/jq-version.txt"
    (cd "$fixture" && "$anodizer" check determinism --runs 2 --stages build,archive \
      --report "$evidence/positive.json") > "$evidence/positive.log" 2>&1
    jq -e '.runs == 2 and .drift_count == 0' "$evidence/positive.json" > /dev/null
    ;;
  negative)
    test -f "$evidence/positive.json"
    set +e
    (cd "$fixture" && ANODIZE_TEST_HARNESS=1 "$anodizer" check determinism \
      --runs 2 --stages build,archive --inject-drift archive \
      --report "$evidence/negative.json") > "$evidence/negative.log" 2>&1
    status=$?
    set -e
    printf '%s\n' "$status" > "$evidence/negative-exit.txt"
    test "$status" -eq 1
    jq -e '.runs == 2 and .drift_count > 0 and any(.drift[]; .artifact | endswith(".tar.gz") or endswith(".zip"))' \
      "$evidence/negative.json" > /dev/null
    ;;
  *)
    printf 'unknown fixture mode: %s\n' "$mode" >&2
    exit 2
    ;;
esac
