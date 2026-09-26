//! Timer event source: emits `time.*` events when registered schedules fire.
//!
//! A [`Schedule`] is one of a one-shot instant, a fixed interval, or a cron
//! expression. One-shots may be built from fuzzy phrases (`"tomorrow morning"`)
//! via [`Schedule::fuzzy`]. The [`TimerSource`] holds a set of [`Timer`]s,
//! sleeps until the next is due, and publishes an [`Event`] for each.
//!
//! Timers can be added and removed at runtime through the same cloneable
//! handle; the engine (Stage 1.7) registers a timer per time-triggered intent.

use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::Duration as StdDuration;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;

use super::{Event, EventBus, EventSource};
use crate::events::fuzzy_time::parse_fuzzy;

/// The source id used for all timer events.
pub const SOURCE_ID: &str = "timer";

/// When a timer fires.
#[derive(Debug, Clone)]
pub enum Schedule {
    /// Fire exactly once at the given instant.
    Once {
        at: DateTime<Utc>,
        /// Original fuzzy phrase, preserved for re-display/re-compilation.
        source_text: Option<String>,
    },
    /// Fire repeatedly, one `period` apart, measured from registration / last fire.
    Interval { period: StdDuration },
    /// Fire on a cron schedule (6 or 7 fields, seconds-first).
    Cron(Box<cron::Schedule>),
}

impl Schedule {
    /// One-shot schedule at a concrete instant.
    pub fn once(at: DateTime<Utc>) -> Self {
        Schedule::Once {
            at,
            source_text: None,
        }
    }

    /// One-shot schedule from a fuzzy phrase, resolved relative to `now`.
    ///
    /// Preserves the original text so it can be shown back or recompiled later.
    pub fn fuzzy(text: impl Into<String>, now: DateTime<Utc>) -> Option<Self> {
        let text = text.into();
        parse_fuzzy(&text, now).map(|at| Schedule::Once {
            at,
            source_text: Some(text),
        })
    }

    /// Recurring schedule firing every `period`.
    pub fn interval(period: StdDuration) -> Self {
        Schedule::Interval { period }
    }

    /// Cron schedule from a 6/7-field expression (seconds-first).
    pub fn cron(expr: &str) -> anyhow::Result<Self> {
        let schedule = cron::Schedule::from_str(expr)
            .map_err(|e| anyhow::anyhow!("invalid cron expression {expr:?}: {e}"))?;
        Ok(Schedule::Cron(Box::new(schedule)))
    }

    /// The event type emitted when this schedule fires.
    pub fn event_type(&self) -> &'static str {
        match self {
            Schedule::Once { .. } => "time.exact",
            Schedule::Interval { .. } => "time.interval",
            Schedule::Cron(_) => "time.cron",
        }
    }

    /// The next instant this schedule fires strictly after `after`, or `None`
    /// if it will never fire again (an elapsed one-shot).
    pub fn next_after(&self, after: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::Once { at, .. } => (*at > after).then_some(*at),
            Schedule::Interval { period } => {
                Duration::from_std(*period).ok().map(|step| after + step)
            }
            Schedule::Cron(schedule) => schedule.after(&after).next(),
        }
    }
}

/// A named schedule registered with the [`TimerSource`].
#[derive(Debug, Clone)]
pub struct Timer {
    /// Stable identifier, echoed in the fired event payload.
    pub id: String,
    /// Optional human label for diagnostics.
    pub label: Option<String>,
    pub schedule: Schedule,
}

impl Timer {
    pub fn new(id: impl Into<String>, schedule: Schedule) -> Self {
        Self {
            id: id.into(),
            label: None,
            schedule,
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }
}

/// Payload of a `time.*` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimerFired {
    pub timer_id: String,
    pub label: Option<String>,
    /// The scheduled instant this firing corresponds to.
    pub scheduled_for: DateTime<Utc>,
    /// How many times this timer has fired, including this one.
    pub fire_count: u64,
}

