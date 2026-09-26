//! Voice module — manages Discord voice connections, audio pipelines,
//! and integration with the daemon's STT/TTS services.
//!
//! Flow:
//!   Songbird VoiceTick (i16 stereo 48kHz)
//!   → downsample to mono 16kHz PCM16
//!   → daemon STT stream (VoiceInput::Audio)
//!   → receive VoiceEvent::UserTranscript
//!   → post to text channel (transcribe) or send to session + TTS (listen)

mod receiver;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use serenity::all::ShardMessenger;
use serenity::model::id::{ChannelId, GuildId};
use simply_daemon_api::{
    CreateSessionOptions, Daemon, Persistence, SeedMessage,
};
use simply_rpc::RequestContext;
use songbird::{Call, Songbird};
use tokio::sync::Mutex;

/// System prompt used when an agent joins voice in Listen mode.
/// Kept in a separate markdown file so it can be edited without recompiling
/// the command layout.
const VOICE_LISTEN_SYSTEM_PROMPT: &str = include_str!("system_prompt.md");

/// Result of a `VoiceManager::join_voice` call — tells the caller whether we
/// actually joined fresh or reused an existing session.
pub enum JoinOutcome {
    Joined,
    AlreadyJoined { voice_channel: ChannelId },
}

/// Resolved TTS provider + voice for a session. Fixed at join time so
/// per-utterance synthesis doesn't re-query the daemon for the provider
/// list and voice list on every turn. A config change to provider/voice
/// takes effect on the next rejoin.
#[derive(Clone)]
pub struct TtsBinding {
    pub provider_id: String,
    pub voice_id: String,
}

/// Active voice session for a guild.
pub struct VoiceSession {
    /// The text channel where transcripts are posted.
    pub text_channel: ChannelId,
    /// The voice channel we're connected to.
    pub voice_channel: ChannelId,
    /// Session mode.
    pub mode: VoiceMode,
    /// Daemon conversation session (for listen mode — persistent across utterances).
    pub daemon_session: Option<simply_daemon::DaemonSession>,
    /// TTS binding fixed at join time. `None` in transcribe-only mode or
    /// when no TTS provider is configured (responses fall back to text).
    pub tts: Option<TtsBinding>,
    /// Handle on the receiver task — aborted when the session is stopped so
    /// we don't leave a zombie consumer of a stale STT event stream hanging
    /// around after `leave_voice`. Without this, a subsequent `/voice join`
    /// would spawn a *second* receiver while the old one is still running,
    /// causing every transcript to be posted twice.
    pub receiver_task: Option<tokio::task::JoinHandle<()>>,
    /// Handle on the current background TTS task, if any. Aborted on
    /// barge-in so a mid-synthesis utterance doesn't blurt out over the
    /// user's new turn — `handler.stop()` alone only kills already-queued
    /// audio, not an in-flight synth HTTP call. Overwriting this field
    /// (next `play_tts`) aborts any prior still-running task, which is
    /// fine because the LLM should merge back-to-back speech into one
    /// utterance anyway.
    pub tts_task: Option<tokio::task::JoinHandle<()>>,
}

/// What the voice session is doing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum VoiceMode {
    /// Transcribe-only: post speech as text messages.
    Transcribe,
    /// Full conversation: STT → LLM → TTS response.
    Listen,
}

/// Manages voice sessions across guilds.
pub struct VoiceManager {
    pub(crate) sessions: Mutex<HashMap<GuildId, VoiceSession>>,
    daemon: Arc<dyn Daemon>,
    config: Mutex<config::VoiceConfig>,
    /// Songbird handle — set once the serenity client is built.
    songbird: OnceLock<Arc<Songbird>>,
    /// Gateway shard messenger — injected from the `ready` handler so
    /// background voice tasks can drive interactive paginator buttons on
    /// their own tool-result messages without needing a `LuminaContext`.
    shard: OnceLock<ShardMessenger>,
}

