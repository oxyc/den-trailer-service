//! ADDON discovery: imdb id → ordered YouTube trailer candidates, via TMDB (primary) and KinoCheck
//! (fallback). Behind a trait so tests can swap in a fake with no network.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::config::Config;

/// The two upstream lookups the resolver needs. A miss/error is always an empty result (never an
/// error to the caller) — same as the Node version's try/catch-to-`[]`.
#[async_trait]
pub trait Upstream: Send + Sync {
    async fn tmdb_candidates(&self, imdb: &str, ty: &str, lang: &str) -> Vec<String>;
    async fn kinocheck_youtube_id(&self, imdb: &str, ty: &str, lang: &str) -> Option<String>;
}

/// Rank a TMDB /videos entry: official trailer first, then trailer, teaser, anything else.
fn rank(v: &Value) -> u8 {
    let ty = v["type"].as_str().unwrap_or("");
    let official = v["official"].as_bool().unwrap_or(false);
    match (ty, official) {
        ("Trailer", true) => 0,
        ("Trailer", _) => 1,
        ("Teaser", _) => 2,
        _ => 3,
    }
}

/// Rank + dedupe a TMDB /videos result into an ordered list of YouTube ids (official trailer first,
/// then trailer, teaser, anything else). Pure — unit-tested.
pub fn pick_trailer_candidates(results: &[Value]) -> Vec<String> {
    let mut yt: Vec<&Value> = results
        .iter()
        .filter(|v| v["site"] == "YouTube" && v["key"].as_str().is_some_and(|k| !k.is_empty()))
        .collect();
    yt.sort_by_key(|v| rank(v)); // stable → preserves TMDB order within a rank, like JS's sort
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for v in yt {
        if let Some(k) = v["key"].as_str() {
            if seen.insert(k.to_string()) {
                out.push(k.to_string());
            }
        }
    }
    out
}

pub struct HttpUpstream {
    cfg: Arc<Config>,
    http: reqwest::Client,
}

impl HttpUpstream {
    pub fn new(cfg: Arc<Config>, http: reqwest::Client) -> HttpUpstream {
        HttpUpstream { cfg, http }
    }

    async fn get_json(&self, url: &str, headers: &[(&str, &str)]) -> Option<Value> {
        let mut req = self.http.get(url);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let res = req.send().await.ok()?;
        if !res.status().is_success() {
            return None;
        }
        res.json::<Value>().await.ok() // null (not throw) on a malformed body, like safeJson()
    }
}

#[async_trait]
impl Upstream for HttpUpstream {
    /// imdb → TMDB id (via /find) → /videos → ordered YouTube trailer candidates ([] on miss).
    async fn tmdb_candidates(&self, imdb: &str, ty: &str, lang: &str) -> Vec<String> {
        let key = match &self.cfg.tmdb_key {
            Some(k) => k,
            None => return Vec::new(),
        };
        let tmdb_type = if ty == "series" { "tv" } else { "movie" };
        let find_url = format!(
            "{}/find/{imdb}?external_source=imdb_id&api_key={key}",
            self.cfg.tmdb_base
        );
        let found = match self.get_json(&find_url, &[]).await {
            Some(v) => v,
            None => return Vec::new(),
        };
        let results = if tmdb_type == "movie" {
            &found["movie_results"]
        } else {
            &found["tv_results"]
        };
        let hit_id = match results.get(0).and_then(|h| h["id"].as_i64()) {
            Some(id) => id,
            None => return Vec::new(),
        };
        let videos_url = format!(
            "{}/{tmdb_type}/{hit_id}/videos?api_key={key}&language={lang}",
            self.cfg.tmdb_base
        );
        let data = match self.get_json(&videos_url, &[]).await {
            Some(v) => v,
            None => return Vec::new(),
        };
        let empty = Vec::new();
        let results = data["results"].as_array().unwrap_or(&empty);
        pick_trailer_candidates(results)
    }

    /// KinoCheck discovery fallback: imdb → official trailer's YouTube id (or None).
    async fn kinocheck_youtube_id(&self, imdb: &str, ty: &str, lang: &str) -> Option<String> {
        let endpoint = if ty == "series" { "shows" } else { "movies" };
        let language = if lang.starts_with("de") { "de" } else { "en" };
        let url = format!(
            "{}/{endpoint}?imdb_id={imdb}&categories=Trailer&language={language}",
            self.cfg.kinocheck_base
        );
        let mut headers: Vec<(&str, &str)> = vec![("Accept", "application/json")];
        if let Some(k) = &self.cfg.kinocheck_key {
            headers.push(("X-Api-Key", k));
            headers.push(("X-Api-Host", "api.kinocheck.com"));
        }
        let data = self.get_json(&url, &headers).await?;
        data["trailer"]["youtube_video_id"].as_str().map(|s| s.to_string())
    }
}
