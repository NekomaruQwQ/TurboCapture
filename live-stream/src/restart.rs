//! Pure restart-boundary and bounded-backoff decisions for `live-stream`.

use std::time::Duration;

use live_shared_texture::RESOURCE_GENERATION_LOST_EXIT_CODE;

/// Delay before the first restart in one consecutive-failure sequence.
const INITIAL_BACKOFF: Duration = Duration::from_millis(250);
/// Maximum delay between attempts; long enough to avoid a hot failure loop.
const MAX_BACKOFF: Duration = Duration::from_secs(4);
/// Uptime that resets an earlier consecutive-failure sequence.
const STABLE_RESET_AFTER: Duration = Duration::from_secs(30);
/// Maximum restarts allowed before the supervisor reports exhaustion.
const MAX_CONSECUTIVE_RESTARTS: u32 = 6;

/// Managed child whose process exit triggered recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Component {
    /// Safe window capture and shared-texture producer.
    Capture,
    /// Shared-texture consumer and H.264 producer.
    Encoder,
    /// Stdin-to-WebSocket transport worker.
    Relay,
}

impl Component {
    /// Stable diagnostic label used in generation-aware supervisor logs.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Capture => "live-capture",
            Self::Encoder => "live-encoder",
            Self::Relay => "live-ws",
        }
    }
}

/// Smallest process/resource scope that safely recovers one child exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryScope {
    /// Retain the mailbox, encoder, and relay while restarting capture.
    Capture,
    /// Retain the mailbox and capture worker while recreating the media pipe pair.
    EncoderRelay,
    /// Re-select the adapter and replace the complete GPU resource cohort.
    ResourceGeneration,
}

/// Map a stable worker exit to the plan's explicit restart boundary.
pub const fn recovery_scope(component: Component, exit_code: Option<i32>) -> RecoveryScope {
    if matches!(exit_code, Some(RESOURCE_GENERATION_LOST_EXIT_CODE)) {
        return RecoveryScope::ResourceGeneration;
    }
    match component {
        Component::Capture => RecoveryScope::Capture,
        Component::Encoder | Component::Relay => RecoveryScope::EncoderRelay,
    }
}

/// Consecutive-failure tracker with capped delay and finite retry budget.
#[derive(Debug, Default)]
pub struct RestartBackoff {
    /// Attempts charged since the component last remained stable long enough.
    failures: u32,
}

impl RestartBackoff {
    /// Charge one failure and return its delay, or `None` after exhaustion.
    ///
    /// `stable_for` belongs to the process or generation that just failed. A
    /// stable interval resets old history before the new failure is charged.
    pub fn next_delay(&mut self, stable_for: Duration) -> Option<Duration> {
        if stable_for >= STABLE_RESET_AFTER {
            self.failures = 0;
        }
        if self.failures >= MAX_CONSECUTIVE_RESTARTS {
            return None;
        }
        let multiplier = 1u32 << self.failures.min(31);
        let delay = INITIAL_BACKOFF.saturating_mul(multiplier).min(MAX_BACKOFF);
        self.failures += 1;
        Some(delay)
    }

    /// Forget component-local failures after creating a new resource cohort.
    pub const fn reset(&mut self) { self.failures = 0; }

    /// Number of attempts already charged for diagnostics.
    pub const fn failures(&self) -> u32 { self.failures }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_loss_overrides_component_local_recovery() {
        assert_eq!(
            recovery_scope(Component::Capture, Some(RESOURCE_GENERATION_LOST_EXIT_CODE)),
            RecoveryScope::ResourceGeneration);
        assert_eq!(
            recovery_scope(Component::Encoder, Some(RESOURCE_GENERATION_LOST_EXIT_CODE)),
            RecoveryScope::ResourceGeneration);
        assert_eq!(recovery_scope(Component::Capture, Some(1)), RecoveryScope::Capture);
        assert_eq!(recovery_scope(Component::Relay, Some(0)), RecoveryScope::EncoderRelay);
    }

    #[test]
    fn backoff_is_capped_and_exhausts() {
        let mut backoff = RestartBackoff::default();
        let delays = std::iter::repeat_with(|| backoff.next_delay(Duration::ZERO).unwrap())
            .take(MAX_CONSECUTIVE_RESTARTS as usize)
            .collect::<Vec<_>>();
        assert_eq!(delays, vec![
            Duration::from_millis(250),
            Duration::from_millis(500),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(4),
        ]);
        assert_eq!(backoff.next_delay(Duration::ZERO), None);
    }

    #[test]
    fn stable_uptime_resets_failure_history() {
        let mut backoff = RestartBackoff::default();
        assert_eq!(backoff.next_delay(Duration::ZERO), Some(INITIAL_BACKOFF));
        assert_eq!(backoff.next_delay(Duration::ZERO), Some(Duration::from_millis(500)));
        assert_eq!(
            backoff.next_delay(STABLE_RESET_AFTER),
            Some(INITIAL_BACKOFF));
    }
}