struct Entry {
    timer: Timer,
    next_fire: Option<DateTime<Utc>>,
    fire_count: u64,
}

#[derive(Default)]
struct State {
    entries: Vec<Entry>,
}

/// An [`EventSource`] that fires registered timers onto the bus.
///
/// Cloneable: clones share the same timer set, so callers hold one handle for
/// registration while another drives the [`EventSource::start`] loop.
#[derive(Clone)]
pub struct TimerSource {
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
}

impl TimerSource {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            wake: Arc::new(Notify::new()),
        }
    }

    /// Register (or replace, by id) a timer. Wakes the run loop to reconsider
    /// its next sleep.
    pub fn register(&self, timer: Timer) {
        let next_fire = timer.schedule.next_after(Utc::now());
        {
            let mut state = self.state.lock().unwrap();
            state.entries.retain(|e| e.timer.id != timer.id);
            state.entries.push(Entry {
                timer,
                next_fire,
                fire_count: 0,
            });
        }
        self.wake.notify_one();
    }

    /// Remove a timer by id. Returns whether one was found.
    pub fn remove(&self, id: &str) -> bool {
        let removed = {
            let mut state = self.state.lock().unwrap();
            let before = state.entries.len();
            state.entries.retain(|e| e.timer.id != id);
            state.entries.len() != before
        };
        if removed {
            self.wake.notify_one();
        }
        removed
    }

    /// Number of registered timers.
    pub fn len(&self) -> usize {
        self.state.lock().unwrap().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Fire all due timers, returning their events and the earliest pending
    /// fire time across the remaining timers.
    ///
    /// A timer overdue by more than one period fires once and skips ahead to
    /// the next future occurrence, so a sleeping daemon never produces a burst
    /// of backlogged events on wake.
    fn collect_due(&self, now: DateTime<Utc>) -> (Vec<Event>, Option<DateTime<Utc>>) {
        let mut state = self.state.lock().unwrap();
        let mut events = Vec::new();

        for entry in &mut state.entries {
            let Some(fire_at) = entry.next_fire else { continue };
            if fire_at <= now {
                entry.fire_count += 1;
                let payload = TimerFired {
                    timer_id: entry.timer.id.clone(),
                    label: entry.timer.label.clone(),
                    scheduled_for: fire_at,
                    fire_count: entry.fire_count,
                };
                events.push(Event::new(
                    entry.timer.schedule.event_type(),
                    SOURCE_ID,
                    payload,
                ));
                entry.next_fire = entry.timer.schedule.next_after(now);
            }
        }

        state.entries.retain(|e| e.next_fire.is_some());
        let earliest = state.entries.iter().filter_map(|e| e.next_fire).min();
        (events, earliest)
    }
}

impl Default for TimerSource {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl EventSource for TimerSource {
    fn source_id(&self) -> &str {
        SOURCE_ID
    }

