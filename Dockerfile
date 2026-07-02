# den-reel — Rust binary + yt-dlp + ffmpeg, in one slim image.
#
# Three stages: build the Rust binary, fetch the extractor tools (deno + yt-dlp) with curl/unzip in
# a throwaway stage, then assemble a runtime image that carries neither the Rust toolchain nor
# curl/unzip — just ffmpeg, ca-certs, the two extractor binaries, and our ~2 MB binary. No Node, no
# npm, no python3 (yt-dlp's standalone build bundles its own interpreter). Builds amd64 and arm64.

# ---- build ----------------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /src
# Cache deps: build against manifests + a dummy main first, so a code-only change re-runs only the
# final (LTO'd) link of our crate, not the whole dependency compile.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release --locked && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release --locked   # `strip = true` in the release profile

# ---- fetch extractor tools (curl/unzip stay OUT of the runtime image) ------
FROM debian:bookworm-slim AS tools
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends curl unzip ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# JS runtime for yt-dlp. Recent yt-dlp REQUIRES one to solve YouTube's signature/nsig challenge —
# without it extraction is deprecated, formats go missing, and playback fails intermittently. deno
# is the one yt-dlp enables by default.
ARG DENO_VERSION=2.1.4
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) arch=x86_64-unknown-linux-gnu ;; \
      arm64) arch=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/denoland/deno/releases/download/v${DENO_VERSION}/deno-${arch}.zip" \
      -o /tmp/deno.zip; \
    unzip -q -d /usr/local/bin /tmp/deno.zip

# Pinned yt-dlp STANDALONE binary (PyInstaller onefile — bundles Python, so no system python3
# needed). Bump YTDLP_VERSION to update (YouTube changes often — that's the whole maintenance
# burden, and it's yt-dlp's, not ours).
ARG YTDLP_VERSION=2026.06.09
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) asset=yt-dlp_linux ;; \
      arm64) asset=yt-dlp_linux_aarch64 ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/${asset}" \
      -o /usr/local/bin/yt-dlp; \
    chmod +x /usr/local/bin/yt-dlp

# ---- runtime --------------------------------------------------------------
FROM debian:bookworm-slim

# ffmpeg (mux/faststart) + ca-certificates (TLS roots). curl/unzip were build-only, so they're gone.
RUN apt-get update && apt-get install -y --no-install-recommends ffmpeg ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=tools /usr/local/bin/deno /usr/local/bin/deno
COPY --from=tools /usr/local/bin/yt-dlp /usr/local/bin/yt-dlp
COPY --from=build /src/target/release/den-reel /usr/local/bin/den-reel

WORKDIR /app
ENV PORT=8092 \
    CACHE_DIR=/cache \
    YTDLP_PATH=/usr/local/bin/yt-dlp \
    MAX_HEIGHT=1080
VOLUME ["/cache"]
EXPOSE 8092

# The binary self-checks (no curl needed on the health path). start-period covers cold startup.
HEALTHCHECK --interval=30s --timeout=5s --start-period=5s CMD ["den-reel", "healthcheck"]
CMD ["den-reel"]
