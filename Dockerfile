# syntax=docker/dockerfile:1.7
# Multi-arch OCI image for the anodizer CLI. ENTRYPOINT runs `anodizer mcp`
# so the image doubles as the Model Context Protocol server registered at
# registry.modelcontextprotocol.io (see `.anodizer.yaml::mcp`).
#
# docker_v2 runs ONE multi-platform buildx build
# (--platform=linux/amd64,linux/arm64) over a single context staged as
# <os>/<arch>/<name>. buildx executes this Dockerfile once per target
# platform with $TARGETOS/$TARGETARCH populated, so the COPY selects each
# platform's binary from its own subdir. A manual `docker build` from a
# flat context must therefore stage the binary under $TARGETOS/$TARGETARCH/
# (or override the COPY source path); BIN only supplies the binary name.
FROM --platform=$TARGETPLATFORM gcr.io/distroless/cc-debian12:nonroot

ARG TARGETOS
ARG TARGETARCH
ARG BIN=anodizer
COPY ${TARGETOS}/${TARGETARCH}/${BIN} /usr/local/bin/anodizer

USER nonroot
ENTRYPOINT ["/usr/local/bin/anodizer"]
CMD ["mcp"]