impl VoiceManager {
    pub fn new(daemon: Arc<dyn Daemon>, voice_config: config::VoiceConfig) -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            daemon,
            config: Mutex::new(voice_config),
            songbird: OnceLock::new(),
            shard: OnceLock::new(),
        }
    }

    pub fn daemon(&self) -> &Arc<dyn Daemon> {
        &self.daemon
    }

    /// Inject the songbird manager. Call once after the serenity client is built.
    pub fn set_songbird(&self, songbird: Arc<Songbird>) {
        let _ = self.songbird.set(songbird);
    }

    /// Inject the gateway shard messenger. Call once from the `ready`
    /// handler — background voice tasks use this to collect pagination
    /// button interactions on their own tool-result embeds.
    pub fn set_shard(&self, shard: ShardMessenger) {
        let _ = self.shard.set(shard);
    }

    pub fn shard(&self) -> Option<&ShardMessenger> {
        self.shard.get()
    }

    /// Stop the session and disconnect from the guild's voice channel.
    /// Returns true if we were connected and left; false otherwise.
    pub async fn leave_voice(&self, guild_id: GuildId) -> anyhow::Result<bool> {
        self.stop_session(&guild_id).await;
        let Some(songbird) = self.songbird.get() else {
            anyhow::bail!("Songbird not initialized");
        };
        match songbird.leave(guild_id).await {
            Ok(()) => Ok(true),
            Err(songbird::error::JoinError::NoCall) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn stt_provider_id(&self) -> Option<String> {
        self.config.lock().await.stt_provider.clone()
    }

    pub async fn tts_provider_id(&self) -> Option<String> {
        self.config.lock().await.tts_provider.clone()
    }

    pub async fn tts_voice_id(&self) -> Option<String> {
        self.config.lock().await.tts_voice.clone()
    }

    pub async fn set_stt_provider(&self, id: String) {
        self.config.lock().await.stt_provider = Some(id);
    }

    pub async fn set_tts_provider(&self, id: String) {
        let mut config = self.config.lock().await;
        config.tts_provider = Some(id);
        // Reset voice when provider changes — voice IDs are provider-specific
        config.tts_voice = None;
    }

    pub async fn set_tts_voice(&self, id: String) {
        self.config.lock().await.tts_voice = Some(id);
    }

    pub async fn save_config(&self, lumina_cfg: &mut config::LuminaConfig) -> Result<(), String> {
        lumina_cfg.voice = self.config.lock().await.clone();
        lumina_cfg.save()
    }

    /// Whether there's an active voice session in this guild.
    pub async fn has_session(&self, guild_id: &GuildId) -> bool {
        self.sessions.lock().await.contains_key(guild_id)
    }

    /// Connect to a guild's voice channel and start a Listen-mode conversation.
    ///
    /// Idempotent: if there's already a Listen session in this guild, returns
    /// `AlreadyJoined` without touching songbird or creating a new daemon
    /// session. Otherwise connects via songbird, creates a daemon session
    /// (seeded with `seed` and the shared voice system prompt), and wires up
    /// the STT/LLM/TTS pipeline via `start_session`. Used by both
    /// `/voice join` and the `join_voice` skill tool.
    pub async fn join_voice(
        self: &Arc<Self>,
        base_ctx: RequestContext,
        guild_id: GuildId,
        voice_channel: ChannelId,
        text_channel: ChannelId,
        seed: Vec<SeedMessage>,
        http: Arc<serenity::http::Http>,
    ) -> anyhow::Result<JoinOutcome> {
        // Reserve the slot atomically so two concurrent join_voice calls
        // (e.g. /voice join racing with the voice_state_update auto-join
        // handler) can't both run the "create a session" path. The second
        // caller sees the reservation and returns AlreadyJoined immediately.
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(existing) = sessions.get(&guild_id) {
                tracing::info!(
                    guild_id = %guild_id,
                    existing_channel = %existing.voice_channel,
                    "voice_manager: already in voice, reusing session"
                );
                return Ok(JoinOutcome::AlreadyJoined { voice_channel: existing.voice_channel });
            }
            sessions.insert(guild_id, VoiceSession {
                text_channel,
                voice_channel,
                mode: VoiceMode::Listen,
                daemon_session: None,
                tts: None,
                receiver_task: None,
                tts_task: None,
            });
        }

        // From here on, any failure needs to clear the reservation.
        let result = self.try_connect_and_start(
            base_ctx, guild_id, voice_channel, text_channel, seed, http,
        ).await;
        if result.is_err() {
            self.sessions.lock().await.remove(&guild_id);
        }
        result.map(|_| JoinOutcome::Joined)
    }

    async fn try_connect_and_start(
        self: &Arc<Self>,
        base_ctx: RequestContext,
        guild_id: GuildId,
        voice_channel: ChannelId,
        text_channel: ChannelId,
        seed: Vec<SeedMessage>,
        http: Arc<serenity::http::Http>,
    ) -> anyhow::Result<()> {
        let songbird = self.songbird.get()
            .ok_or_else(|| anyhow::anyhow!("Songbird not initialized"))?;
        let call = songbird.join(guild_id, voice_channel).await?;

        let session_ctx = base_ctx
            .with_metadata("discord.guild_id", guild_id.get().to_string())
            .with_metadata("discord.channel_id", text_channel.get().to_string())
            .with_metadata("discord.voice_channel_id", voice_channel.get().to_string());

        let session = simply_daemon::DaemonSession::create(
            self.daemon.clone(),
            session_ctx,
            CreateSessionOptions {
                persistence: Some(Persistence::Ephemeral),
                system_prompt: Some(VOICE_LISTEN_SYSTEM_PROMPT.to_string()),
                model_id: None,
                seed,
                tool_filter: None,
            },
        ).await?;
        tracing::info!(session_id = %session.id(), guild_id = %guild_id, "voice listen session created");

        // Resolve TTS binding once at join — fixes provider/voice for the
        // lifetime of this session. Soft-fail: a Listen session with no TTS
        // is degraded to text-only responses rather than failing to join.
        let tts = match self.resolve_tts_binding().await {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::warn!(error = %e, "voice: no TTS available, responses will be text-only");
                None
            }
        };

        self.start_session(
            guild_id, voice_channel, text_channel,
            VoiceMode::Listen, Some(session), tts,
            call, http,
        ).await
    }

    /// Start a voice session and register the audio receive handler on the songbird Call.
    #[allow(clippy::too_many_arguments)] // flat session parameters, forwarded from the slash commands
    pub async fn start_session(
        self: &Arc<Self>,
        guild_id: GuildId,
        voice_channel: ChannelId,
        text_channel: ChannelId,
        mode: VoiceMode,
        daemon_session: Option<simply_daemon::DaemonSession>,
        tts: Option<TtsBinding>,
        call: Arc<Mutex<Call>>,
        http: Arc<serenity::http::Http>,
    ) -> anyhow::Result<()> {
        // Connect to daemon STT — use configured provider or first available
        let stt_provider_id = match self.stt_provider_id().await {
            Some(id) => id,
            None => {
                let providers = self.daemon.voice().list_voice_providers().await?;
                providers.iter()
                    .find(|p| p.capabilities.contains(&"stt".to_string()))
                    .map(|p| p.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("No STT provider available"))?
            }
        };

        let stt_handle = self.daemon.voice().voice_connect(&stt_provider_id).await?;
        let (stt_input, stt_events) = stt_handle.into_parts();

        // Register songbird receive handler — pipes audio to daemon STT
        {
            let mut handler = call.lock().await;
            handler.add_global_event(
                songbird::CoreEvent::VoiceTick.into(),
                receiver::VoiceReceiver::new(stt_input),
            );
        }

        // Spawn event handler — processes STT results. Keep the JoinHandle
        // so stop_session can abort the task, otherwise it zombies past a
        // `leave_voice` and double-processes audio once we rejoin.
        let receiver_task = receiver::spawn_event_handler(
            guild_id,
            text_channel,
            mode,
            stt_events,
            http,
            Arc::clone(self),
            tts.is_some(),
        );

        let session = VoiceSession {
            text_channel,
            voice_channel,
            mode,
            daemon_session,
            tts,
            receiver_task: Some(receiver_task),
            tts_task: None,
        };
        self.sessions.lock().await.insert(guild_id, session);
        tracing::info!(
            guild_id = %guild_id,
            voice_channel = %voice_channel,
            text_channel = %text_channel,
            mode = ?mode,
            stt_provider = %stt_provider_id,
            "voice session started"
        );
        Ok(())
    }

    /// Stop and remove the voice session for a guild. Also aborts the
    /// receiver task so it doesn't linger and double-process audio on a
    /// subsequent rejoin.
    pub async fn stop_session(&self, guild_id: &GuildId) -> Option<VoiceSession> {
        let session = self.sessions.lock().await.remove(guild_id);
        if let Some(ref s) = session {
            if let Some(ref handle) = s.receiver_task {
                handle.abort();
            }
            if let Some(ref handle) = s.tts_task {
                handle.abort();
            }
            tracing::info!(guild_id = %guild_id, "voice session stopped");
        }
        session
    }

    /// Get the active mode for a guild.
    pub async fn get_mode(&self, guild_id: &GuildId) -> Option<VoiceMode> {
        self.sessions.lock().await.get(guild_id).map(|s| s.mode)
    }

    /// Resolve the TTS provider + voice the session should use. Falls back
    /// to the first provider/voice available when the user hasn't configured
    /// one. Called once per session at join time — the result is stored on
    /// `VoiceSession.tts` so subsequent TTS calls skip the daemon round-trips.
    pub async fn resolve_tts_binding(&self) -> anyhow::Result<TtsBinding> {
        let provider_id = match self.tts_provider_id().await {
            Some(id) => id,
            None => {
                let providers = self.daemon.voice().list_voice_providers().await?;
                providers.iter()
                    .find(|p| p.capabilities.contains(&"tts".to_string()))
                    .map(|p| p.id.clone())
                    .ok_or_else(|| anyhow::anyhow!("No TTS provider available. Use /voice provider to set one."))?
            }
        };

        let voice_id = match self.tts_voice_id().await {
            Some(id) => id,
            None => match self.daemon.voice().list_voices(&provider_id).await {
                Ok(voices) if !voices.is_empty() => {
                    use rand::Rng;
                    let idx = rand::rng().random_range(0..voices.len());
                    tracing::info!(count = voices.len(), picked = %voices[idx].name, "TTS: picked random voice");
                    voices[idx].id.clone()
                }
                _ => String::new(),
            },
        };

        tracing::info!(provider = %provider_id, voice = %voice_id, "TTS binding resolved");
        Ok(TtsBinding { provider_id, voice_id })
    }

    /// Synthesize text and return audio ready for songbird (interleaved stereo f32 48kHz).
    pub async fn synthesize_for_discord(&self, text: &str, binding: &TtsBinding) -> anyhow::Result<Vec<f32>> {
        let audio = self.daemon.voice().synthesize(text, &binding.provider_id, &binding.voice_id).await?;
        let mono = audio.to_f32_samples();
        tracing::info!(
            text_len = text.len(),
            samples = mono.len(),
            source_rate = audio.format.sample_rate,
            provider = %binding.provider_id,
            voice = %binding.voice_id,
            "TTS synthesized for Discord"
        );
        Ok(resample_mono_to_stereo_48k(&mono, audio.format.sample_rate))
    }

    /// Queue TTS playback for a guild and return immediately. The synth +
    /// playback run on a background task whose handle is recorded in the
    /// session's `tts_task`, replacing (and aborting) any still-running
    /// prior one — so the LLM's tool call returns in milliseconds instead
    /// of waiting on the TTS provider, and a barge-in can cleanly cancel
    /// whatever's in flight.
    ///
    /// Returns `Ok(true)` when playback was queued, `Ok(false)` when the
    /// bot isn't in voice (caller should surface that to the LLM).
    pub async fn play_tts(self: &Arc<Self>, guild_id: GuildId, text: &str) -> anyhow::Result<bool> {
        let binding = {
            let sessions = self.sessions.lock().await;
            match sessions.get(&guild_id).and_then(|s| s.tts.clone()) {
                Some(b) => b,
                None => return Ok(false),
            }
        };
        let Some(songbird) = self.songbird.get() else {
            anyhow::bail!("Songbird not initialized");
        };
        let Some(call) = songbird.get(guild_id) else { return Ok(false); };

        let vm = self.clone();
        let text = text.to_string();
        let handle = tokio::spawn(async move {
            let stereo = match vm.synthesize_for_discord(&text, &binding).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %e, "tts synth failed");
                    return;
                }
            };
            let bytes: Vec<u8> = stereo.iter().flat_map(|s| s.to_le_bytes()).collect();
            let input = songbird::input::RawAdapter::new(std::io::Cursor::new(bytes), 48_000, 2);
            call.lock().await.play_input(input.into());
        });

        let mut sessions = self.sessions.lock().await;
        if let Some(s) = sessions.get_mut(&guild_id) {
            if let Some(prev) = s.tts_task.replace(handle) {
                prev.abort();
            }
        }
        Ok(true)
    }

    /// Barge-in: abort any in-flight TTS synth (before it plays) and stop
    /// whatever's currently playing. Called when VAD detects the user has
    /// started speaking — `handler.stop()` alone would only kill currently-
    /// queued audio, not a mid-flight synth HTTP call that's about to add
    /// a fresh track after the stop.
    pub async fn barge_in(&self, guild_id: GuildId) {
        {
            let mut sessions = self.sessions.lock().await;
            if let Some(s) = sessions.get_mut(&guild_id) {
                if let Some(task) = s.tts_task.take() {
                    task.abort();
                }
            }
        }
        if let Some(songbird) = self.songbird.get() {
            if let Some(call) = songbird.get(guild_id) {
                call.lock().await.stop();
            }
        }
    }
}

/// TypeMap key for the VoiceManager.
pub struct VoiceManagerKey;

impl serenity::prelude::TypeMapKey for VoiceManagerKey {
    type Value = Arc<VoiceManager>;
}

/// Resample mono audio to interleaved stereo 48kHz.
fn resample_mono_to_stereo_48k(mono: &[f32], source_rate: u32) -> Vec<f32> {
    let ratio = 48_000.0 / source_rate as f64;
    let output_len = (mono.len() as f64 * ratio) as usize;
    let mut stereo = Vec::with_capacity(output_len * 2);

    for i in 0..output_len {
        let src_idx = (i as f64 / ratio) as usize;
        let sample = mono.get(src_idx).copied().unwrap_or(0.0);
        stereo.push(sample);
        stereo.push(sample);
    }

    stereo
}
