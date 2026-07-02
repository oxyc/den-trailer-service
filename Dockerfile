# den-reel — Rust binary + yt-dlp + ffmpeg, in one slim image.
#
# Stage 1 builds a small release binary; stage 2 is debian-slim with just the runtime deps the
# extractor needs (ffmpeg, deno, yt-dlp). No Node, no npm, no python3 — the app is a single ~2 MB
# binary and yt-dlp's self-contained build bundles its own interpreter, so the image weight is now
# entirely the extractor toolchain, not a language runtime. Builds for amd64 and arm64.

# ---- build ----------------------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /src
# Cache deps: build against manifests + a dummy main first, so a code-only change skips the (slow,
# LTO'd) dependency rebuild and only relinks our crate.
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && cargo build --release && rm -rf src
COPY src ./src
RUN touch src/main.rs && cargo build --release && strip target/release/den-reel

# ---- runtime --------------------------------------------------------------
FROM debian:bookworm-slim
ARG TARGETARCH

# ffmpeg (mux/faststart) + ca-certs (TLS roots for yt-dlp/deno) + curl/unzip (build-time fetch of
# deno + yt-dlp).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ffmpeg ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*

# JS runtime for yt-dlp. Recent yt-dlp REQUIRES one to solve YouTube's signature/nsig challenge —
# without it extraction is deprecated, formats go missing, and playback fails intermittently. deno
# is the one yt-dlp enables by default; just needs to be on PATH.
ARG DENO_VERSION=2.1.4
RUN set -eux; \
    case "$TARGETARCH" in \
      amd64) arch=x86_64-unknown-linux-gnu ;; \
      arm64) arch=aarch64-unknown-linux-gnu ;; \
      *) echo "unsupported TARGETARCH: $TARGETARCH" >&2; exit 1 ;; \
    esac; \
    curl -fsSL "https://github.com/denoland/deno/releases/download/v${DENO_VERSION}/deno-${arch}.zip" \
      -o /tmp/deno.zip; \
    unzip -q -d /usr/local/bin /tmp/deno.zip; \
    rm /tmp/deno.zip

# Pinned yt-dlp STANDALONE binary (PyInstaller onefile — bundles Python, so no system python3
# needed). Bump YTDLP_VERSION to update (YouTube changes often — keep this current; that's the
# whole maintenance burden, and it's yt-dlp's, not ours).
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

WORKDIR /app
COPY --from=build /src/target/release/den-reel /usr/local/bin/den-reel

ENV PORT=8092 \
    CACHE_DIR=/cache \
    YTDLP_PATH=/usr/local/bin/yt-dlp \
    MAX_HEIGHT=1080
VOLUME ["/cache"]
EXPOSE 8092

# The binary self-checks (no curl needed on the health path).
HEALTHCHECK --interval=30s --timeout=5s CMD ["den-reel", "healthcheck"]
CMD ["den-reel"]
