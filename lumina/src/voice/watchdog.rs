//! Voice watchdog: DMs the owner when a stage stays in flight too long.
//!
//! One DM per stuck episode. An episode is named by its in-flight stamp, so
//! it re-arms once the stage clears (or clears and restarts between polls).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serenity::all::{CreateMessage, UserId};
use serenity::model::id::GuildId;

use super::health::Stage;
use super::VoiceManager;

const POLL: Duration = Duration::from_secs(5);

struct Episode {
    since_ms: u64,
    rx_dropped_at_start: u64,
    alerted: bool,
}

impl VoiceManager {
    /// Spawn the watchdog. It runs for the life of the process.
    pub fn spawn_watchdog(
        self: &Arc<Self>,
        http: Arc<serenity::http::Http>,
        owner: UserId,
        threshold: Duration,
    ) {
        let vm = Arc::clone(self);
        tokio::spawn(async move {
            let mut episodes: HashMap<(GuildId, Stage), Episode> = HashMap::new();
            let mut tick = tokio::time::interval(POLL);
            loop {
                tick.tick().await;
                let guilds: Vec<_> = vm.health_map().iter().map(|(g, h)| (*g, Arc::clone(h))).collect();
                episodes.retain(|(g, _), _| guilds.iter().any(|(live, _)| live == g));

                for (guild_id, health) in guilds {
                    for stage in Stage::ALL {
                        let key = (guild_id, stage);
                        let Some(since_ms) = health.in_flight_since(stage) else {
                            episodes.remove(&key);
                            continue;
                        };
                        let rx_dropped = health.dropped(Stage::Rx);
                        let ep = episodes.entry(key).or_insert(Episode {
                            since_ms, rx_dropped_at_start: rx_dropped, alerted: false,
                        });
                        if ep.since_ms != since_ms {
                            *ep = Episode { since_ms, rx_dropped_at_start: rx_dropped, alerted: false };
                        }
                        let age = crate::clock::since(since_ms);
                        // An empty transcription leaves STT "in flight" until
                        // the next speech, which looks the same as a hung STT
                        // call. Only a backing-up rx (dropped audio) tells them
                        // apart, so STT alerts need both.
                        let backing_up = stage != Stage::Stt || rx_dropped > ep.rx_dropped_at_start;
                        if ep.alerted || age < threshold || !backing_up {
                            continue;
                        }
                        ep.alerted = true;
                        let Some(lines) = vm.debug_snapshot(guild_id) else { continue };
                        let body = format!(
                            "⚠️ voice watchdog: guild {guild_id}, `{}` in flight {}\n```\n{}\n```",
                            stage.label(),
                            crate::clock::fmt_age(age),
                            lines.join("\n"),
                        );
                        tracing::warn!(guild_id = %guild_id, stage = stage.label(), age_s = age.as_secs(), "voice watchdog: stage stuck");
                        if let Err(e) = owner.direct_message(&http, CreateMessage::new().content(body)).await {
                            tracing::warn!(error = %e, "voice watchdog: owner DM failed");
                        }
                    }
                }
            }
        });
    }
}
