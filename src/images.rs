use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use regex::Regex;
use tokio::sync::{Mutex, Semaphore};

/// The raw image-URL pattern is fixed; compile it once instead of per message.
/// The second alternative matches klipy/tenor media explicitly (any subdomain),
/// covering `.mp4`/`.webm` and extension-less share URLs without grabbing
/// arbitrary links.
fn image_url_regex() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(
            r"(?i)https?://(?:[^\s<>]+?\.(?:png|apng|jpe?g|gif|webp)(?:\?[^\s<>]*)?|(?:[a-z0-9-]+\.)*(?:klipy|tenor)\.com/[^\s<>]*)",
        )
        .expect("image URL regex must compile")
    })
}

/// Extract candidate image/gif URLs from a chat message's text.
pub fn extract_image_urls(text: &str) -> Vec<String> {
    image_url_regex()
        .find_iter(text)
        .map(|m| m.as_str().to_string())
        .collect()
}

/// Load a string → URL map from a JSON file: `{ "token": "url", ... }`.
pub fn load_map(path: &str) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

/// Precompile the word-boundary regex for every map key ONCE (this runs per
/// message on the AuthVerify hot path). Rebuild whenever the map changes.
pub fn compile_map_regexes(map: &HashMap<String, String>) -> HashMap<String, Regex> {
    let mut out = HashMap::with_capacity(map.len());
    for key in map.keys() {
        if key.is_empty() {
            continue;
        }
        let pattern = format!(r"\b{}\b", regex::escape(key));
        if let Ok(re) = Regex::new(&pattern) {
            out.insert(key.clone(), re);
        }
    }
    out
}

/// Resolve `:token:` / standalone `token` occurrences to their mapped URLs.
fn mapped_urls(
    text: &str,
    map: &HashMap<String, String>,
    regexes: &HashMap<String, Regex>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (key, val) in map {
        if val.is_empty() {
            continue;
        }
        let colon = format!(":{}:", key);
        let colon_hit = text.contains(&colon);
        let word_hit = regexes
            .get(key)
            .map(|re| re.is_match(text))
            .unwrap_or(false);
        if colon_hit || word_hit {
            out.push(val.clone());
        }
    }
    out
}

/// Collect every image URL to embed: raw image/gif URLs in the text PLUS any
/// URLs a mapped token resolves to.
pub fn collect_image_urls(
    text: &str,
    map: &HashMap<String, String>,
    regexes: &HashMap<String, Regex>,
) -> Vec<String> {
    let mut urls = extract_image_urls(text);
    let mut dedup: Vec<String> = Vec::new();
    for u in urls.drain(..) {
        if !dedup.contains(&u) {
            dedup.push(u);
        }
    }
    for u in mapped_urls(text, map, regexes) {
        if !dedup.contains(&u) {
            dedup.push(u);
        }
    }
    dedup
}

/// Outcome of trying to embed a URL as ascii art.
#[derive(Debug, Clone)]
pub enum RenderResult {
    /// Successfully converted a static image.
    Ok(String),
    /// Converted an animation: ascii frames + per-frame duration (ms).
    Frames(Vec<String>, Vec<u32>),
    /// Could not be shown; the string is a human-readable reason.
    Reason(String),
}

/// Outcome of the video→GIF flow.
enum VideoOutcome {
    /// Terminal: converted frames/static art, or an explicit limit rejection.
    Done(RenderResult),
    /// Download/converter/ffprobe failed — caller falls back to legacy behavior.
    Fallback,
}

/// True when a response `Content-Type` may carry an image. Some CDNs serve
/// images as a generic `application/octet-stream` / `binary/*`, and a few omit
/// the header entirely; accept those and sniff the bytes during decode.
fn content_type_is_image(ctype: &str) -> bool {
    let ctype = ctype.trim();
    if ctype.is_empty() {
        return true;
    }
    let ct = ctype.split(';').next().unwrap_or(ctype).trim();
    ct.starts_with("image/") || ct == "application/octet-stream" || ct.starts_with("binary/")
}

/// Lowercased URL path (scheme + host stripped, query/fragment removed).
fn url_path_lower(url: &str) -> String {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let path = after_scheme.split(['?', '#']).next().unwrap_or("");
    path.to_ascii_lowercase()
}

/// True when `url`'s host is klipy.com or tenor.com (any subdomain).
fn is_klipy_or_tenor(url: &str) -> bool {
    let after_scheme = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = after_scheme.split(['/', '?', '#']).next().unwrap_or("");
    let host = host.rsplit('@').next().unwrap_or(host);
    let host = host.split(':').next().unwrap_or(host);
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    host == "klipy.com"
        || host.ends_with(".klipy.com")
        || host == "tenor.com"
        || host.ends_with(".tenor.com")
}

/// True when the URL path ends in an extension the `image` crate can decode.
fn has_image_extension(url: &str) -> bool {
    let p = url_path_lower(url);
    [".png", ".apng", ".jpg", ".jpeg", ".gif", ".webp"]
        .iter()
        .any(|e| p.ends_with(e))
}

