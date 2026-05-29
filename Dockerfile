# syntax=docker/dockerfile:1.7
# Multi-arch OCI image for the anodizer CLI. ENTRYPOINT runs `anodizer mcp`
# so the image doubles as the Model Context Protocol server registered at
# registry.modelcontextprotocol.io (see `.anodizer.yaml::mcp`).
#
# docker_v2 builds this once per platform (linux/amd64, linux/arm64) using
# the pre-built release binary from the dist tree. BIN is supplied by the
# anodize build pipeline and points at the per-arch binary in dist/.
FROM --platform=$TARGETPLATFORM gcr.io/distroless/cc-debian12:nonroot

ARG BIN=anodizer
COPY ${BIN} /usr/local/bin/anodizer

USER nonroot
ENTRYPOINT ["/usr/local/bin/anodizer"]
CMD ["mcp"]
