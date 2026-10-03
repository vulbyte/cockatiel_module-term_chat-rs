use crate::engine::EngineHandle;
use rodio::{Decoder, OutputStream, Sink, Source};
use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

/// One unit of audio to speak, in FIFO order.
enum PlayJob {
    /// Audio that rode WITH the message (pre/in-process modules attach it).
    Inline { bytes: Vec<u8> },
    /// A post-process TTS clip: fetch the rendered bytes from the engine by
    /// message uuid (the audio is persisted only after the TTS module acks).
    Fetch { uuid7: String },
}

/// A single, strictly-serialized audio player.
///
/// The old design spawned one thread + one `OutputStream` per clip, so N
/// concurrent clips opened N output streams over the same default device on
/// macOS: they overlapped, and some `OutputStream::try_default()` calls failed
/// silently, meaning a message was displayed but never heard. This type instead
/// runs ONE worker that owns ONE `OutputStream` + `Sink` for its whole life and
/// drains a FIFO queue — a clip is played to completion before the next one is
/// even dequeued. Playback is therefore always one clip at a time, in display
/// order, never overlapping.
#[derive(Clone)]
pub struct AudioPlayer {
    tx: tokio::sync::mpsc::UnboundedSender<PlayJob>,
    queued: Arc<std::sync::Mutex<HashSet<String>>>,
}

impl AudioPlayer {
    /// Start the player. `engine_state` is re-read per fetch so a reconnect's
    /// fresh handle is always used. `retries`/`retry_delay_ms` bound how long a
    /// Fetch job waits for the TTS module to persist its clip.
    ///
    /// The worker runs on a dedicated OS thread (not a tokio task): rodio's
    /// `OutputStream`/`Sink` are not `Send`, so they can't cross an `.await`.
    /// The thread owns a private runtime for the async engine fetches.
    pub fn start(
        engine_state: Arc<tokio::sync::RwLock<EngineHandle>>,
        volume: f32,
        max_seconds: f32,
        retries: u32,
        retry_delay_ms: u32,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let queued = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let player = AudioPlayer {
            tx,
            queued: queued.clone(),
        };

        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(_) => return player,
        };
        std::thread::spawn(move || {
            let Ok((_stream, handle)) = OutputStream::try_default() else { return };
            let Ok(sink) = Sink::try_new(&handle) else { return };
            let retries = retries.max(1);
            let retry_delay = Duration::from_millis(retry_delay_ms as u64);
            while let Some(job) = rx.blocking_recv() {
                let bytes = match &job {
                    PlayJob::Inline { bytes } => bytes.clone(),
                    PlayJob::Fetch { uuid7 } => {
                        let uuid7 = uuid7.clone();
                        let engine_state = engine_state.clone();
                        rt.block_on(async move {
                            for attempt in 0..retries {
                                if attempt > 0 {
                                    tokio::time::sleep(retry_delay).await;
                                }
                                let sql = serde_json::json!({ "uuid7": uuid7 }).to_string();
                                let engine = engine_state.read().await.clone();
                                if let Ok(res) = engine.db_query("audio_for_message", &sql).await {
                                    if res.success && !res.result_blob.is_empty() {
                                        return Some(res.result_blob);
                                    }
                                }
                            }
                            None
                        })
                        .unwrap_or_default()
                    }
                };
                if bytes.is_empty() {
                    continue;
                }
                let Ok(decoder) = Decoder::new(Cursor::new(bytes)) else { continue };
                // Known-length clips over the cap are skipped.
                if let Some(d) = decoder.total_duration() {
                    if d.as_secs_f64() > max_seconds as f64 {
                        continue;
                    }
                }
                sink.set_volume(volume.clamp(0.0, 1.0));
                sink.append(decoder);
                // Wait for the clip to finish (with a hard cap for unknown
                // lengths) BEFORE taking the next job — this is the
                // serialization: one sound at a time, in order.
                let deadline = std::time::Instant::now()
                    + Duration::from_secs_f64(max_seconds.max(0.1) as f64);
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
            }
        });

        player
    }

    /// Queue audio that rode with the message. Deduped by uuid so an engine
    /// re-delivery never plays the same clip twice.
    pub fn play_inline(&self, uuid7: &str, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if !self.claim(uuid7) {
            return;
        }
        let _ = self.tx.send(PlayJob::Inline {
            bytes,
        });
    }

    /// Queue a post-process TTS clip to be fetched from the engine by uuid and
    /// played in order. Deduped by uuid like inline audio.
    pub fn play_tts(&self, uuid7: &str) {
        if uuid7.is_empty() {
            return;
        }
        if !self.claim(uuid7) {
            return;
        }
        let _ = self.tx.send(PlayJob::Fetch {
            uuid7: uuid7.to_string(),
        });
    }

    /// Record a uuid as queued/played, returning false if it already was.
    fn claim(&self, uuid7: &str) -> bool {
        let mut set = self.queued.lock().unwrap();
        if set.contains(uuid7) {
            false
        } else {
            set.insert(uuid7.to_string());
            true
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    /// The claim() dedup must accept a uuid once and reject repeats, so an
    /// engine re-delivery never double-enqueues the same clip into the
    /// serialized player.
    #[test]
    fn claim_dedupes_by_uuid() {
        let queued = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let player = AudioPlayer {
            tx: tokio::sync::mpsc::unbounded_channel().0,
            queued,
        };
        assert!(player.claim("a"), "first claim accepted");
        assert!(!player.claim("a"), "repeat claim rejected");
        assert!(player.claim("b"), "a different uuid is accepted");
        assert!(!player.claim("b"), "its repeat is rejected");
    }
}
