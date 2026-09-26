//! Songbird voice receive handler — captures Discord audio and pipes to daemon STT.
//!
//! Songbird is configured to decode to mono i16 16kHz, which matches the daemon
//! STT format directly — no resampling needed.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use serenity::model::id::{ChannelId, GuildId};
use simply_daemon_api::DaemonEvent;
use simply_voice::{AudioChunk, VoiceEvent, VoiceInput};
use songbird::{Event, EventContext, EventHandler as VoiceEventHandler};
use tokio::sync::mpsc;

use super::VoiceMode;

/// Flip on to get the same tool-call/result embeds in voice that chat
/// posts. Off by default — each embed is a Discord round-trip that stalls
/// the STT event handler, and voice is hands-free anyway.
fn show_tool_activity() -> bool {
    matches!(
        std::env::var("LUMINA_VOICE_SHOW_TOOLS").as_deref(),
        Ok("1" | "true" | "yes"),
    )
}

/// Songbird event handler that receives decoded audio and sends to daemon STT.
pub struct VoiceReceiver {
    stt_input: mpsc::Sender<VoiceInput>,
}

impl VoiceReceiver {
    pub fn new(stt_input: mpsc::Sender<VoiceInput>) -> Self {
        Self { stt_input }
    }
}

#[async_trait]
impl VoiceEventHandler for VoiceReceiver {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::VoiceTick(tick) = ctx {
            let mut combined: Vec<i16> = Vec::new();
            for data in tick.speaking.values() {
                if let Some(decoded) = data.decoded_voice.as_ref() {
                    if combined.is_empty() {
                        combined.extend_from_slice(decoded);
                    } else {
                        for (i, &sample) in decoded.iter().enumerate() {
                            if i < combined.len() {
                                combined[i] = combined[i].saturating_add(sample);
                            }
                        }
                    }
                }
            }
            if !combined.is_empty() {
                let pcm16: Vec<u8> = combined.iter().flat_map(|s| s.to_le_bytes()).collect();
                // try_send so we never block the songbird event loop.
                let _ = self.stt_input.try_send(VoiceInput::Audio(AudioChunk::new(pcm16)));
            }
        }
        None
    }
}

/// Fire-and-forget channel post so the voice event handler doesn't stall
/// on Discord round-trips.
fn post_bg(http: &Arc<serenity::http::Http>, channel: ChannelId, body: String) {
    let http = http.clone();
    tokio::spawn(async move {
        if let Err(e) = channel.say(&http, body).await {
            tracing::warn!(error = %e, "voice: background channel post failed");
        }
    });
}

/// Send one user transcript to the daemon session and collect the LLM response,
/// surfacing tool activity to the text channel along the way. Runs without
/// holding `voice_mgr.sessions.lock()` so skill tools that need the same lock
/// (e.g. `leave_voice`) can run concurrently.
async fn process_llm_turn(
    daemon_session: &mut simply_daemon::DaemonSession,
    user_text: &str,
    text_channel: ChannelId,
    http: &Arc<serenity::http::Http>,
    voice_mgr: &Arc<super::VoiceManager>,
) -> Option<String> {
    if let Err(e) = daemon_session.send(simply_daemon_api::UserMessage {
        content: vec![simply_daemon_api::InputContent::Text { text: user_text.to_string() }],
    }).await {
        tracing::error!(error = %e, "failed to send to session");
        return None;
    }

    // DaemonEvent::ToolResult only carries the tool-call id; keep the
    // id→name mapping seen on ToolCall so we can title the result embed
    // with the tool's name.
    let mut tool_names: HashMap<String, String> = HashMap::new();
    let mut response_text = String::new();
    let show_tools = show_tool_activity();
    loop {
        match daemon_session.recv().await {
            Ok(DaemonEvent::TextDelta(delta)) => response_text.push_str(&delta),
            Ok(DaemonEvent::ToolCall { id, name, arguments }) => {
                if show_tools {
                    // Voice sessions don't stash tool state — nobody scrolls
                    // back through a voice turn's JSON.
                    if let Err(e) = crate::tool_render::post_tool_call(
                        http, text_channel, &id, &name, arguments, None,
                    ).await {
                        tracing::warn!(tool = %name, error = %e, "voice: failed to post tool call");
                    }
                }
                tool_names.insert(id, name);
            }
            Ok(DaemonEvent::ToolResult { id, result }) => {
                if !show_tools { continue; }
                let Some(shard) = voice_mgr.shard() else { continue };
                let name = tool_names.remove(&id).unwrap_or_else(|| "tool".to_string());
                if let Err(e) = crate::tool_render::post_tool_result(
                    http, shard, text_channel, &id, &name, result, None,
                ).await {
                    tracing::warn!(tool = %name, error = %e, "voice: failed to post tool result");
                }
            }
            Ok(DaemonEvent::TurnComplete) => break,
            Ok(DaemonEvent::Error(e)) => {
                tracing::error!(error = %e, "session error");
                break;
            }
            Err(_) => break,
            _ => {}
        }
    }
    if response_text.trim().is_empty() { None } else { Some(response_text) }
}

