# den trailer-service — Node + yt-dlp + ffmpeg.
FROM node:22-bookworm-slim

# ffmpeg (mux/faststart) + python3 (yt-dlp runtime) + ca-certs + unzip (for deno).
RUN apt-get update && apt-get install -y --no-install-recommends \
      ffmpeg python3 ca-certificates curl unzip \
    && rm -rf /var/lib/apt/lists/*

# JS runtime for yt-dlp. Recent yt-dlp REQUIRES one to solve YouTube's signature/nsig
# challenge — without it extraction is deprecated, formats go missing, and playback fails
# intermittently. deno is the one yt-dlp enables by default; just needs to be on PATH.
ARG DENO_VERSION=2.1.4
RUN curl -fsSL "https://github.com/denoland/deno/releases/download/v${DENO_VERSION}/deno-x86_64-unknown-linux-gnu.zip" \
      -o /tmp/deno.zip && unzip -q -d /usr/local/bin /tmp/deno.zip && rm /tmp/deno.zip

# Pinned yt-dlp standalone binary. Bump YTDLP_VERSION to update (YouTube changes often —
# keep this current; that's the whole maintenance burden, and it's yt-dlp's, not ours).
ARG YTDLP_VERSION=2026.06.09
RUN curl -fsSL "https://github.com/yt-dlp/yt-dlp/releases/download/${YTDLP_VERSION}/yt-dlp" \
      -o /usr/local/bin/yt-dlp && chmod +x /usr/local/bin/yt-dlp

WORKDIR /app
COPY package.json server.js ./

ENV PORT=8092 \
    CACHE_DIR=/cache \
    YTDLP_PATH=/usr/local/bin/yt-dlp \
    MAX_HEIGHT=720
VOLUME ["/cache"]
EXPOSE 8092

HEALTHCHECK --interval=30s --timeout=5s CMD curl -fsS http://localhost:8092/health || exit 1
CMD ["node", "server.js"]
