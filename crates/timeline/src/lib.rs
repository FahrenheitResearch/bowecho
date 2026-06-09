//! Timeline and animation state for live/archive volume playback.

use chrono::{DateTime, Utc};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TimelineFrame {
    pub volume_time: DateTime<Utc>,
    pub volume_id: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PlaybackState {
    Stopped,
    Playing,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playback_states_are_distinct() {
        assert_ne!(PlaybackState::Stopped, PlaybackState::Playing);
    }
}