/// Spawn a task that processes STT events and acts on them. Returns the
/// `JoinHandle` so the owning `VoiceSession` can abort the task on stop.
pub fn spawn_event_handler(
    guild_id: GuildId,
    text_channel: ChannelId,
    mode: VoiceMode,
    mut stt_events: mpsc::Receiver<VoiceEvent>,
    http: Arc<serenity::http::Http>,
    voice_mgr: Arc<super::VoiceManager>,
    has_tts: bool,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Welcome greeting — `play_tts` is already fire-and-forget and
        // tracks its handle for barge-in, so no local spawn needed.
        if has_tts {
            if let Err(e) = voice_mgr.play_tts(guild_id, "Hi, I'm Lumina, ready to help.").await {
                tracing::warn!(error = %e, "welcome TTS failed");
            }
        }

        while let Some(event) = stt_events.recv().await {
            match event {
                VoiceEvent::UserTranscript(text) => match mode {
                    VoiceMode::Transcribe => {
                        post_bg(&http, text_channel, format!("🗣️ {text}"));
                    }
                    VoiceMode::Listen => {
                        post_bg(&http, text_channel, format!("🗣️ {text}"));

                        // Take the DaemonSession out of the map so tool calls
                        // (e.g. leave_voice, which takes sessions.lock()) can
                        // run concurrently with the send/recv loop and don't
                        // deadlock on our lock.
                        let daemon_session = {
                            let mut sessions = voice_mgr.sessions.lock().await;
                            sessions.get_mut(&guild_id).and_then(|s| s.daemon_session.take())
                        };

                        let (response, daemon_session) = if let Some(mut ds) = daemon_session {
                            let r = process_llm_turn(&mut ds, &text, text_channel, &http, &voice_mgr).await;
                            (r, Some(ds))
                        } else {
                            (None, None)
                        };

                        if let Some(ds) = daemon_session {
                            let mut sessions = voice_mgr.sessions.lock().await;
                            if let Some(s) = sessions.get_mut(&guild_id) {
                                s.daemon_session = Some(ds);
                            }
                        }

                        // The written reply always goes to the text channel;
                        // voice output is opt-in via the `say` tool (see
                        // voice/system_prompt.md).
                        if let Some(text) = response {
                            post_bg(&http, text_channel, format!("💬 {text}"));
                        }
                    }
                }
                VoiceEvent::Listening => {
                    // Barge-in: abort any in-flight synth and stop playback
                    // so a mid-synth utterance doesn't blurt over the user.
                    voice_mgr.barge_in(guild_id).await;
                }
                VoiceEvent::Error(e) => {
                    tracing::error!(guild_id = %guild_id, error = %e, "voice STT error");
                    post_bg(&http, text_channel, format!("Voice error: {e}"));
                }
                _ => {}
            }
        }
    })
}