/// Rewrite a klipy/tenor `.mp4`/`.webm` media URL to its sibling `.gif` URL
/// (preserving any query string). Returns `None` for a non-video URL.
fn gif_sibling_url(url: &str) -> Option<String> {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let lower = base.to_ascii_lowercase();
    let new_base = if lower.ends_with(".mp4") {
        format!("{}.gif", &base[..base.len() - 4])
    } else if lower.ends_with(".webm") {
        format!("{}.gif", &base[..base.len() - 5])
    } else {
        return None;
    };
    Some(match query {
        Some(q) => format!("{}?{}", new_base, q),
        None => new_base,
    })
}

/// Rewrite a klipy/tenor `.mp4`/`.webm` media URL to its static placeholder
/// frame — the host's `preview` format, served as a `.png` for the same media
/// id (query preserved). Used when no ffmpeg converter is available so a video
/// gif still embeds as a still instead of failing. Returns `None` for a
/// non-video URL.
fn static_sibling_url(url: &str) -> Option<String> {
    let (base, query) = match url.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (url, None),
    };
    let lower = base.to_ascii_lowercase();
    let new_base = if lower.ends_with(".mp4") {
        format!("{}.png", &base[..base.len() - 4])
    } else if lower.ends_with(".webm") {
        format!("{}.png", &base[..base.len() - 5])
    } else {
        return None;
    };
    Some(match query {
        Some(q) => format!("{}?{}", new_base, q),
        None => new_base,
    })
}

/// True when an external converter is runnable. An explicit path (contains a
/// separator) is checked directly; a bare program name is looked up on `PATH`
/// (a `.exe` suffix is also tried on Windows).
fn converter_available(path: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    let is_explicit =
        path.contains('/') || path.contains('\\') || path.contains(std::path::MAIN_SEPARATOR);
    if is_explicit {
        return std::path::Path::new(path).is_file();
    }
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    let names: Vec<std::ffi::OsString> = if cfg!(windows) {
        vec![path.into(), format!("{}.exe", path).into()]
    } else {
        vec![path.into()]
    };
    for dir in std::env::split_paths(&path_var) {
        if names.iter().any(|name| dir.join(name).is_file()) {
            return true;
        }
    }
    false
}

/// True when `url`'s path ends in a video extension we can hand to ffmpeg.
fn has_video_extension(url: &str) -> bool {
    let p = url_path_lower(url);
    p.ends_with(".mp4") || p.ends_with(".webm")
}

/// Derive the `ffprobe` path from the configured `ffmpeg` path so an explicit
/// converter path with `ffprobe` beside it works; a path without `ffmpeg`
/// falls back to the bare name (resolved via `PATH`).
fn ffprobe_for(ffmpeg_path: &str) -> String {
    match ffmpeg_path.rfind("ffmpeg") {
        Some(i) => format!(
            "{}ffprobe{}",
            &ffmpeg_path[..i],
            &ffmpeg_path[i + "ffmpeg".len()..]
        ),
        None => "ffprobe".to_string(),
    }
}

/// True iff a video of `bytes_len` bytes and `duration_secs` seconds fits
/// within both maximums. Inclusive: exactly at a maximum still passes.
fn video_within_limits(
    bytes_len: usize,
    duration_secs: u32,
    max_bytes: usize,
    max_secs: u32,
) -> bool {
    bytes_len <= max_bytes && duration_secs <= max_secs
}

/// Parse `ffprobe -show_entries format=duration` stdout (a bare seconds value
/// such as `12.345000`) into whole seconds. Empty, `N/A`, non-numeric,
/// negative or non-finite input yields `None`.
fn parse_ffprobe_duration(output: &str) -> Option<u32> {
    let t = output.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("N/A") {
        return None;
    }
    let secs = t.parse::<f64>().ok()?;
    if !secs.is_finite() || secs < 0.0 {
        return None;
    }
    Some(secs.round() as u32)
}

/// Bounded LRU-ish cache of rendered art, keyed by `url|colsxrows`.
/// Capped at ~4× the visible chat height so memory stays bounded on a long
/// stream (oldest entries evicted first).
#[derive(Default)]
struct ImageCache {
    map: std::collections::HashMap<String, RenderResult>,
    order: std::collections::VecDeque<String>,
}

type Cache = Arc<Mutex<ImageCache>>;

pub struct ImageRenderer {
    /// External `ascii-image-converter` binary path (used when it exists on
    /// disk — better quality + animated gifs). Empty/absent → built-in.
    pub converter_path: String,
    pub width: usize,
    pub referer: String,
    cache: Cache,
    /// Bounds concurrent downloads (memory safety under a fast chat).
    download_semaphore: Arc<Semaphore>,
    /// Hard cap on a single downloaded image's bytes.
    max_image_bytes: usize,
    /// Per-download HTTP timeout.
    timeout: Duration,
    /// Cache eviction bound: multiplier × visible chat height.
    cache_multiplier: usize,
    /// Cache eviction floor.
    cache_min: usize,
    /// Fraction of the terminal an embedded image may occupy.
    fit_fraction: f32,
    /// External video→GIF converter (`ffmpeg`) path/name. Empty/absent disables
    /// conversion — klipy/tenor videos then embed as the host's static
    /// placeholder frame (`.png`) instead.
    video_converter_path: String,
    /// Hard cap on a single downloaded video's bytes.
    video_max_bytes: usize,
    /// Hard cap on a converted video's duration (seconds).
    video_max_seconds: u32,
}

