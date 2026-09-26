//! Per-stage heartbeats for one guild's voice pipeline.
//!
//! Every stage keeps a completed-event counter, the time of its last event,
//! and an "in flight since" stamp that is set while a unit of work runs. A
//! stage stuck in flight reads as a dead-lock; counters that move while
//! nothing comes out read as a live-lock. All atomics: the rx stage is
//! recorded on every 20 ms voice tick, so no locks here.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::Duration;

use super::{TtsBinding, VoiceMode};
use crate::clock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Stage {
    /// Audio chunks forwarded from Discord to STT.
    Rx,
    /// STT finals. In flight from `Transcribing` to the next STT event.
    Stt,
    /// One daemon session turn per final (Listen mode).
    Llm,
    /// One synth call per `play_tts`.
    Tts,
    /// One songbird track, from queueing to its end.
    Play,
}

impl Stage {
    pub const ALL: [Stage; 5] = [Stage::Rx, Stage::Stt, Stage::Llm, Stage::Tts, Stage::Play];

    pub fn label(self) -> &'static str {
        match self {
            Stage::Rx => "rx audio",
            Stage::Stt => "stt final",
            Stage::Llm => "llm turn",
            Stage::Tts => "tts synth",
            Stage::Play => "playback",
        }
    }
}

#[derive(Default)]
struct Beat {
    count: AtomicU64,
    dropped: AtomicU64,
    last_ms: AtomicU64,
    /// Units in flight. `since_ms` is set on 0 → 1 and cleared on 1 → 0.
    active: AtomicU32,
    since_ms: AtomicU64,
}

/// Heartbeats plus what the session was started with.
pub struct VoiceHealth {
    mode: VoiceMode,
    stt_provider: String,
    tts: Option<TtsBinding>,
    beats: [Beat; 5],
}

impl VoiceHealth {
    pub fn new(mode: VoiceMode, stt_provider: String, tts: Option<TtsBinding>) -> Arc<Self> {
        Arc::new(Self { mode, stt_provider, tts, beats: Default::default() })
    }

    fn beat(&self, stage: Stage) -> &Beat {
        &self.beats[stage as usize]
    }

    /// One completed event.
    pub fn record(&self, stage: Stage) {
        let b = self.beat(stage);
        b.count.fetch_add(1, Relaxed);
        b.last_ms.store(clock::now_ms(), Relaxed);
    }

    /// One unit of work dropped on the floor (e.g. a full channel).
    pub fn record_drop(&self, stage: Stage) {
        self.beat(stage).dropped.fetch_add(1, Relaxed);
    }

    pub fn dropped(&self, stage: Stage) -> u64 {
        self.beat(stage).dropped.load(Relaxed)
    }

    /// Mark a unit of work in flight until the guard drops. The guard
    /// clears on drop, so an aborted task clears too.
    #[must_use]
    pub fn begin(self: &Arc<Self>, stage: Stage) -> InFlight {
        let b = self.beat(stage);
        if b.active.fetch_add(1, Relaxed) == 0 {
            b.since_ms.store(clock::now_ms(), Relaxed);
        }
        InFlight { health: Arc::clone(self), stage }
    }

    /// `Some(stamp)` while the stage has work in flight. The stamp names
    /// the episode: a new stamp means it cleared in between.
    pub fn in_flight_since(&self, stage: Stage) -> Option<u64> {
        match self.beat(stage).since_ms.load(Relaxed) {
            0 => None,
            stamp => Some(stamp),
        }
    }

    pub fn in_flight_for(&self, stage: Stage) -> Option<Duration> {
        self.in_flight_since(stage).map(clock::since)
    }

    /// Header plus one line per stage, as `/debug voice` shows it.
    pub fn render(&self) -> Vec<String> {
        let tts = self.tts.as_ref()
            .map(|b| format!("{}/{}", b.provider_id, b.voice_id))
            .unwrap_or_else(|| "none".to_string());
        let mut lines = vec![format!("mode {:?} · stt {} · tts {tts}", self.mode, self.stt_provider)];
        for stage in Stage::ALL {
            let b = self.beat(stage);
            let last = match b.last_ms.load(Relaxed) {
                0 => "never".to_string(),
                ms => format!("{} ago", clock::fmt_age(clock::since(ms))),
            };
            let mut line = format!("{:<9}  {last:>8} · {}", stage.label(), b.count.load(Relaxed));
            match b.dropped.load(Relaxed) {
                0 => {}
                n => line.push_str(&format!(" · dropped {n}")),
            }
            if let Some(age) = self.in_flight_for(stage) {
                line.push_str(&format!(" · ⚠️ in flight {}", clock::fmt_age(age)));
            }
            lines.push(line);
        }
        lines
    }
}

/// A unit of work in flight on one stage.
pub struct InFlight {
    health: Arc<VoiceHealth>,
    stage: Stage,
}

impl InFlight {
    /// Count a completion without ending the flight (the drop ends it).
    pub fn record(&self) {
        self.health.record(self.stage);
    }

    /// Count a completion and end the flight.
    pub fn done(self) {
        self.record();
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        let b = self.health.beat(self.stage);
        if b.active.fetch_sub(1, Relaxed) == 1 {
            b.since_ms.store(0, Relaxed);
        }
    }
}
