use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::types::ChatMessageItem;

/// Display-rate limiter for chat messages. When enabled, a burst of messages
/// is shown at a target rate (target_per_min); if nothing has been emitted for
/// a while, buffered messages flush instantly.
pub struct EasingQueue {
    pub enabled: bool,
    pub target_per_min: f64,
    last_emit: Option<Instant>,
    buffer: VecDeque<ChatMessageItem>,
}

impl EasingQueue {
    pub fn new(enabled: bool, target_per_min: f64) -> Self {
        Self {
            enabled,
            target_per_min: if target_per_min > 0.0 {
                target_per_min
            } else {
                120.0
            },
            last_emit: None,
            buffer: VecDeque::new(),
        }
    }

    pub fn push(&mut self, item: ChatMessageItem) {
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
        } else if since > Duration::from_secs(2) {
            // Idle long enough — flush the whole buffer so the chat feels live.
            out.extend(self.buffer.drain(..));
            self.last_emit = Some(now);
        }
        out
    }
}