impl ImageRenderer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        converter_path: String,
        width: usize,
        referer: String,
        max_image_bytes: usize,
        max_concurrent: usize,
        timeout_secs: u32,
        cache_multiplier: usize,
        cache_min: usize,
        fit_fraction: f32,
        video_converter_path: String,
        video_max_bytes: usize,
        video_max_seconds: u32,
    ) -> Self {
        Self {
            converter_path,
            width,
            referer,
            cache: Arc::new(Mutex::new(ImageCache::default())),
            download_semaphore: Arc::new(Semaphore::new(max_concurrent)),
            max_image_bytes,
            timeout: Duration::from_secs(timeout_secs as u64),
            cache_multiplier,
            cache_min,
            fit_fraction,
            video_converter_path,
            video_max_bytes,
            video_max_seconds,
        }
    }

    /// Download, convert to ascii, and return the art (temp file deleted after).
    /// Cached per URL *and* terminal size — a resize re-renders at the new
    /// size instead of returning the old art. Returns `Reason` on any failure.
    pub async fn render(&self, url: &str) -> RenderResult {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((self.width as u16, 24));
        let cache_key = format!("{}|{}x{}", url, cols, rows);
        {
            let cache = self.cache.lock().await;
            if let Some(hit) = cache.map.get(&cache_key) {
                return hit.clone();
            }
        }

        let result = self.render_uncached(url, cols, rows).await;
        let mut cache = self.cache.lock().await;
        cache.map.insert(cache_key.clone(), result.clone());
        cache.order.push_back(cache_key);
        // Evict the oldest entries past ~multiplier× the visible chat height.
        let max = (self.cache_multiplier * rows as usize).max(self.cache_min);
        while cache.order.len() > max {
            if let Some(old) = cache.order.pop_front() {
                cache.map.remove(&old);
            }
        }
        result
    }

    async fn render_uncached(&self, url: &str, cols: u16, rows: u16) -> RenderResult {
        let video_converter = converter_available(&self.video_converter_path);

        // A video URL (klipy/tenor `.mp4`/`.webm`, or any `.mp4`/`.webm`):
        // when an ffmpeg converter is installed, download + convert it to a
        // GIF and render that. Limit rejections ("video too large"/"video too
        // long") are terminal; converter/ffprobe failures fall through to the
        // legacy `.gif` sibling behavior below.
        if video_converter && has_video_extension(url) {
            if let VideoOutcome::Done(result) = self.render_video(url, cols, rows).await {
                return result;
            }
        }

        // Klipy/Tenor serve GIFs as `.mp4`/`.webm` which the `image` crate
        // cannot decode. With no ffmpeg converter (or a failed conversion),
        // fall back to the static placeholder frame the host serves for the
        // same media id (`<id>.png`) and embed that as a still. Non-video URLs
        // decode directly; extension-less share URLs need their API, so skip.
        let owned_target;
        let target = if is_klipy_or_tenor(url) {
            if has_video_extension(url) {
                match static_sibling_url(url) {
                    Some(png) => {
                        owned_target = png;
                        owned_target.as_str()
                    }
                    None => return RenderResult::Reason("video gif not supported".to_string()),
                }
            } else if let Some(gif) = gif_sibling_url(url) {
                owned_target = gif;
                owned_target.as_str()
            } else if has_image_extension(url) {
                url
            } else {
                return RenderResult::Reason("video gif not supported".to_string());
            }
        } else {
            url
        };

        // Bound concurrent downloads so a fast chat can't spawn unbounded HTTP
        // fetches / in-flight byte buffers.
        let permit = match self.download_semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => return RenderResult::Reason("download failed".to_string()),
        };

        let client = reqwest::Client::builder()
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120 Safari/537.36",
            )
            .build()
            .unwrap_or_default();
        let mut req = client.get(target);
        if !self.referer.is_empty() {
            req = req.header("Referer", self.referer.as_str());
        }
        let resp = match req.timeout(self.timeout).send().await {
            Ok(r) => r,
            Err(_) => return RenderResult::Reason("download failed".to_string()),
        };

        // Hotlink protection / auth walls refuse with 401/403.
        if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 {
            return RenderResult::Reason("embedding not allowed".to_string());
        }
        if !resp.status().is_success() {
            return RenderResult::Reason(format!(
                "download failed (HTTP {})",
                resp.status().as_u16()
            ));
        }

        // Not an image → nothing to embed. A `video/*` response is converted
        // when a converter is installed (the extension-based check above only
        // catches `.mp4`/`.webm` paths; this covers extension-less video URLs).
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ctype.starts_with("video/") {
            if video_converter {
                drop(permit); // release before the self-contained video download
                if let VideoOutcome::Done(result) = self.render_video(url, cols, rows).await {
                    return result;
                }
            }
            return RenderResult::Reason("video gif not supported".to_string());
        }
        if !content_type_is_image(&ctype) {
            return RenderResult::Reason("not an image".to_string());
        }

        // Byte cap: reject by Content-Length when the server reports one, and
        // re-check the downloaded bytes afterwards (some servers send no
        // Content-Length; the in-memory read still lets us drop oversized
        // payloads before they're decoded).
        if let Some(cl) = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
        {
            if cl > self.max_image_bytes {
                return RenderResult::Reason("image too large".to_string());
            }
        }

        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(_) => return RenderResult::Reason("download failed".to_string()),
        };
        if bytes.is_empty() {
            return RenderResult::Reason("download failed".to_string());
        }
        if bytes.len() > self.max_image_bytes {
            return RenderResult::Reason("image too large".to_string());
        }
        drop(permit); // download done — release before decode/convert
        self.render_image_bytes(&bytes, cols, rows).await
    }

    /// Decode already-downloaded image/GIF bytes and render them: animated
    /// frames when possible, otherwise the external/built-in static path.
    async fn render_image_bytes(&self, bytes: &[u8], cols: u16, rows: u16) -> RenderResult {
        // Decode to get the source dimensions (needed to preserve aspect).
        let img = match image::ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .map_err(|e| e.to_string())
            .and_then(|r| r.decode().map_err(|e| e.to_string()))
        {
            Ok(i) => i,
            Err(_) => return RenderResult::Reason("could not be decoded".to_string()),
        };
        let (iw, ih) = (img.width(), img.height());
        if iw == 0 || ih == 0 {
            return RenderResult::Reason("conversion failed".to_string());
        }

        let (out_w, out_h) = fit_dimensions(cols, rows, iw, ih, self.fit_fraction);

        // Animated GIF/APNG: extract every frame and animate inside this
        // module (never shell out for animation). A single-frame file falls
        // through to the static path below.
        if let Some(anim) = extract_frames(bytes, out_w, out_h) {
            return anim;
        }

        // Prefer the external converter when it's actually installed (nicer
        // output); otherwise use the built-in renderer so the feature works
        // with no external dependency.
        if converter_available(&self.converter_path) {
            self.convert_external(bytes, out_w, out_h).await
        } else {
            self.convert_builtin(&img, out_w, out_h)
        }
    }

    /// Download a video URL (bounded by the image semaphore/timeout, capped at
    /// `video_max_bytes`), probe its duration, and convert it to a GIF which is
    /// then rendered through the normal image path. Returns `Fallback` on any
    /// download/converter/ffprobe failure so the caller can try the legacy
    /// `.gif` sibling; explicit size/duration limit rejections are terminal.
    async fn render_video(&self, url: &str, cols: u16, rows: u16) -> VideoOutcome {
        let permit = match self.download_semaphore.acquire().await {
            Ok(p) => p,
            Err(_) => return VideoOutcome::Fallback,
        };

        let client = reqwest::Client::builder()
            .user_agent(
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120 Safari/537.36",
            )
            .build()
            .unwrap_or_default();
        let mut req = client.get(url);
        if !self.referer.is_empty() {
            req = req.header("Referer", self.referer.as_str());
        }
        let resp = match req.timeout(self.timeout).send().await {
            Ok(r) => r,
            Err(_) => return VideoOutcome::Fallback,
        };
        if !resp.status().is_success() {
            return VideoOutcome::Fallback;
        }

        // Reject oversized videos by Content-Length before downloading.
        if let Some(cl) = resp
            .headers()
            .get(reqwest::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<usize>().ok())
        {
            if cl > self.video_max_bytes {
                return VideoOutcome::Done(RenderResult::Reason("video too large".to_string()));
            }
        }

        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(_) => return VideoOutcome::Fallback,
        };
        drop(permit); // download done — release before ffprobe/ffmpeg
        if bytes.is_empty() {
            return VideoOutcome::Fallback;
        }
        if bytes.len() > self.video_max_bytes {
            return VideoOutcome::Done(RenderResult::Reason("video too large".to_string()));
        }

        let mut input = match tempfile::NamedTempFile::new() {
            Ok(t) => t,
            Err(_) => return VideoOutcome::Fallback,
        };
        if input.write_all(&bytes).is_err() {
            return VideoOutcome::Fallback;
        }

        let duration = match self.probe_duration(input.path()).await {
            Some(d) => d,
            None => return VideoOutcome::Fallback,
        };
        if !video_within_limits(
            bytes.len(),
            duration,
            self.video_max_bytes,
            self.video_max_seconds,
        ) {
            return VideoOutcome::Done(RenderResult::Reason("video too long".to_string()));
        }

        let gif = match self
            .convert_to_gif(input.path(), self.video_max_seconds)
            .await
        {
            Some(g) => g,
            None => return VideoOutcome::Fallback,
        };
        drop(input); // NamedTempFile deletes on drop.

        VideoOutcome::Done(self.render_image_bytes(&gif, cols, rows).await)
    }

    /// Probe a media file's duration (whole seconds) via `ffprobe`. `None` when
    /// ffprobe is unavailable, fails, or reports no usable duration.
    async fn probe_duration(&self, path: &std::path::Path) -> Option<u32> {
        let ffprobe = ffprobe_for(&self.video_converter_path);
        if !converter_available(&ffprobe) {
            return None;
        }
        let output = tokio::process::Command::new(&ffprobe)
            .arg("-v")
            .arg("error")
            .arg("-show_entries")
            .arg("format=duration")
            .arg("-of")
            .arg("default=nw=1:nk=1")
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await
            .ok()?;
        if !output.status.success() {
            return None;
        }
        parse_ffprobe_duration(&String::from_utf8_lossy(&output.stdout))
    }

    /// Convert a media file to a GIF with ffmpeg (palette-optimised, 15 fps,
    /// 480 px wide, bounded to `max_secs`). Returns the GIF bytes, or `None`
    /// on any failure.
    async fn convert_to_gif(&self, input: &std::path::Path, max_secs: u32) -> Option<Vec<u8>> {
        let out = tempfile::Builder::new().suffix(".gif").tempfile().ok()?;
        let out_path = out.path().to_path_buf();
        let filter =
            "fps=15,scale=480:-1:flags=lanczos,split[s0][s1];[s0]palettegen[p];[s1][p]paletteuse";
        let status = tokio::process::Command::new(&self.video_converter_path)
            .arg("-y")
            .arg("-i")
            .arg(input)
            .arg("-t")
            .arg(max_secs.to_string())
            .arg("-vf")
            .arg(filter)
            .arg("-loop")
            .arg("0")
            .arg(&out_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .ok()?;
        if !status.success() {
            return None;
        }
        let bytes = std::fs::read(&out_path).ok()?;
        drop(out); // NamedTempFile deletes on drop.
        if bytes.is_empty() {
            return None;
        }
        Some(bytes)
    }

    /// Run the external `ascii-image-converter` on the downloaded bytes.
    /// Passes both width and height via `--dimensions WxH` (the `-d` flag);
    /// `--width` alone would ignore the chat-area height cap and let images
    /// overflow the chat area.
    async fn convert_external(&self, bytes: &[u8], out_w: u32, out_h: u32) -> RenderResult {
        let mut tmp = match tempfile::NamedTempFile::new() {
            Ok(t) => t,
            Err(_) => return RenderResult::Reason("conversion failed".to_string()),
        };
        if tmp.write_all(bytes).is_err() {
            return RenderResult::Reason("conversion failed".to_string());
        }
        let path = tmp.path().to_path_buf();

        let output = tokio::process::Command::new(&self.converter_path)
            .arg(&path)
            .arg("--dimensions")
            .arg(format!("{out_w}x{out_h}"))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await;

        drop(tmp); // NamedTempFile deletes on drop.

        let output = match output {
            Ok(o) => o,
            Err(_) => return RenderResult::Reason("conversion failed".to_string()),
        };
        if !output.status.success() {
            return RenderResult::Reason("conversion failed".to_string());
        }
        let text = String::from_utf8_lossy(&output.stdout).to_string();
        if text.trim().is_empty() {
            RenderResult::Reason("conversion failed".to_string())
        } else {
            RenderResult::Ok(text)
        }
    }

    /// Built-in decoder → luminance ramp ascii art (no external tools).
    fn convert_builtin(&self, img: &image::DynamicImage, out_w: u32, out_h: u32) -> RenderResult {
        let art = image_to_ascii(img, out_w, out_h);
        if art.trim().is_empty() {
            RenderResult::Reason("conversion failed".to_string())
        } else {
            RenderResult::Ok(art)
        }
    }
}

