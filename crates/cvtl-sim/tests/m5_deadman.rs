//! Concept-only independent watchdog; this does not establish R-01 for a real instrument.
use std::time::Duration;

#[derive(Debug, PartialEq, Eq)]
enum Relay {
    Energized,
    Open,
    Unknown,
}

struct IndependentWatchdog {
    timeout: Duration,
    last_heartbeat: Duration,
    relay: Relay,
    relay_available: bool,
}

impl IndependentWatchdog {
    fn heartbeat(&mut self, local_time: Duration) {
        self.last_heartbeat = local_time;
    }

    fn advance_local_clock(&mut self, local_time: Duration) {
        if local_time.saturating_sub(self.last_heartbeat) >= self.timeout {
            self.relay = if self.relay_available {
                Relay::Open
            } else {
                Relay::Unknown
            };
        }
    }
}

#[test]
fn abandoned_host_opens_independent_simulated_relay() {
    let mut watchdog = IndependentWatchdog {
        timeout: Duration::from_secs(2),
        last_heartbeat: Duration::ZERO,
        relay: Relay::Energized,
        relay_available: true,
    };
    watchdog.heartbeat(Duration::from_secs(1));
    watchdog.advance_local_clock(Duration::from_millis(2999));
    assert_eq!(watchdog.relay, Relay::Energized);
    // The host is abandoned; only the independent watchdog clock advances.
    watchdog.advance_local_clock(Duration::from_secs(3));
    assert_eq!(watchdog.relay, Relay::Open);
}

#[test]
fn unavailable_relay_does_not_create_off_evidence() {
    let mut watchdog = IndependentWatchdog {
        timeout: Duration::from_secs(2),
        last_heartbeat: Duration::ZERO,
        relay: Relay::Energized,
        relay_available: false,
    };
    watchdog.advance_local_clock(Duration::from_secs(2));
    assert_eq!(watchdog.relay, Relay::Unknown);
}
