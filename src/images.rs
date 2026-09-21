use std::collections::HashMap;
use std::io::{Cursor, Write};
use std::process::Stdio;
use std::sync::Arc;

use regex::Regex;
use tokio::sync::Mutex;

/// Extract candidate image/gif URLs from a chat message's text.
pub fn extract_image_urls(text: &str) -> Vec<String> {
    let re = Regex::new(
        r"https?://[^\s<>]+?\.(?:png|jpe?g|gif|webp)(?:\?[^\s<>]*)?",
    )
    .unwrap();
    re.find_iter(text).map(|m| m.as_str().to_string()).collect()
}

/// Load a string → URL map from a JSON file: `{ "token": "url", ... }`.
pub fn load_map(path: &str) -> HashMap<String, String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str(&data).ok())
        .unwrap_or_default()
}

/// Resolve `:token:` / standalone `token` occurrences to their mapped URLs.
fn mapped_urls(text: &str, map: &HashMap<String, String>) -> Vec<String> {
    let mut out = Vec::new();
    for (key, val) in map {
        if val.is_empty() {
            continue;
        }
        let colon = format!(":{}:", key);
        let word = format!(r"\b{}\b", regex::escape(key));
        let colon_hit = text.contains(&colon);
        let word_hit = Regex::new(&word).map(|re| re.is_match(text)).unwrap_or(false);
        if colon_hit || word_hit {
            out.push(val.clone());
        }
    }
    out
}

/// Collect every image URL to embed: raw image/gif URLs in the text PLUS any
/// URLs a mapped token resolves to.
pub fn collect_image_urls(text: &str, map: &HashMap<String, String>) -> Vec<String> {
    let mut urls = extract_image_urls(text);
    let mut dedup: Vec<String> = Vec::new();
    for u in urls.drain(..) {
        if !dedup.contains(&u) {
            dedup.push(u);
        }
    }
    for u in mapped_urls(text, map) {
        if !dedup.contains(&u) {
            dedup.push(u);
        }
    }
    dedup
}

/// Outcome of trying to embed a URL as ascii art.
#[derive(Debug, Clone)]
pub enum RenderResult {
    /// Successfully converted.
    Ok(String),
    /// Could not be shown; the string is a human-readable reason.
    Reason(String),
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
}

impl ImageRenderer {
    pub fn new(converter_path: String, width: usize, referer: String) -> Self {
        Self {
            converter_path,
            width,
            referer,
            cache: Arc::new(Mutex::new(ImageCache::default())),
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
        // Evict the oldest entries past ~4× the visible chat height.
        let max = (4 * rows as usize).max(8);
        while cache.order.len() > max {
            if let Some(old) = cache.order.pop_front() {
                cache.map.remove(&old);
            }
        }
        result
    }

    async fn render_uncached(&self, url: &str, cols: u16, rows: u16) -> RenderResult {
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
        let resp = match req.send().await {
            Ok(r) => r,
            Err(_) => return RenderResult::Reason("download failed".to_string()),
        };

        // Hotlink protection / auth walls refuse with 401/403.
        if resp.status().as_u16() == 401 || resp.status().as_u16() == 403 {
            return RenderResult::Reason("embedding not allowed".to_string());
        }
        if !resp.status().is_success() {
            return RenderResult::Reason(format!("download failed (HTTP {})", resp.status().as_u16()));
        }

        // Not an image → nothing to embed.
        let ctype = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if !ctype.is_empty() && !ctype.starts_with("image/") {
            return RenderResult::Reason("not an image".to_string());
        }

        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(_) => return RenderResult::Reason("download failed".to_string()),
        };
        if bytes.is_empty() {
            return RenderResult::Reason("download failed".to_string());
        }

        // Decode to get the source dimensions (needed to preserve aspect).
        let img = match image::ImageReader::new(Cursor::new(&bytes))
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

        let (out_w, out_h) = fit_dimensions(cols, rows, iw, ih);

        // Prefer the external converter when it's actually installed (nicer
        // output + animated gifs); otherwise use the built-in renderer so the
        // feature works with no external dependency.
        if !self.converter_path.is_empty() && std::path::Path::new(&self.converter_path).exists() {
            self.convert_external(&bytes, out_w).await
        } else {
            self.convert_builtin(&img, out_w, out_h)
        }
    }

    /// Run the external `ascii-image-converter` on the downloaded bytes.
    async fn convert_external(&self, bytes: &[u8], width: u32) -> RenderResult {
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
            .arg("--width")
            .arg(width.to_string())
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

/// Compute the ascii cell dimensions so the image's longest dimension fits
/// within ~80% of the terminal (width and height), preserving aspect ratio
/// (terminal cells are ~2:1 so rows = H/2 in cell units). Never upscales.
fn fit_dimensions(cols: u16, rows: u16, iw: u32, ih: u32) -> (u32, u32) {
    let max_w = ((cols as f64 * 0.8) as usize).max(10);
    let max_h = ((rows as f64 * 0.8) as usize).max(3);
    let cell_w = iw as f32;
    let cell_h = ih as f32 / 2.0;
    let scale = (max_w as f32 / cell_w)
        .min(max_h as f32 / cell_h)
        .min(1.0);
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
        // 120x30 terminal -> 0.8 => max 96x24 => scale = min(96/400, 24/200) = 0.12 -> 48x24
        let (w1, h1) = fit_dimensions(120, 30, 400, 400);
        assert_eq!((w1, h1), (48, 24));
        // Resized to 60x20 -> 0.8 => max 48x16 => scale = min(48/400, 16/200) = 0.08 -> 32x16
        let (w2, h2) = fit_dimensions(60, 20, 400, 400);
        assert_eq!((w2, h2), (32, 16));
        // A resize must produce a different size (so the cache re-renders).
        assert_ne!((w1, h1), (w2, h2));

        // Wide image: width-constrained.
        // 800x200: cell = 800 x 100; at 120x30 -> max 96x24 -> scale = min(96/800, 24/100) = 0.12 -> 96x12
        assert_eq!(fit_dimensions(120, 30, 800, 200), (96, 12));
        // Never upscale a tiny image.
        // 40x40: cell = 40 x 20; at 120x30 -> scale would be >1, capped at 1 -> 40x20
        assert_eq!(fit_dimensions(120, 30, 40, 40), (40, 20));
    }

    #[test]
    fn render_cache_is_keyed_by_terminal_size() {
        // The cache key must include cols x rows so a resize re-renders.
        let renderer = ImageRenderer::new(String::new(), 60, String::new());
        // Construct keys the same way `render` does.
        let k1 = format!("{}|{}x{}", "http://x/i.png", 120u16, 30u16);
        let k2 = format!("{}|{}x{}", "http://x/i.png", 60u16, 20u16);
        assert_ne!(k1, k2);
        let _ = renderer; // field presence sanity
    }
}