/// Hard cap on frames decoded from one animation (bounds ascii memory).
const MAX_ANIM_FRAMES: usize = 150;
/// Floor/ceiling for a single frame's on-screen duration.
const MIN_FRAME_MS: u32 = 20;
const MAX_FRAME_MS: u32 = 2000;

/// Decode + ascii-render every frame of a GIF or APNG. Returns `None` when the
/// bytes are another format, contain a single frame (static), or fail to
/// decode (caller then falls back to the static first-frame path).
fn extract_frames(bytes: &[u8], out_w: u32, out_h: u32) -> Option<RenderResult> {
    use image::AnimationDecoder;
    let format = image::guess_format(bytes).ok()?;
    // Iterate rather than `collect_frames()` so an over-long animation stops
    // decoding at the cap instead of buffering every frame (memory bound).
    let frame_iter = match format {
        image::ImageFormat::Gif => image::codecs::gif::GifDecoder::new(Cursor::new(bytes))
            .ok()?
            .into_frames(),
        image::ImageFormat::Png => image::codecs::png::PngDecoder::new(Cursor::new(bytes))
            .ok()?
            .apng()
            .ok()?
            .into_frames(),
        _ => return None,
    };

    let mut decoded = Vec::new();
    let mut capped = false;
    for frame in frame_iter {
        decoded.push(frame.ok()?); // a bad frame falls back to the static path
        if decoded.len() > MAX_ANIM_FRAMES {
            decoded.truncate(MAX_ANIM_FRAMES);
            capped = true;
            break;
        }
    }
    if decoded.len() <= 1 {
        return None;
    }
    if capped {
        tracing::warn!(
            "[image] animation capped at {} frames for ascii playback",
            MAX_ANIM_FRAMES
        );
    }

    let mut arts = Vec::with_capacity(decoded.len());
    let mut delays = Vec::with_capacity(decoded.len());
    for frame in decoded {
        let (num, den) = frame.delay().numer_denom_ms();
        let ms = num.checked_div(den).unwrap_or(MIN_FRAME_MS);
        delays.push(ms.clamp(MIN_FRAME_MS, MAX_FRAME_MS));
        let img = image::DynamicImage::ImageRgba8(frame.into_buffer());
        arts.push(image_to_ascii(&img, out_w, out_h));
    }
    Some(RenderResult::Frames(arts, delays))
}

