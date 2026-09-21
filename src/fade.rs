use std::time::Duration;

/// How a message should fade out once its lifetime elapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeMode {
    /// Drop the message outright when the timeout hits.
    Remove,
    /// Dim the message during a short fade-out window, then drop it.
    Dim,
}

impl FadeMode {
    /// Parse a config value. Unknown/empty values fall back to `Remove`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "dim" => FadeMode::Dim,
            _ => FadeMode::Remove,
        }
    }
}

/// What the fade logic wants the renderer to do with a message right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FadeAction {
    /// Keep rendering normally.
    Keep,
    /// Keep rendering, but dimmed (only produced in `Dim` mode).
    Dim,
    /// The message is past its lifetime and should be removed from the display.
    Remove,
}

/// Length of the fade-out window used in `Dim` mode: the final ~20% of the
/// lifetime, capped at 2 seconds so short lifetimes still get a quick dim.
pub fn fade_window(fade_secs: u64) -> Duration {
    if fade_secs == 0 {
        return Duration::ZERO;
    }
    let fraction = Duration::from_secs_f64(fade_secs as f64 * 0.2);
    fraction.min(Duration::from_secs(2))
}

/// Decide what to do with a message of the given `age`.
///
/// `fade_secs == 0` disables fading entirely. In `Remove` mode the message is
/// kept until `age >= fade_secs`, then removed. In `Dim` mode it dims for the
/// last `fade_window(fade_secs)` before being removed at `fade_secs`.
pub fn fade_action(age: Duration, fade_secs: u64, mode: FadeMode) -> FadeAction {
    if fade_secs == 0 {
        return FadeAction::Keep;
    }
    let total = Duration::from_secs(fade_secs);
    match mode {
        FadeMode::Remove => {
            if age >= total {
                FadeAction::Remove
            } else {
                FadeAction::Keep
            }
        }
        FadeMode::Dim => {
            let dim_start = total.saturating_sub(fade_window(fade_secs));
            if age >= total {
                FadeAction::Remove
            } else if age >= dim_start {
                FadeAction::Dim
            } else {
                FadeAction::Keep
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_fade_keeps_forever() {
        assert_eq!(
            fade_action(Duration::from_secs(9999), 0, FadeMode::Remove),
            FadeAction::Keep
        );
        assert_eq!(
            fade_action(Duration::from_secs(9999), 0, FadeMode::Dim),
            FadeAction::Keep
        );
        assert_eq!(fade_window(0), Duration::ZERO);
    }

    #[test]
    fn remove_mode_drops_at_timeout() {
        assert_eq!(
            fade_action(Duration::from_secs(4), 5, FadeMode::Remove),
            FadeAction::Keep
        );
        assert_eq!(
            fade_action(Duration::from_secs(5), 5, FadeMode::Remove),
            FadeAction::Remove
        );
        assert_eq!(
            fade_action(Duration::from_secs(6), 5, FadeMode::Remove),
            FadeAction::Remove
        );
    }

    #[test]
    fn dim_mode_dims_then_removes() {
        // fade_secs = 10 -> window = min(2.0, 10 * 0.2) = 2s -> dim starts at 8s.
        assert_eq!(
            fade_action(Duration::from_secs(7), 10, FadeMode::Dim),
            FadeAction::Keep
        );
        assert_eq!(
            fade_action(Duration::from_secs(8), 10, FadeMode::Dim),
            FadeAction::Dim
        );
        assert_eq!(
            fade_action(Duration::from_secs(9), 10, FadeMode::Dim),
            FadeAction::Dim
        );
        assert_eq!(
            fade_action(Duration::from_secs(10), 10, FadeMode::Dim),
            FadeAction::Remove
        );
        assert_eq!(
            fade_action(Duration::from_secs(11), 10, FadeMode::Dim),
            FadeAction::Remove
        );
    }

    #[test]
    fn dim_window_is_capped_at_two_seconds() {
        assert_eq!(fade_window(1000), Duration::from_secs(2));
        // Short lifetimes keep a proportional (small) window.
        assert!(fade_window(3) < Duration::from_secs(2));
    }

    #[test]
    fn parse_mode_defaults_to_remove() {
        assert_eq!(FadeMode::parse("dim"), FadeMode::Dim);
        assert_eq!(FadeMode::parse("DIM"), FadeMode::Dim);
        assert_eq!(FadeMode::parse("remove"), FadeMode::Remove);
        assert_eq!(FadeMode::parse(""), FadeMode::Remove);
        assert_eq!(FadeMode::parse("explode"), FadeMode::Remove);
    }
}