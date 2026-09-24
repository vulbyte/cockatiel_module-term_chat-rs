use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::types::ChatMessageItem;

/// Display-rate limiter for chat messages. When enabled, a burst of messages
/// is shown at a target rate (target_per_min); if nothing has been emitted for
/// a while, buffered messages flush instantly. The pending buffer is bounded:
/// with easing enabled and sustained chat above the target rate the queue would
/// otherwise grow without bound until a long idle gap flushed it.
pub struct EasingQueue {
    pub enabled: bool,
    pub target_per_min: f64,
    max_buffered: usize,
    idle_flush: Duration,
    last_emit: Option<Instant>,
    buffer: VecDeque<ChatMessageItem>,
}

impl EasingQueue {
    pub fn new(enabled: bool, target_per_min: f64, max_buffered: usize, idle_flush_secs: u64) -> Self {
        Self {
            enabled,
            target_per_min: if target_per_min > 0.0 {
                target_per_min
            } else {
                120.0
            },
            max_buffered: max_buffered.max(1),
            idle_flush: Duration::from_secs(idle_flush_secs),
            last_emit: None,
            buffer: VecDeque::new(),
        }
    }

    /// True when a message with this id is already waiting to be shown.
    pub fn contains_id(&self, id: &str) -> bool {
        self.buffer.iter().any(|item| item.id == id)
    }

    pub fn push(&mut self, item: ChatMessageItem) {
        // Apply the buffer cap AT PUSH time (not just at display time): drop the
        // oldest pending entry when the queue would exceed the bound.
        if self.buffer.len() >= self.max_buffered {
            self.buffer.pop_front();
        }
        self.buffer.push_back(item);
    }

    /// Emit items ready to be displayed right now.
    pub fn poll(&mut self, now: Instant) -> Vec<ChatMessageItem> {
        if !self.enabled {
            return self.buffer.drain(..).collect();
        }
        if self.buffer.is_empty() {
            return Vec::new();
        }

        let interval = Duration::from_secs_f64(60.0 / self.target_per_min);
        let since = self
            .last_emit
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or(Duration::from_secs(99));

        let mut out = Vec::new();
        if since >= interval {
            if let Some(item) = self.buffer.pop_front() {
                out.push(item);
            }
            self.last_emit = Some(now);
        } else if since > self.idle_flush {
            // Idle long enough — flush the whole buffer so the chat feels live.
            out.extend(self.buffer.drain(..));
            self.last_emit = Some(now);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ChatMessageItem;
    use std::time::Instant;

    fn item(id: &str) -> ChatMessageItem {
        ChatMessageItem {
            id: id.to_string(),
            username: String::new(),
            name_color: String::new(),
            rank: String::new(),
            score: 0,
            role_badges: String::new(),
            reprimanded: false,
            platform: String::new(),
            user_handle: String::new(),
            user_uuid7: String::new(),
            content: String::new(),
            image_art: None,
            image_status: None,
            added_at: Instant::now(),
        }
    }

    #[test]
    fn push_drops_oldest_when_buffer_over_cap() {
        let mut q = EasingQueue::new(true, 30.0, 3, 2);
        for i in 0..10 {
            q.push(item(&format!("m{}", i)));
        }
        // Cap 3: only the NEWEST 3 are still pending; the oldest 7 were dropped.
        for i in 0..7 {
            assert!(!q.contains_id(&format!("m{}", i)), "m{} should be dropped", i);
        }
        for i in 7..10 {
            assert!(q.contains_id(&format!("m{}", i)), "m{} should be pending", i);
        }
    }

    #[test]
    fn contains_id_sees_pending_entries() {
        let mut q = EasingQueue::new(true, 30.0, 10, 2);
        q.push(item("m1"));
        assert!(q.contains_id("m1"));
        assert!(!q.contains_id("m2"));
    }
}