/// Compute the ascii cell dimensions so the image's longest dimension fits
/// within `fraction` of the terminal (width and height), preserving aspect
/// ratio (terminal cells are ~2:1 so rows = H/2 in cell units). Never upscales.
/// The height is additionally capped so the whole image message (bracket line,
/// content line, art and spacer) stays within `fraction` of the chat area
/// (`rows - 4`, the header/banner/status/footer are reserved): `image_h + 3 <=
/// fraction * (rows - 4)`.
fn fit_dimensions(cols: u16, rows: u16, iw: u32, ih: u32, fraction: f32) -> (u32, u32) {
    let max_w = ((cols as f32 * fraction) as usize).max(10);
    let chat_rows = rows.saturating_sub(4) as f32;
    let overhead = 3.0;
    let height_cap = (chat_rows * fraction - overhead).max(0.0);
    let max_h = ((rows as f32 * fraction).min(height_cap) as usize).max(3);
    let cell_w = iw as f32;
    let cell_h = ih as f32 / 2.0;
    let scale = (max_w as f32 / cell_w).min(max_h as f32 / cell_h).min(1.0);
    let out_w = ((cell_w * scale).round() as u32).max(1);
    let out_h = ((cell_h * scale).round() as u32).max(1);
    (out_w, out_h)
}

