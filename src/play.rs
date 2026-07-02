//! PLAYBACK request path: ytId → cached faststart MP4 (yt-dlp + ffmpeg), served with HTTP range
//! support. A cached file is served instantly; a cold id downloads to completion first (prewarm at
//! /meta keeps the cache warm ahead of play, so cold is the exception).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use futures_util::{FutureExt, TryStreamExt};
use hyper::body::Frame;
use hyper::header::HeaderMap;
use hyper::{Response, StatusCode};
use http_body_util::{BodyExt, StreamBody};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

use crate::config::Config;
use crate::httputil::{self, parse_range, Body, RangeReq};
use crate::state::{AppState, BoxFuture, SharedDownload};
use crate::ytdlp::{self, PlayError};

fn cache_path(cfg: &Config, vid: &str) -> PathBuf {
    cfg.cache_dir.join(format!("{vid}.mp4"))
}

/// Bump a cached file's atime so the LRU eviction sees it as recently used (best-effort).
fn touch_atime(fp: PathBuf) {
    tokio::task::spawn_blocking(move || {
        if let Ok(f) = std::fs::File::open(&fp) {
            let times = std::fs::FileTimes::new().set_accessed(SystemTime::now());
            let _ = f.set_times(times);
        }
    });
}

/// Evict least-recently-used cached files until under the size cap (bounded cache). Sync fs, run
/// off the runtime thread via spawn_blocking.
fn evict_if_needed(cfg: &Config) {
    let mut files: Vec<(PathBuf, u64, SystemTime)> = match std::fs::read_dir(&cfg.cache_dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.path().extension().map_or(false, |x| x == "mp4"))
            .filter_map(|e| {
                let md = e.metadata().ok()?;
                let atime = md.accessed().unwrap_or(SystemTime::UNIX_EPOCH);
                Some((e.path(), md.len(), atime))
            })
            .collect(),
        Err(_) => return,
    };
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    if total <= cfg.cache_max_bytes {
        return;
    }
    files.sort_by_key(|f| f.2); // oldest atime first
    for (p, size, _) in &files {
        if total <= cfg.cache_max_bytes {
            break;
        }
        if std::fs::remove_file(p).is_ok() {
            total -= size;
        }
    }
}

/// Download+mux a faststart MP4 for `vid`, cached. De-dupes concurrent requests via `in_flight`.
pub async fn fetch_trailer(state: Arc<AppState>, vid: String) -> Result<PathBuf, PlayError> {
    let fp = cache_path(&state.cfg, &vid);
    if let Ok(md) = tokio::fs::metadata(&fp).await {
        if md.len() > 0 {
            touch_atime(fp.clone()); // bump atime for LRU
            return Ok(fp);
        }
    }

    // One shared download per vid: the first caller creates it, everyone else awaits the same future.
    let shared: SharedDownload = {
        let mut map = state.in_flight.lock().unwrap();
        if let Some(existing) = map.get(&vid) {
            existing.clone()
        } else {
            let st = state.clone();
            let v = vid.clone();
            let fut: BoxFuture<Result<PathBuf, PlayError>> =
                Box::pin(async move { download_cached(st, v).await });
            let shared = fut.shared();
            map.insert(vid.clone(), shared.clone());
            shared
        }
    };
    let result = shared.await;
    state.in_flight.lock().unwrap().remove(&vid); // idempotent: whichever waiter finishes clears it
    result
}

/// The actual yt-dlp download for a cold `vid`: mux to a temp file, atomically rename into place,
/// then evict if we blew the cap.
async fn download_cached(state: Arc<AppState>, vid: String) -> Result<PathBuf, PlayError> {
    let fp = cache_path(&state.cfg, &vid);
    // Temp MUST end in .mp4 — yt-dlp derives the merge output name from the extension.
    let tmp = state
        .cfg
        .cache_dir
        .join(format!(".{vid}.{}.partial.mp4", std::process::id()));

    if let Err(e) = ytdlp::download_to(&state.cfg, &vid, &tmp).await {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, &fp).map_err(|e| PlayError {
        status: 502,
        reason: "extraction_failed".into(),
        message: "Could not fetch this trailer.".into(),
        detail: format!("rename {}: {e}", tmp.display()),
    })?;
    let cfg = state.cfg.clone();
    let _ = tokio::task::spawn_blocking(move || evict_if_needed(&cfg)).await;
    Ok(fp)
}

/// Wrap an async reader as a streaming response body.
fn stream_body<R>(reader: R) -> Body
where
    R: tokio::io::AsyncRead + Send + Sync + 'static,
{
    let stream = ReaderStream::new(reader).map_ok(Frame::data);
    StreamBody::new(stream).boxed()
}

/// Serve a file with HTTP range support (so the player can scrub). Always answers Content-Length +
/// Accept-Ranges (+206 on Range) — which tvOS AVPlayer REQUIRES for a progressive MP4.
async fn serve_file(range: Option<&str>, fp: &Path) -> Response<Body> {
    let file = match tokio::fs::File::open(fp).await {
        Ok(f) => f,
        Err(e) => {
            eprintln!("serve_file open {}: {e}", fp.display());
            return httputil::text(StatusCode::INTERNAL_SERVER_ERROR, "open failed");
        }
    };
    let size = match file.metadata().await {
        Ok(m) => m.len(),
        Err(_) => return httputil::text(StatusCode::INTERNAL_SERVER_ERROR, "stat failed"),
    };

    match parse_range(range, size) {
        Some(RangeReq::Unsatisfiable) => Response::builder()
            .status(StatusCode::RANGE_NOT_SATISFIABLE)
            .header("content-range", format!("bytes */{size}"))
            .body(httputil::full(""))
            .unwrap(),
        Some(RangeReq::Satisfiable { start, end }) => {
            let len = end - start + 1;
            let mut file = file;
            if file.seek(std::io::SeekFrom::Start(start)).await.is_err() {
                return httputil::text(StatusCode::INTERNAL_SERVER_ERROR, "seek failed");
            }
            Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header("content-range", format!("bytes {start}-{end}/{size}"))
                .header("accept-ranges", "bytes")
                .header("content-length", len)
                .header("content-type", "video/mp4")
                .body(stream_body(file.take(len)))
                .unwrap()
        }
        None => Response::builder()
            .status(StatusCode::OK)
            .header("content-length", size)
            .header("content-type", "video/mp4")
            .header("accept-ranges", "bytes")
            .body(stream_body(file))
            .unwrap(),
    }
}

/// Typed /play failure body (geo_blocked 451 / restricted 403 / unavailable 404 / 502).
fn play_error(vid: &str, e: &PlayError) -> Response<Body> {
    let body = serde_json::json!({ "error": e.reason, "message": e.message, "id": vid });
    httputil::json(
        StatusCode::from_u16(e.status).unwrap_or(StatusCode::BAD_GATEWAY),
        &body,
        &[],
    )
}

pub async fn handle_play(state: Arc<AppState>, headers: &HeaderMap, vid: String) -> Response<Body> {
    match fetch_trailer(state, vid.clone()).await {
        Ok(fp) => {
            let range = headers.get("range").and_then(|v| v.to_str().ok());
            serve_file(range, &fp).await
        }
        Err(e) => {
            eprintln!("[{vid}] {}", e.detail);
            play_error(&vid, &e)
        }
    }
}
