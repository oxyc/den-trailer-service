//! The yt-dlp/ffmpeg subprocess layer: probe a candidate (fast, no download), download+mux a
//! faststart MP4, and map yt-dlp's stderr to an HTTP status so `/play` can say *why* it failed.

use std::path::Path;
use std::process::Stdio;

use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::config::Config;

/// A typed `/play` failure: an HTTP status + a short machine reason + a user-facing message.
/// Clone-able because the in-flight de-dupe shares one download future across waiters.
#[derive(Clone, Debug)]
pub struct PlayError {
    pub status: u16,
    pub reason: String,
    pub message: String,
    /// Diagnostic detail (last of stderr / spawn error) — logged, never sent to the client.
    pub detail: String,
}

impl PlayError {
    fn spawn(e: std::io::Error) -> PlayError {
        PlayError {
            status: 502,
            reason: "extraction_failed".into(),
            message: "Could not fetch this trailer.".into(),
            detail: format!("spawn yt-dlp: {e}"),
        }
    }
}

/// Map a yt-dlp failure to an HTTP status + short reason (the cause is in stderr; match the common
/// YouTube ones). Anything unrecognized is a blanket 502.
pub fn classify(code: Option<i32>, stderr: &str) -> PlayError {
    let s = stderr.to_lowercase();
    let (status, reason, message) = if s.contains("available in your country")
        || s.contains("available in your location")
        || s.contains("blocked it in your country")
        || s.contains("not available from your location")
    {
        (451, "geo_blocked", "This trailer is not available in your region.")
    } else if s.contains("private video")
        || s.contains("sign in to confirm your age")
        || s.contains("age-restricted")
        || s.contains("members-only")
    {
        (403, "restricted", "This trailer is private or age-restricted.")
    } else if s.contains("video unavailable")
        || s.contains("has been removed")
        || s.contains("no longer available")
        || s.contains("does not exist")
        || s.contains("removed by the uploader")
    {
        (404, "unavailable", "This trailer is no longer available.")
    } else {
        (502, "extraction_failed", "Could not fetch this trailer.")
    };
    let tail: String = stderr.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect();
    PlayError {
        status,
        reason: reason.into(),
        message: message.into(),
        detail: format!("yt-dlp exit {code:?}: {tail}"),
    }
}

fn watch_url(vid: &str) -> String {
    format!("https://www.youtube.com/watch?v={vid}")
}

/// Does yt-dlp think this id is extractable HERE (right region, decodable formats)? Fast:
/// `--simulate`, no download. Any spawn/exec error counts as "not extractable".
pub async fn probe_extractable(cfg: &Config, vid: &str) -> bool {
    let cache = cfg.ytdlp_cache.to_string_lossy().into_owned();
    let status = Command::new(&cfg.ytdlp)
        .args([
            "-q",
            "--simulate",
            "--no-warnings",
            "--cache-dir",
            &cache,
            "-f",
            &cfg.ytdlp_format,
            &watch_url(vid),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
    matches!(status, Ok(s) if s.success())
}

/// Run yt-dlp+ffmpeg to produce a faststart MP4 at `tmp`. Returns `Ok` iff the process exited 0 and
/// wrote a non-empty file; otherwise a classified [`PlayError`]. Caller owns `tmp`'s lifecycle
/// (rename on success, unlink on failure).
pub async fn download_to(cfg: &Config, vid: &str, tmp: &Path) -> Result<(), PlayError> {
    let cache = cfg.ytdlp_cache.to_string_lossy().into_owned();
    let tmp_s = tmp.to_string_lossy().into_owned();
    let mut child = Command::new(&cfg.ytdlp)
        .args([
            "-q",
            "--no-playlist",
            "--no-warnings",
            "--cache-dir",
            &cache, // reuse the nsig/player-JS work the probe already did
            "-N",
            "4", // parallel DASH fragments → faster download
            // AVPlayer hardware-decodable: H.264 (avc1) + AAC (mp4a) — same string the probe validates.
            "-f",
            &cfg.ytdlp_format,
            "--merge-output-format",
            "mp4",
            // faststart during the merge's ffmpeg (one pass), not a separate whole-file rewrite.
            "--postprocessor-args",
            "Merger+ffmpeg:-movflags +faststart",
            "-o",
            &tmp_s,
            &watch_url(vid),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(PlayError::spawn)?;

    // Drain stderr concurrently with wait() so a chatty yt-dlp can't deadlock on a full pipe.
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let drain = tokio::spawn(async move {
        let mut buf = String::new();
        let _ = stderr_pipe.read_to_string(&mut buf).await;
        buf
    });
    let status = child.wait().await.map_err(PlayError::spawn)?;
    let stderr = drain.await.unwrap_or_default();

    let wrote = tokio::fs::metadata(tmp).await.map(|m| m.len() > 0).unwrap_or(false);
    if status.success() && wrote {
        Ok(())
    } else {
        Err(classify(status.code(), &stderr))
    }
}
