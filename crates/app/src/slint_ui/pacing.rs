//! Coalesce software redraws to the actual display period. Slint 1.18.1's
//! Windows timer truncates 144 Hz to 6 ms, scheduling ~167 rather than 144 Hz.
//! Only pending visible redraws are delayed; there is no idle repaint timer.
use std::time::{Duration, Instant};

/// Slint 1.18's internal Instant truncates sub-ms Duration additions. Never
/// schedule a zero-ms wake for a fractional display deadline.
pub(super) fn timer_delay(delay: Duration) -> Duration {
    let millis = delay.as_millis() as u64;
    Duration::from_millis(millis + u64::from(!delay.subsec_nanos().is_multiple_of(1_000_000)))
        .max(Duration::from_millis(1))
}

/// Minimum plausible display refresh rate (24 Hz in millihertz). Virtual, RDP,
/// or waking display drivers sometimes report bogus low values like 1000 mHz
/// (1 Hz), which would throttle UI updates to 1 FPS if accepted verbatim.
const MIN_REFRESH_MILLIHERTZ: u32 = 24_000;
const MAX_REFRESH_MILLIHERTZ: u32 = 1_000_000;
/// Sub-half-millisecond early timer wakeups redraw immediately instead of
/// arming an extra 1 ms timer round-trip.
const EARLY_WAKE_TOLERANCE: Duration = Duration::from_micros(500);

pub(super) struct Pacer {
    pub enabled: bool,
    pub refresh_known: bool,
    interval: Duration,
    next: Option<Instant>,
}
impl Pacer {
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            refresh_known: false,
            interval: Duration::from_secs_f64(1.0 / 60.0),
            next: None,
        }
    }
    pub fn set_refresh(&mut self, millihertz: Option<u32>) {
        let rate = millihertz
            .filter(|r| (MIN_REFRESH_MILLIHERTZ..=MAX_REFRESH_MILLIHERTZ).contains(r))
            .unwrap_or(60000);
        self.interval = Duration::from_secs_f64(1000.0 / f64::from(rate));
        self.refresh_known = true;
        self.next = None;
    }
    pub fn reset(&mut self) {
        self.next = None;
        self.refresh_known = false;
    }
    pub fn delay(&mut self, now: Instant) -> Option<Duration> {
        self.next = Some(if let Some(next) = self.next {
            if now < next {
                let remaining = next.duration_since(now);
                if remaining > EARLY_WAKE_TOLERANCE {
                    return Some(remaining.min(self.interval));
                }
                next + self.interval
            } else if now.duration_since(next) < self.interval {
                // Keep the display phase despite timer rounding. A late frame is
                // dropped rather than scheduling catch-up redraws in a burst.
                next + self.interval
            } else {
                now + self.interval
            }
        } else {
            now + self.interval
        });
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fractional_wakes_never_create_zero_millisecond_timers() {
        assert_eq!(
            timer_delay(Duration::from_nanos(1)),
            Duration::from_millis(1)
        );
        assert_eq!(
            timer_delay(Duration::from_micros(6944)),
            Duration::from_millis(7)
        );
        assert_eq!(
            timer_delay(Duration::from_millis(16)),
            Duration::from_millis(16)
        );
    }
    #[test]
    fn high_refresh_period_is_not_truncated_to_whole_milliseconds() {
        let mut p = Pacer::new(true);
        p.set_refresh(Some(144000));
        let now = Instant::now();
        assert!(p.delay(now).is_none());
        assert_eq!(
            p.delay(now + Duration::from_millis(6)),
            Some(Duration::from_nanos(944444))
        );
        assert!(p.delay(now + Duration::from_millis(7)).is_none());
    }
    #[test]
    fn bogus_low_refresh_rate_falls_back_to_60hz_and_tiny_early_wake_renders_now() {
        let mut p = Pacer::new(true);
        // Bogus 1 Hz driver report must not stall telemetry repaints to 1 FPS.
        p.set_refresh(Some(1000));
        let now = Instant::now();
        assert!(p.delay(now).is_none());
        assert!(p.delay(now + Duration::from_millis(17)).is_none());
        // Sub-0.5ms early wakeup proceeds immediately instead of looping timers.
        p.set_refresh(Some(60000));
        assert!(p.delay(now).is_none());
        assert!(p.delay(now + Duration::from_micros(16_300)).is_none());
    }
    #[test]
    fn delayed_requests_coalesce_and_restore_has_no_first_frame_delay() {
        let mut p = Pacer::new(true);
        p.set_refresh(None);
        let now = Instant::now();
        assert!(p.delay(now).is_none());
        assert!(p.delay(now + Duration::from_millis(3)).is_some());
        assert!(p.delay(now + Duration::from_millis(17)).is_none());
        p.reset();
        assert!(p.delay(now + Duration::from_millis(18)).is_none());
    }
}
