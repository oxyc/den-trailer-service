//! ADDON request path: resolve (and cache) the first PLAYABLE trailer for an imdb id, then build
//! the Fusion `meta` payload whose play URL points back at THIS host.

use std::collections::HashSet;
use std::sync::Arc;

use hyper::header::HeaderMap;
use hyper::{Response, StatusCode};
use serde_json::{json, Value};

use crate::httputil::{self, query_param, Body};
use crate::state::{AppState, YtEntry};
use crate::{MAX_PROBE, YT_NEG_TTL_MS, YT_TTL_MS};

pub fn manifest() -> Value {
    json!({
        "id": "fi.oxy.den-reel",
        "version": "0.2.0",
        "name": "Den Reel",
        "description": "Direct-URL trailers (TMDB/KinoCheck → yt-dlp service) for inline playback.",
        "resources": ["meta"],
        "types": ["movie", "series"],
        "idPrefixes": ["tt", "tmdb:"],
        "catalogs": [],
    })
}

fn is_imdb(id: &str) -> bool {
    id.strip_prefix("tt").map_or(false, |d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// `^[a-z]{2}$` (case-insensitive), else the caller falls back to "en".
fn valid_lang(l: &str) -> bool {
    l.len() == 2 && l.bytes().all(|b| b.is_ascii_alphabetic())
}

/// The base URL this server is reachable at (for building play URLs the device will fetch).
fn self_base(cfg_public: Option<&str>, headers: &HeaderMap, port: u16) -> String {
    if let Some(b) = cfg_public {
        return b.trim_end_matches('/').to_string();
    }
    let hdr = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let proto = hdr("x-forwarded-proto")
        .map(|p| p.split(',').next().unwrap_or("http").trim().to_string())
        .unwrap_or_else(|| "http".to_string());
    let host = hdr("x-forwarded-host")
        .or_else(|| hdr("host"))
        .map(|h| h.to_string())
        .unwrap_or_else(|| format!("localhost:{port}"));
    format!("{proto}://{host}")
}

/// Build the Fusion `meta` payload for a resolved (or missing) trailer.
pub fn build_meta(ty: &str, imdb: &str, base: &str, yt_id: &str) -> Value {
    if yt_id.is_empty() {
        return json!({ "meta": { "id": imdb, "type": ty, "links": [] } });
    }
    let trailers = format!("{}/play/{yt_id}.mp4", base.trim_end_matches('/'));
    json!({
        "meta": {
            "id": imdb,
            "type": ty,
            "links": [{
                "name": "Trailer",
                "category": "Trailer",
                "trailers": trailers,
                "provider": "Den Reel",
            }],
        }
    })
}

/// First candidate yt-dlp can actually extract here, preserving rank order. The common case (top
/// trailer plays) costs ONE probe; only on a miss do we probe the rest concurrently and take the
/// highest-ranked that passes — so a geo-blocked top pick no longer serialises N×3s.
async fn first_playable(state: &Arc<AppState>, candidates: &[String]) -> String {
    if candidates.is_empty() {
        return String::new();
    }
    if (state.prober)(candidates[0].clone()).await {
        return candidates[0].clone();
    }
    let rest = &candidates[1..];
    // Spawn so the probes actually run concurrently (futures are lazy)...
    let handles: Vec<_> = rest
        .iter()
        .map(|c| tokio::spawn((state.prober)(c.clone())))
        .collect();
    for (i, h) in handles.into_iter().enumerate() {
        if h.await.unwrap_or(false) {
            return rest[i].clone(); // ...but await in rank order → highest-ranked winner
        }
    }
    String::new()
}

/// Resolve (and cache) the first PLAYABLE trailer ytId for an imdb id. "" = looked up, nothing
/// playable (cached shorter, in case it's transient).
pub async fn resolve_youtube_id(state: &Arc<AppState>, imdb: &str, ty: &str, lang: &str) -> String {
    let cache_key = format!("{imdb}:{lang}");
    {
        let cache = state.yt_cache.lock().unwrap();
        if let Some(e) = cache.get(&cache_key) {
            if e.exp > (state.clock)() {
                return e.id.clone();
            }
        }
    }
    // TMDB + KinoCheck concurrently (KinoCheck is only a fallback source, but fetching it in
    // parallel costs no extra wall-clock). Official trailer first, KinoCheck appended.
    let (tmdb, kc) = tokio::join!(
        state.upstream.tmdb_candidates(imdb, ty, lang),
        state.upstream.kinocheck_youtube_id(imdb, ty, lang),
    );
    let mut seen = HashSet::new();
    let mut candidates: Vec<String> = Vec::new();
    for c in tmdb.into_iter().chain(kc) {
        if seen.insert(c.clone()) {
            candidates.push(c);
        }
    }
    candidates.truncate(MAX_PROBE);
    let id = first_playable(state, &candidates).await;
    let ttl = if id.is_empty() { YT_NEG_TTL_MS } else { YT_TTL_MS };
    state.yt_cache.lock().unwrap().insert(
        cache_key,
        YtEntry {
            id: id.clone(),
            exp: (state.clock)() + ttl,
        },
    );
    id
}

pub async fn handle_meta(
    state: &Arc<AppState>,
    headers: &HeaderMap,
    ty: &str,
    raw_id: &str,
    query: &str,
) -> Response<Body> {
    let imdb = raw_id.split(':').next().unwrap_or(""); // series may arrive as tt…:S:E — trailers are show-level
    let base = self_base(state.cfg.public_base_url.as_deref(), headers, state.cfg.port);
    // Only imdb ids reach the upstreams (and our URLs) — reject anything else so a crafted id
    // can't be interpolated into a TMDB/KinoCheck request.
    if !is_imdb(imdb) {
        return httputil::json(StatusCode::OK, &build_meta(ty, imdb, &base, ""), &[]);
    }
    let raw_lang = query_param(query, "lang").unwrap_or_else(|| "en".to_string());
    let lang = if valid_lang(&raw_lang) { raw_lang } else { "en".to_string() };
    let yt_id = resolve_youtube_id(state, imdb, ty, &lang).await;
    // Prewarm the download UNLESS the caller opted out (?prewarm=0).
    if !yt_id.is_empty() && query_param(query, "prewarm").as_deref() != Some("0") {
        (state.prewarm)(state.clone(), yt_id.clone());
    }
    let payload = build_meta(ty, imdb, &base, &yt_id);
    // Only cache a SUCCESSFUL resolution (a real trailer). Empty results stay uncached to re-check.
    let has_link = payload["meta"]["links"].as_array().map_or(false, |a| !a.is_empty());
    let extra: &[(&str, &str)] = if has_link {
        &[("cache-control", "public, max-age=604800")]
    } else {
        &[]
    };
    httputil::json(StatusCode::OK, &payload, extra)
}
