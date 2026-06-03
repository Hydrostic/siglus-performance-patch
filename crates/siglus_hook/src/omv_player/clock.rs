use crate::omv_player::looper::LoopConfig;

pub struct ClockTracker {
    last_clock_ms: Option<i64>,
}
impl ClockTracker {
    pub fn new() -> Self {
        Self {
            last_clock_ms: None,
        }
    }

    pub fn update(&mut self, clock_ms: i64, loop_config: &LoopConfig) -> ClockEvent {
        assert!(clock_ms >= 0);
        let event = match self.last_clock_ms {
            None => ClockEvent::FirstSample,
            Some(last)
                if loop_config.is_enabled()
                    && (clock_ms <= loop_config.head_guard_ms)
                    && (last
                        >= (loop_config.loop_duration_ms - loop_config.tail_guard_ms).max(0)) =>
            {
                ClockEvent::Wrapped
            }
            Some(last) if clock_ms >= last => ClockEvent::Advanced,
            Some(_) => ClockEvent::Seeked,
        };
        self.last_clock_ms = Some(clock_ms);
        event
    }
}
pub enum ClockEvent {
    FirstSample,
    Advanced,
    Wrapped,
    Seeked,
}

pub(crate) const MICROSECOND_RATIONAL: ffmpeg_next::Rational = ffmpeg_next::Rational(1, 1_000);
