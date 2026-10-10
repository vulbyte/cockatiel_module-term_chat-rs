use rodio::{Decoder, OutputStream, Sink, Source};
use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

/// One unit of audio to speak, in FIFO order.
enum PlayJob {
    /// Audio that rode WITH the message (content-generating modules such as
    /// tts-rs run in-process and attach it).
    Inline { bytes: Vec<u8> },
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
    /// Start the player. `volume` and `max_seconds` bound playback.
    ///
    /// The worker runs on a dedicated OS thread (not a tokio task): rodio's
    /// `OutputStream`/`Sink` are not `Send`, so they can't cross an `.await`.
    pub fn start(volume: f32, max_seconds: f32) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let queued = Arc::new(std::sync::Mutex::new(HashSet::new()));
        let player = AudioPlayer {
            tx,
            queued: queued.clone(),
        };

        std::thread::spawn(move || {
            let Ok((_stream, handle)) = OutputStream::try_default() else { return };
            let Ok(sink) = Sink::try_new(&handle) else { return };
            while let Some(job) = rx.blocking_recv() {
                let bytes = match &job {
                    PlayJob::Inline { bytes } => bytes.clone(),
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
