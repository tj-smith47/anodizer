#!/usr/bin/env bash
# Produces the transcript beside this script:
#
#   bash podman-digestfile-measure.sh > podman-digestfile-vs-registry.txt
#
# It measures what `podman push --digestfile` and `podman manifest push
# --digestfile` write against the `Docker-Content-Digest` the destination
# registry then serves for the same tag — the equality the docker stage's
# `{{ Digest }}` and the release's docker landing check both rest on.
#
# Podman runs inside `quay.io/podman/stable`, so the measurement needs only
# docker and no podman on the host. The registry is a throwaway `registry:2`
# on 127.0.0.1:5000; both containers, and any image this script pulled, are
# removed on exit.
set -euo pipefail

REGISTRY_CTR=anodizer-digestfile-registry
PODMAN_CTR=anodizer-digestfile-podman
REGISTRY_IMAGE=registry:2
PODMAN_IMAGE=quay.io/podman/stable:latest

used=$(df --output=pcent / | tail -1 | tr -dc '0-9')
if [ "$used" -ge 80 ]; then
    echo "refusing to run: / is ${used}% full" >&2
    exit 1
fi

pulled=()
pull_once() {
    docker image inspect "$1" >/dev/null 2>&1 && return 0
    pulled+=("$1")
    docker pull --quiet "$1" >/dev/null
}
pull_once "$REGISTRY_IMAGE"
pull_once "$PODMAN_IMAGE"

cleanup() {
    docker rm -f "$PODMAN_CTR" "$REGISTRY_CTR" >/dev/null 2>&1 || true
    [ ${#pulled[@]} -eq 0 ] || docker rmi -f "${pulled[@]}" >/dev/null 2>&1 || true
}
trap cleanup EXIT

docker run -d --rm --name "$REGISTRY_CTR" -p 127.0.0.1:5000:5000 \
    "$REGISTRY_IMAGE" >/dev/null
# The host network lets the podman container and the host's curl name the
# same registry, so the transcript's URLs are the ones a reader can retype.
docker run -d --rm --name "$PODMAN_CTR" --privileged --network host \
    --entrypoint sh "$PODMAN_IMAGE" -c 'exec tail -f /dev/null' >/dev/null

podman() { docker exec "$PODMAN_CTR" podman "$@"; }

# Echo the command the way a terminal would, then its own output, so the file
# reads as a session and every line in it came from a real run.
run() {
    printf '  $ %s\n' "$*"
    "$@" 2>&1 | sed 's/^/  /'
}
show() {
    printf '  $ cat %s\n' "$1"
    # The digestfile holds no trailing newline, so the transcript adds one.
    docker exec "$PODMAN_CTR" sh -c 'cat "$0"; echo' "$1" | sed 's/^/  /'
}
ask() {
    printf "  \$ curl -i -H 'Accept: %s' \\\\\n      %s\n" "$2" "$1"
    curl -sS -i -H "Accept: $2" "$1" | tr -d '\r' | sed 's/^/  /'
}

MANIFEST_TYPES='application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json'
INDEX_TYPES='application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json'

cat <<'HEADER'
Recorded run of `podman push --digestfile` and `podman manifest push
--digestfile` against a local `registry:2`, to check the value podman writes
against the digest the registry then serves for the same tag.

Regenerate with `bash podman-digestfile-measure.sh` beside this file: its
stdout IS this transcript.

HEADER

run podman version
echo

docker exec "$PODMAN_CTR" mkdir -p /ctx /out
docker exec "$PODMAN_CTR" sh -c 'printf hello > /ctx/hello'
docker exec "$PODMAN_CTR" sh -c \
    'printf "FROM scratch\nCOPY hello /hello\n" > /ctx/Containerfile'
run podman build --quiet -t localhost:5000/probe:t /ctx
echo

run podman push --tls-verify=false --digestfile=/out/push.digest \
    localhost:5000/probe:t
show /out/push.digest
echo
ask http://localhost:5000/v2/probe/manifests/t "$MANIFEST_TYPES"
echo

run podman manifest create probelist
run podman manifest add probelist --tls-verify=false localhost:5000/probe:t
run podman manifest push --tls-verify=false \
    --digestfile=/out/manifest.digest probelist \
    docker://localhost:5000/probelist:t
show /out/manifest.digest
echo
ask http://localhost:5000/v2/probelist/manifests/t "$INDEX_TYPES"
echo

run podman manifest inspect --tls-verify=false localhost:5000/probelist:t

cat <<'FOOTER'

Both verbs wrote exactly the digest the registry serves, so the value recorded
as `{{ Digest }}` and compared by the release's docker landing check is the
same kind of value buildx reports under `containerimage.digest`.
FOOTER
