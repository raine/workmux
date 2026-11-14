//! When the daemon next observes, captures, heartbeats and runs maintenance.

use std::time::{Duration, Instant};

const OBSERVATION_INTERVAL: Duration = Duration::from_secs(2);
const CAPTURE_INTERVAL: Duration = Duration::from_secs(2);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
const HEARTBEAT_RETRY_INTERVAL: Duration = Duration::from_secs(1);
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(1);
/// How long a Rex daemon waits for an unreachable server before exiting.
pub(super) const REX_SERVER_GRACE: Duration = Duration::from_secs(30);
const EVENT_COALESCE_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub(super) struct Scheduler {
    next_observation: Instant,
    next_capture: Instant,
    next_heartbeat: Instant,
    next_maintenance: Instant,
    event_observation: Option<Instant>,
}

fn advance_deadline(deadline: &mut Instant, interval: Duration, now: Instant) {
    while *deadline <= now {
        *deadline += interval;
    }
}

impl Scheduler {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            next_observation: now,
            next_capture: now,
            next_heartbeat: now + HEARTBEAT_INTERVAL,
            next_maintenance: now + MAINTENANCE_INTERVAL,
            event_observation: None,
        }
    }

    pub(super) fn notify_tmux_event(&mut self, now: Instant) {
        self.event_observation
            .get_or_insert(now + EVENT_COALESCE_INTERVAL);
    }

    pub(super) fn observation_due(&self, now: Instant) -> bool {
        self.next_observation <= now
            || self
                .event_observation
                .is_some_and(|deadline| deadline <= now)
    }

    pub(super) fn finish_observation(&mut self, now: Instant) {
        if self.next_observation <= now {
            advance_deadline(&mut self.next_observation, OBSERVATION_INTERVAL, now);
        }
        self.event_observation = None;
    }

    pub(super) fn capture_due(&self, now: Instant) -> bool {
        self.next_capture <= now
    }

    pub(super) fn finish_capture(&mut self, now: Instant) {
        advance_deadline(&mut self.next_capture, CAPTURE_INTERVAL, now);
    }

    pub(super) fn heartbeat_due(&self, now: Instant) -> bool {
        self.next_heartbeat <= now
    }

    pub(super) fn finish_heartbeat(&mut self, now: Instant, success: bool) {
        self.next_heartbeat = now
            + if success {
                HEARTBEAT_INTERVAL
            } else {
                HEARTBEAT_RETRY_INTERVAL
            };
    }

    pub(super) fn maintenance_due(&self, now: Instant) -> bool {
        self.next_maintenance <= now
    }

    pub(super) fn finish_maintenance(&mut self, now: Instant) {
        advance_deadline(&mut self.next_maintenance, MAINTENANCE_INTERVAL, now);
    }

    pub(super) fn wait(&self, now: Instant) -> Duration {
        [
            self.next_observation,
            self.next_capture,
            self.next_heartbeat,
            self.next_maintenance,
            self.event_observation.unwrap_or(self.next_observation),
        ]
        .into_iter()
        .min()
        .unwrap()
        .saturating_duration_since(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduler_observes_and_captures_immediately_at_startup() {
        let now = Instant::now();
        let scheduler = Scheduler::new(now);

        assert!(scheduler.observation_due(now));
        assert!(scheduler.capture_due(now));
        assert!(!scheduler.heartbeat_due(now));
    }

    #[test]
    fn worker_and_config_publications_schedule_no_tmux_or_capture_work() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);
        scheduler.finish_capture(start);
        for publication_wakeup in [
            start + Duration::from_millis(100),
            start + Duration::from_millis(200),
        ] {
            assert!(!scheduler.observation_due(publication_wakeup));
            assert!(!scheduler.capture_due(publication_wakeup));
        }
    }

    #[test]
    fn failed_observation_keeps_the_periodic_recovery_deadline() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);

        assert!(!scheduler.observation_due(start + Duration::from_secs(1)));
        assert!(scheduler.observation_due(start + OBSERVATION_INTERVAL));
    }

    #[test]
    fn tmux_events_coalesce_without_postponing_the_first_deadline() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);
        scheduler.finish_capture(start);
        scheduler.notify_tmux_event(start + Duration::from_millis(10));
        scheduler.notify_tmux_event(start + Duration::from_millis(40));

        assert!(!scheduler.observation_due(start + Duration::from_millis(59)));
        assert!(scheduler.observation_due(start + Duration::from_millis(60)));
    }

    #[test]
    fn event_arriving_during_observation_gets_its_own_coalesced_run() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);
        scheduler.notify_tmux_event(start + Duration::from_millis(20));
        scheduler.finish_observation(start + Duration::from_millis(70));
        scheduler.notify_tmux_event(start + Duration::from_millis(80));

        assert!(!scheduler.observation_due(start + Duration::from_millis(129)));
        assert!(scheduler.observation_due(start + Duration::from_millis(130)));
    }

    #[test]
    fn periodic_observation_absorbs_a_pending_event() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);
        scheduler.notify_tmux_event(start + OBSERVATION_INTERVAL - Duration::from_millis(10));
        scheduler.finish_observation(start + OBSERVATION_INTERVAL);

        assert_eq!(scheduler.event_observation, None);
        assert!(!scheduler.observation_due(start + OBSERVATION_INTERVAL + EVENT_COALESCE_INTERVAL));
    }

    #[test]
    fn event_work_does_not_postpone_periodic_deadlines() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        scheduler.finish_observation(start);
        scheduler.finish_capture(start);
        scheduler.notify_tmux_event(start + Duration::from_millis(500));
        scheduler.finish_observation(start + Duration::from_millis(550));

        assert!(scheduler.observation_due(start + OBSERVATION_INTERVAL));
        assert!(scheduler.capture_due(start + CAPTURE_INTERVAL));
    }

    #[test]
    fn overdue_deadlines_advance_past_long_running_work() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        let after_stall = start + Duration::from_secs(7);
        scheduler.finish_observation(after_stall);
        scheduler.finish_capture(after_stall);

        assert!(!scheduler.observation_due(after_stall));
        assert!(!scheduler.capture_due(after_stall));
        assert_eq!(scheduler.next_observation, start + Duration::from_secs(8));
        assert_eq!(scheduler.next_capture, start + Duration::from_secs(8));
    }

    #[test]
    fn heartbeat_retry_and_maintenance_are_independent() {
        let start = Instant::now();
        let mut scheduler = Scheduler::new(start);
        let heartbeat = start + HEARTBEAT_INTERVAL;
        assert!(scheduler.heartbeat_due(heartbeat));
        scheduler.finish_heartbeat(heartbeat, false);
        scheduler.finish_maintenance(start + MAINTENANCE_INTERVAL);

        assert_eq!(
            scheduler.next_heartbeat,
            heartbeat + HEARTBEAT_RETRY_INTERVAL
        );
        assert_eq!(scheduler.next_maintenance, start + MAINTENANCE_INTERVAL * 2);
    }
}