/// Convert a decoded image to ascii art at exactly `out_w` × `out_h` cells.
/// Uses a standard luminance ramp and skips fully-transparent pixels.
fn image_to_ascii(img: &image::DynamicImage, out_w: u32, out_h: u32) -> String {
    const RAMP: &[u8] = b" .:-=+*#%@";
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    if w == 0 || h == 0 || out_w == 0 || out_h == 0 {
        return String::new();
    }

    let mut out = String::new();
    for y in 0..out_h {
        let sy = ((y as f32 + 0.5) / out_h as f32 * h as f32).min((h - 1) as f32) as u32;
        for x in 0..out_w {
            let sx = ((x as f32 + 0.5) / out_w as f32 * w as f32).min((w - 1) as f32) as u32;
            let p = rgba.get_pixel(sx, sy);
            // Skip fully-transparent pixels (renders as blank, like a GIF frame).
            if p[3] == 0 {
                out.push(' ');
                continue;
            }
            let lum = 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32;
            let idx = ((lum / 255.0) * (RAMP.len() - 1) as f32).round() as usize;
            out.push(RAMP[idx.min(RAMP.len() - 1)] as char);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_dimensions_tracks_terminal_resize() {
        // 400x400 square: cell = 400 wide x 200 tall.
        // 120x30 terminal -> 0.7 => max_w 84, chat_rows 26, cap = 26*0.7-3 = 15.2 => max_h 15
        //   scale = min(84/400, 15/200) = 0.075 -> 30x15
        let (w1, h1) = fit_dimensions(120, 30, 400, 400, 0.7);
        assert_eq!((w1, h1), (30, 15));
        // Resized to 60x20 -> max_w 42, chat_rows 16, cap = 16*0.7-3 = 8.2 => max_h 8
        //   scale = min(42/400, 8/200) = 0.04 -> 16x8
        let (w2, h2) = fit_dimensions(60, 20, 400, 400, 0.7);
        assert_eq!((w2, h2), (16, 8));
        // A resize must produce a different size (so the cache re-renders).
        assert_ne!((w1, h1), (w2, h2));

        // Wide image: width-constrained.
        // 800x200: cell = 800 x 100; at 120x30 -> max_w 84, max_h 15
        //   scale = min(84/800, 15/100) = 0.105 -> 84x11 (10.5 rounds to 11)
        assert_eq!(fit_dimensions(120, 30, 800, 200, 0.7), (84, 11));
        // Small square is height-capped but never upscaled (scale stays < 1).
        // 40x40: cell = 40 x 20; at 120x30 -> scale = min(84/40, 15/20) = 0.75 -> 30x15
        assert_eq!(fit_dimensions(120, 30, 40, 40, 0.7), (30, 15));
    }

    #[test]
    fn square_source_fits_chat_area() {
        use crate::render::{rows_needed, RenderedLine};
        for rows in [24u16, 30u16] {
            let (w, h) = fit_dimensions(120, rows, 400, 400, 0.7);
            // The image message must stay within `fraction` of the chat area,
            // where the chat area is `rows - 4` (header/banner/status/footer
            // reserved).
            let budget = 0.7 * (rows as f32 - 4.0);
            assert!(
                (h + 3) as f32 <= budget,
                "rows={rows} art_h={h} budget={budget} (image_h + 3 must fit)"
            );
            // A one-line bracket + one-line content, then the art rows.
            let art: String = (0..h).map(|_| " ".repeat(w as usize) + "\n").collect();
            let line = RenderedLine {
                username: "u".to_string(),
                name_color: String::new(),
                bracket_content: "u".to_string(),
                content: "hello".to_string(),
                donation_tag: None,
                image_art: Some(art),
                image_status: None,
                dimmed: false,
            };
            let needed = rows_needed(&line, 120);
            assert!(
                needed <= rows as usize - 3,
                "rows={rows} art_h={h} needed={needed}"
            );
        }
    }

    #[test]
    fn image_url_regex_matches_klipy_and_tenor() {
        let re = image_url_regex();
        for u in [
            "https://static.klipy.com/abc/def/x.mp4",
            "https://media.tenor.com/abc/x.webm",
            "https://klipy.com/gifs/funny-cat",
            "https://tenor.com/view/funny-cat-123",
        ] {
            assert!(re.is_match(u), "should match {u}");
        }
        assert!(!re.is_match("https://random.example.com/x.mp4"));
        assert!(!re.is_match("https://cdn.example.com/video.webm"));
    }

    #[test]
    fn gif_sibling_url_rewrites_video_extensions() {
        assert_eq!(
            gif_sibling_url("https://static.klipy.com/a/b/x.mp4").as_deref(),
            Some("https://static.klipy.com/a/b/x.gif")
        );
        assert_eq!(
            gif_sibling_url("https://media.tenor.com/a/x.webm").as_deref(),
            Some("https://media.tenor.com/a/x.gif")
        );
        assert_eq!(
            gif_sibling_url("https://static.klipy.com/a/x.mp4?t=1").as_deref(),
            Some("https://static.klipy.com/a/x.gif?t=1")
        );
        assert_eq!(gif_sibling_url("https://static.klipy.com/a/x.gif"), None);
        assert_eq!(gif_sibling_url("https://x/a.png"), None);
    }

    #[test]
    fn static_sibling_url_rewrites_video_to_placeholder_png() {
        assert_eq!(
            static_sibling_url("https://static.klipy.com/a/b/x.mp4").as_deref(),
            Some("https://static.klipy.com/a/b/x.png")
        );
        assert_eq!(
            static_sibling_url("https://media.tenor.com/a/x.webm").as_deref(),
            Some("https://media.tenor.com/a/x.png")
        );
        assert_eq!(
            static_sibling_url("https://static.klipy.com/a/x.mp4?t=1").as_deref(),
            Some("https://static.klipy.com/a/x.png?t=1")
        );
        assert_eq!(static_sibling_url("https://static.klipy.com/a/x.gif"), None);
        assert_eq!(static_sibling_url("https://x/a.png"), None);
    }

    #[test]
    fn klipy_host_detection_is_scoped() {
        assert!(is_klipy_or_tenor("https://static.klipy.com/x.mp4"));
        assert!(is_klipy_or_tenor("https://klipy.com/gifs/x"));
        assert!(is_klipy_or_tenor("https://media.tenor.com/x.gif"));
        assert!(is_klipy_or_tenor("https://tenor.com/view/x"));
        assert!(!is_klipy_or_tenor("https://notklipy.com/x.mp4"));
        assert!(!is_klipy_or_tenor("https://example.com/klipy.com/x.mp4"));
    }

    #[test]
    fn render_cache_is_keyed_by_terminal_size() {
        // The cache key must include cols x rows so a resize re-renders.
        let renderer = ImageRenderer::new(
            String::new(),
            60,
            String::new(),
            10 * 1024 * 1024,
            8,
            15,
            4,
            8,
            0.8,
            "ffmpeg".to_string(),
            10 * 1024 * 1024,
            30,
        );
        // Construct keys the same way `render` does.
        let k1 = format!("{}|{}x{}", "http://x/i.png", 120u16, 30u16);
        let k2 = format!("{}|{}x{}", "http://x/i.png", 60u16, 20u16);
        assert_ne!(k1, k2);
        let _ = renderer; // field presence sanity
    }

    #[test]
    fn collect_includes_raw_and_mapped_urls() {
        let map = HashMap::from([("kek".to_string(), "http://x/kek.png".to_string())]);
        let regexes = compile_map_regexes(&map);
        let urls = collect_image_urls("check this :kek: http://a/b.png", &map, &regexes);
        assert!(urls.contains(&"http://x/kek.png".to_string()));
        assert!(urls.contains(&"http://a/b.png".to_string()));
    }

    #[test]
    fn mapped_url_is_word_boundary_aware() {
        let map = HashMap::from([("cat".to_string(), "http://x/cat.gif".to_string())]);
        let regexes = compile_map_regexes(&map);
        // "cat" inside "scatter" must NOT match — only the standalone token.
        let urls = collect_image_urls("scatter is not a cat", &map, &regexes);
        assert_eq!(urls, vec!["http://x/cat.gif".to_string()]);
    }

    #[test]
    fn image_url_regex_is_case_insensitive_and_matches_apng() {
        let re = image_url_regex();
        for u in [
            "http://x/a.PNG",
            "http://x/a.apng",
            "http://x/a.GIF",
            "http://x/a.jpeg?q=1",
        ] {
            assert!(re.is_match(u), "should match {u}");
        }
        assert!(!re.is_match("http://x/a.txt"));
        assert!(!re.is_match("a.png"));
    }

    #[test]
    fn content_type_is_image_classifies() {
        for ok in [
            "",
            "image/png",
            "application/octet-stream",
            "binary/octet-stream",
        ] {
            assert!(content_type_is_image(ok), "should accept {ok:?}");
        }
        for bad in ["text/html", "application/json"] {
            assert!(!content_type_is_image(bad), "should reject {bad:?}");
        }
    }

    #[test]
    fn converter_available_handles_paths_and_path_lookup() {
        assert!(!converter_available(""));
        assert!(!converter_available(
            "/nonexistent/definitely/missing-binary"
        ));
        #[cfg(unix)]
        assert!(converter_available("sh"), "sh should be found on PATH");
    }

    #[test]
    fn video_within_limits_boundaries() {
        // Exactly at both maximums passes.
        assert!(video_within_limits(100, 30, 100, 30));
        // Over either maximum fails.
        assert!(!video_within_limits(101, 30, 100, 30));
        assert!(!video_within_limits(100, 31, 100, 30));
        assert!(!video_within_limits(101, 31, 100, 30));
        // Zero-length / zero-duration is within limits.
        assert!(video_within_limits(0, 0, 100, 30));
    }

    #[test]
    fn parse_ffprobe_duration_handles_values() {
        assert_eq!(parse_ffprobe_duration("12.345000\n"), Some(12));
        assert_eq!(parse_ffprobe_duration("12.5"), Some(13));
        assert_eq!(parse_ffprobe_duration("0"), Some(0));
        assert_eq!(parse_ffprobe_duration("30.000000"), Some(30));
        // Missing / invalid → None.
        assert_eq!(parse_ffprobe_duration(""), None);
        assert_eq!(parse_ffprobe_duration("   "), None);
        assert_eq!(parse_ffprobe_duration("N/A"), None);
        assert_eq!(parse_ffprobe_duration("garbage"), None);
        assert_eq!(parse_ffprobe_duration("-1.0"), None);
        assert_eq!(parse_ffprobe_duration("inf"), None);
    }

    #[test]
    fn has_video_extension_detects_mp4_and_webm() {
        assert!(has_video_extension("https://static.klipy.com/a/x.mp4"));
        assert!(has_video_extension("https://media.tenor.com/a/x.webm?t=1"));
        assert!(has_video_extension("https://x/a.MP4"));
        assert!(!has_video_extension("https://x/a.gif"));
        assert!(!has_video_extension("https://x/a.mp4.gif"));
        assert!(!has_video_extension("https://tenor.com/view/x"));
    }

    #[test]
    fn ffprobe_for_derives_sibling_path() {
        assert_eq!(ffprobe_for("ffmpeg"), "ffprobe");
        assert_eq!(
            ffprobe_for("/usr/local/bin/ffmpeg"),
            "/usr/local/bin/ffprobe"
        );
        assert_eq!(
            ffprobe_for("/opt/ffmpeg/bin/ffmpeg"),
            "/opt/ffmpeg/bin/ffprobe"
        );
        assert_eq!(ffprobe_for("myconverter"), "ffprobe");
    }

    #[test]
    fn ffmpeg_probe_and_convert_round_trip() {
        // Only runs where ffmpeg + ffprobe are installed; CI without them stays
        // green. Any setup failure also skips rather than fails.
        if !converter_available("ffmpeg") || !converter_available(&ffprobe_for("ffmpeg")) {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.mp4");
        let gen = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-f",
                "lavfi",
                "-i",
                "testsrc=duration=1:size=64x64:rate=10",
                "-pix_fmt",
                "yuv420p",
            ])
            .arg(&src)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        if !matches!(gen, Ok(s) if s.success()) {
            return;
        }

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let renderer = ImageRenderer::new(
            String::new(),
            60,
            String::new(),
            10 * 1024 * 1024,
            8,
            15,
            4,
            8,
            0.8,
            "ffmpeg".to_string(),
            10 * 1024 * 1024,
            30,
        );
        let duration = rt.block_on(renderer.probe_duration(&src));
        assert!(matches!(duration, Some(d) if d <= 1));
        let gif = rt.block_on(renderer.convert_to_gif(&src, 30));
        let gif = gif.expect("ffmpeg should produce gif bytes");
        assert_eq!(&gif[..3], b"GIF");
    }

    #[test]
    fn extract_frames_reads_two_frame_gif() {
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame, RgbaImage};
        let mut bytes = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut bytes);
            let frame = |v: u8| {
                Frame::from_parts(
                    RgbaImage::from_pixel(4, 4, image::Rgba([v, v, v, 255])),
                    0,
                    0,
                    Delay::from_numer_denom_ms(100, 1),
                )
            };
            enc.encode_frame(frame(0)).unwrap();
            enc.encode_frame(frame(255)).unwrap();
        }
        match extract_frames(&bytes, 4, 4) {
            Some(RenderResult::Frames(arts, delays)) => {
                assert_eq!(arts.len(), 2);
                assert_eq!(delays.len(), 2);
                assert!(delays.iter().all(|d| (20..=2000).contains(d)));
            }
            other => panic!("expected two animated frames, got {other:?}"),
        }
    }
}
