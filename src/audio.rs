use crate::engine::EngineHandle;
use rodio::{Decoder, OutputStream, Sink, Source};
use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

/// Fetches + plays message audio from the engine. Every `fetch_and_play` shares
/// the same `audio_for_message` query_id, and the engine matches query results
/// by query_id over a broadcast — NOT by the requesting uuid. Two concurrent
/// fetches would therefore both receive the FIRST result and play the WRONG
/// message's audio. This type serializes fetch+enqueue so only one audio query
/// is in flight at a time, and dedups so a uuid that was already fetched/played
/// is never refetched (engine re-deliveries / recovery re-queues).
#[derive(Clone)]
pub struct AudioFetcher {
    inner: Arc<AudioFetcherInner>,
}

impl AudioFetcher {
    /// Serializes fetch + enqueue so concurrent fetches can't cross-match.
    /// Records the message uuid7s whose audio has already been fetched/played.
    pub fn with_retries(retries: u32, retry_delay_ms: u64) -> Self {
        Self {
            inner: Arc::new(AudioFetcherInner {
                gate: tokio::sync::Mutex::new(()),
                fetched: std::sync::Mutex::new(HashSet::new()),
                retries: retries.max(1),
                retry_delay: Duration::from_millis(retry_delay_ms),
            }),
        }
    }
}

impl Default for AudioFetcher {
    fn default() -> Self {
        Self::with_retries(3, 800)
    }
}

struct AudioFetcherInner {
    /// Serializes fetch + enqueue so concurrent fetches can't cross-match.
    gate: tokio::sync::Mutex<()>,
    /// Message uuid7s whose audio has already been fetched/played.
    fetched: std::sync::Mutex<HashSet<String>>,
    /// Fetch attempts before giving up on a TTS clip.
    retries: u32,
    /// Delay between fetch attempts.
    retry_delay: Duration,
}

impl AudioFetcher {
    /// Play audio that rode WITH the message (no engine round-trip needed).
    /// Records the message uuid so a later engine re-delivery won't fetch +
    /// replay the same clip.
    pub fn play_inline(&self, uuid7: &str, bytes: Vec<u8>, volume: f64, max_seconds: f64) {
        if !uuid7.is_empty() {
            self.mark_fetched(uuid7);
        }
        play_audio(bytes, volume, max_seconds);
    }

    fn mark_fetched(&self, uuid7: &str) {
        if let Ok(mut set) = self.inner.fetched.lock() {
            set.insert(uuid7.to_string());
        }
    }

    fn already_fetched(&self, uuid7: &str) -> bool {
        self.inner
            .fetched
            .lock()
            .map(|set| set.contains(uuid7))
            .unwrap_or(false)
    }

    /// Fetch a message's rendered audio from the engine (saved to the timeline
    /// by the TTS module after the message broadcasts) and play it. Retries
    /// briefly because post-process synthesis happens right after the display
    /// sees the message. Serialized against other fetches so concurrent fetches
    /// can't cross-match on the shared query id.
    pub async fn fetch_and_play(
        &self,
        engine: &EngineHandle,
        uuid7: &str,
        volume: f64,
        max_seconds: f64,
    ) {
        if uuid7.is_empty() || self.already_fetched(uuid7) {
            return;
        }
        // Hold the gate through fetch AND enqueue: a second concurrent fetch
        // waits, so it can't receive the first fetch's result. Tradeoff: audio
        // queries serialize (at most one in flight), but the critical section
        // is short (network fetch + rodio enqueue) and actual playback runs on
        // its own rodio thread, so sound output is never serialized by this.
        let _gate = self.inner.gate.lock().await;
        if self.already_fetched(uuid7) {
            return; // fetched by the task we were waiting on
        }
        for attempt in 0..self.inner.retries {
            if attempt > 0 {
                tokio::time::sleep(self.inner.retry_delay).await;
            }
            let sql = serde_json::json!({ "uuid7": uuid7 }).to_string();
            if let Ok(res) = engine.db_query("audio_for_message", &sql).await {
                if res.success && !res.result_blob.is_empty() {
                    play_audio(res.result_blob, volume, max_seconds);
                    self.mark_fetched(uuid7);
                    return;
                }
            }
        }
    }
}

/// Play rendered audio (mp3/wav bytes) via rodio. Runs on a background thread
/// so playback never blocks the UI loop. Enforces the volume and the max
/// clip-length cap: clips longer than `max_seconds` are skipped entirely, and
/// even unknown-length clips are stopped once the cap elapses.
pub fn play_audio(bytes: Vec<u8>, volume: f64, max_seconds: f64) {
    if bytes.is_empty() {
        return;
    }
    std::thread::spawn(move || {
        let Ok((_stream, handle)) = OutputStream::try_default() else { return };
        let Ok(sink) = Sink::try_new(&handle) else { return };
        let Ok(decoder) = Decoder::new(Cursor::new(bytes)) else { return };

        // Known-length clips over the cap are skipped.
        if let Some(d) = decoder.total_duration() {
            if d.as_secs_f64() > max_seconds {
                return;
            }
        }

        sink.set_volume(volume.clamp(0.0, 1.0) as f32);
        sink.append(decoder);

        // Hard cap: stop after max_seconds even when the length was unknown.
        let deadline = std::time::Instant::now()
            + Duration::from_secs_f64(max_seconds.max(0.1));
        loop {
            if sink.empty() {
                break;
            }
            if std::time::Instant::now() >= deadline {
                sink.stop();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    });
}