    async fn start(&self, bus: EventBus) -> anyhow::Result<()> {
        loop {
            let (events, earliest) = self.collect_due(Utc::now());
            for event in events {
                bus.publish(event);
            }

            match earliest {
                Some(fire_at) => {
                    let wait = (fire_at - Utc::now())
                        .to_std()
                        .unwrap_or(StdDuration::ZERO);
                    // Returns early if a register/remove wakes us first.
                    let _ = tokio::time::timeout(wait, self.wake.notified()).await;
                }
                // No pending timers: idle until one is registered.
                None => self.wake.notified().await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(h: u32, mi: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 1, h, mi, s).unwrap()
    }

    #[test]
    fn once_fires_then_exhausts() {
        let s = Schedule::once(at(12, 0, 0));
        assert_eq!(s.next_after(at(11, 0, 0)), Some(at(12, 0, 0)));
        assert_eq!(s.next_after(at(12, 0, 0)), None);
        assert_eq!(s.event_type(), "time.exact");
    }

    #[test]
    fn interval_advances() {
        let s = Schedule::interval(StdDuration::from_secs(60));
        assert_eq!(s.next_after(at(12, 0, 0)), Some(at(12, 1, 0)));
        assert_eq!(s.event_type(), "time.interval");
    }

    #[test]
    fn cron_computes_next() {
        // Every day at 09:00:00 (seconds-first 6-field form).
        let s = Schedule::cron("0 0 9 * * *").unwrap();
        assert_eq!(s.next_after(at(8, 0, 0)), Some(at(9, 0, 0)));
        assert_eq!(s.event_type(), "time.cron");
        assert!(Schedule::cron("not a cron").is_err());
    }

    #[test]
    fn fuzzy_builds_once_with_source_text() {
        let now = at(8, 0, 0);
        let s = Schedule::fuzzy("in 1 hour", now).unwrap();
        match s {
            Schedule::Once { at, source_text } => {
                assert_eq!(at, now + Duration::hours(1));
                assert_eq!(source_text.as_deref(), Some("in 1 hour"));
            }
            _ => panic!("expected Once"),
        }
        assert!(Schedule::fuzzy("gibberish", now).is_none());
    }

    #[test]
    fn collect_due_fires_past_timers_and_reports_earliest() {
        let source = TimerSource::new();
        source.register(Timer::new("past", Schedule::once(at(11, 0, 0))).with_label("old"));
        source.register(Timer::new("future", Schedule::once(at(23, 0, 0))));
        assert_eq!(source.len(), 2);

        let (events, earliest) = source.collect_due(at(12, 0, 0));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "time.exact");
        let fired: TimerFired = events[0].payload_as().unwrap();
        assert_eq!(fired.timer_id, "past");
        assert_eq!(fired.label.as_deref(), Some("old"));
        assert_eq!(fired.fire_count, 1);

        // The elapsed one-shot is dropped; only the future timer remains.
        assert_eq!(earliest, Some(at(23, 0, 0)));
        assert_eq!(source.len(), 1);
    }

    #[test]
    fn overdue_interval_fires_once_and_skips_ahead() {
        let source = TimerSource::new();
        source.register(Timer::new(
            "tick",
            Schedule::interval(StdDuration::from_secs(60)),
        ));
        // Force the next fire well into the past.
        {
            let mut state = source.state.lock().unwrap();
            state.entries[0].next_fire = Some(at(10, 0, 0));
        }

        let now = at(12, 0, 0);
        let (events, earliest) = source.collect_due(now);
        assert_eq!(events.len(), 1, "overdue interval fires exactly once");
        // Skips ahead relative to now, not the stale schedule.
        assert_eq!(earliest, Some(now + Duration::seconds(60)));
    }

    #[test]
    fn register_replaces_by_id() {
        let source = TimerSource::new();
        source.register(Timer::new("a", Schedule::once(at(12, 0, 0))));
        source.register(Timer::new("a", Schedule::once(at(13, 0, 0))));
        assert_eq!(source.len(), 1);
        assert!(source.remove("a"));
        assert!(!source.remove("a"));
        assert!(source.is_empty());
    }

    // Real-time test: `TimerSource` reads the chrono wall clock, which tokio's
    // paused clock can't drive, so we use short real durations instead.
    #[tokio::test]
    async fn start_emits_event_when_timer_due() {
        let bus = EventBus::new();
        let mut sub = bus.subscribe_filtered(super::super::EventFilter::Prefix("time".into()));
        let source = TimerSource::new();

        let handle = {
            let source = source.clone();
            let bus = bus.clone();
            tokio::spawn(async move { source.start(bus).await })
        };

        // Register after start so the loop is already idling on `wake`.
        source.register(Timer::new(
            "soon",
            Schedule::once(Utc::now() + Duration::milliseconds(20)),
        ));

        let event = tokio::time::timeout(StdDuration::from_secs(2), sub.recv())
            .await
            .expect("event within timeout")
            .unwrap();
        assert_eq!(event.event_type, "time.exact");
        assert_eq!(event.payload_as::<TimerFired>().unwrap().timer_id, "soon");

        handle.abort();
    }
}
