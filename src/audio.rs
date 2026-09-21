use crate::engine::EngineHandle;
use rodio::{Decoder, OutputStream, Sink, Source};
use std::io::Cursor;
use std::time::Duration;

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

/// Fetch a message's rendered audio from the engine (saved to the timeline by
/// the TTS module after the message broadcasts) and play it. Retries briefly
/// because post-process synthesis happens right after the display sees the
/// message.
pub async fn fetch_and_play(engine: &EngineHandle, uuid7: &str, volume: f64, max_seconds: f64) {
    for attempt in 0..3 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(800)).await;
        }
        let sql = serde_json::json!({ "uuid7": uuid7 }).to_string();
        if let Ok(res) = engine.db_query("audio_for_message", &sql).await {
            if res.success && !res.result_blob.is_empty() {
                play_audio(res.result_blob, volume, max_seconds);
                return;
            }
        }
    }
}