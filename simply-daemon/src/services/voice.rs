//! Voice pipeline service — STT, TTS, realtime providers.

use std::sync::Arc;
use async_trait::async_trait;
use crate::api::*;
use tokio::sync::mpsc;

use std::collections::HashMap;

struct RegisteredProvider {
    info: VoiceProviderInfo,
    stt: Option<Arc<dyn simply_voice::SttProvider>>,
    tts: Option<Arc<dyn simply_voice::TtsProvider>>,
    realtime: Option<Arc<dyn simply_voice::RealtimeProvider>>,
}

pub struct VoiceService {
    providers: HashMap<String, RegisteredProvider>,
}

impl Default for VoiceService {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceService {
    pub fn new() -> Self {
        Self { providers: HashMap::new() }
    }

    pub fn register_stt(
        mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        provider: Arc<dyn simply_voice::SttProvider>,
    ) -> Self {
        let id = id.into();
        let entry = self.providers.entry(id.clone()).or_insert_with(|| RegisteredProvider {
            info: VoiceProviderInfo { id, name: name.into(), capabilities: Vec::new() },
            stt: None, tts: None, realtime: None,
        });
        entry.stt = Some(provider);
        if !entry.info.capabilities.contains(&"stt".to_string()) {
            entry.info.capabilities.push("stt".to_string());
        }
        self
    }

    pub fn register_tts(
        mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        provider: Arc<dyn simply_voice::TtsProvider>,
    ) -> Self {
        let id = id.into();
        let entry = self.providers.entry(id.clone()).or_insert_with(|| RegisteredProvider {
            info: VoiceProviderInfo { id, name: name.into(), capabilities: Vec::new() },
            stt: None, tts: None, realtime: None,
        });
        entry.tts = Some(provider);
        if !entry.info.capabilities.contains(&"tts".to_string()) {
            entry.info.capabilities.push("tts".to_string());
        }
        self
    }

    pub fn register_realtime(
        mut self,
        id: impl Into<String>,
        name: impl Into<String>,
        provider: Arc<dyn simply_voice::RealtimeProvider>,
    ) -> Self {
        let id = id.into();
        let entry = self.providers.entry(id.clone()).or_insert_with(|| RegisteredProvider {
            info: VoiceProviderInfo { id, name: name.into(), capabilities: Vec::new() },
            stt: None, tts: None, realtime: None,
        });
        entry.realtime = Some(provider);
        if !entry.info.capabilities.contains(&"realtime".to_string()) {
            entry.info.capabilities.push("realtime".to_string());
        }
        self
    }
}

/// Spawn the STT pipeline: VoiceInput → VAD → SttProvider → VoiceEvents.
fn spawn_stt_pipeline(
    stt: Arc<dyn simply_voice::SttProvider>,
    mut input_rx: mpsc::Receiver<simply_voice::VoiceInput>,
    event_tx: mpsc::Sender<simply_voice::VoiceEvent>,
) {
    tokio::spawn(async move {
        use simply_voice::{VadEvent, VoiceActivityDetector, VoiceEvent, VoiceInput, AudioChunk};

        let mut vad = VoiceActivityDetector::new();

        while let Some(input) = input_rx.recv().await {
            let VoiceInput::Audio(chunk) = input;

            let samples: Vec<i16> = chunk.data.chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]))
                .collect();

            if let Some(vad_event) = vad.process(&samples) {
                match vad_event {
                    VadEvent::SpeechStart => {
                        let _ = event_tx.send(VoiceEvent::Listening).await;
                    }
                    VadEvent::SpeechChunk(_) => {}
                    VadEvent::SpeechEnd(audio_samples) => {
                        tracing::info!(samples = audio_samples.len(), "speech ended, transcribing");
                        let _ = event_tx.send(VoiceEvent::Transcribing).await;

                        let bytes: Vec<u8> = audio_samples.iter()
                            .flat_map(|s| s.to_le_bytes())
                            .collect();
                        let audio = AudioChunk::new(bytes);

                        match stt.transcribe(audio).await {
                            Ok(t) if !t.text.trim().is_empty() => {
                                tracing::info!(text = %t.text, "STT transcription");
                                let _ = event_tx.send(VoiceEvent::UserTranscript(t.text)).await;
                            }
                            Ok(_) => {
                                tracing::debug!("STT: empty transcription");
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "STT transcription failed");
                                let _ = event_tx.send(VoiceEvent::Error(format!("STT failed: {e}"))).await;
                            }
                        }
                    }
                }
            }
        }
    });
}

/// Spawn the realtime pipeline: VoiceInput → RealtimeProvider → VoiceEvents.
fn spawn_realtime_pipeline(
    realtime: Arc<dyn simply_voice::RealtimeProvider>,
    mut input_rx: mpsc::Receiver<simply_voice::VoiceInput>,
    event_tx: mpsc::Sender<simply_voice::VoiceEvent>,
) {
    tokio::spawn(async move {
        use simply_voice::{RealtimeConfig, RealtimeEvent, RealtimeInput, VoiceEvent, VoiceInput};

        let config = RealtimeConfig::default();
        let (rt_tx, mut rt_rx) = match realtime.connect(config).await {
            Ok(pair) => pair,
            Err(e) => {
                let _ = event_tx.send(VoiceEvent::Error(format!("Realtime connect failed: {e}"))).await;
                return;
            }
        };

        // Forward input to realtime provider
        let rt_tx_clone = rt_tx.clone();
        tokio::spawn(async move {
            while let Some(input) = input_rx.recv().await {
                let rt_input = match input {
                    VoiceInput::Audio(chunk) => RealtimeInput::Audio(chunk),
                };
                if rt_tx_clone.send(rt_input).await.is_err() {
                    break;
                }
            }
        });

        // Forward realtime events to voice events
        while let Some(event) = rt_rx.recv().await {
            let voice_event = match event {
                RealtimeEvent::Audio(chunk) => VoiceEvent::Audio(chunk),
                RealtimeEvent::ModelTranscript(text) => VoiceEvent::ModelTranscript(text),
                RealtimeEvent::UserTranscript(text) => VoiceEvent::UserTranscript(text),
                RealtimeEvent::TurnEnd => VoiceEvent::TurnEnd,
            };
            if event_tx.send(voice_event).await.is_err() {
                break;
            }
        }
    });
}

#[async_trait]
impl VoiceApi for VoiceService {
    async fn list_voice_providers(&self) -> anyhow::Result<Vec<VoiceProviderInfo>> {
        Ok(self.providers.values().map(|p| p.info.clone()).collect())
    }

    async fn voice_connect(&self, provider_id: &str) -> anyhow::Result<simply_rpc::StreamHandle<simply_voice::VoiceInput, simply_voice::VoiceEvent>> {
        tracing::info!(provider_id, "voice_connect");
        let provider = self.providers.get(provider_id)
            .ok_or_else(|| anyhow::anyhow!("unknown voice provider: {provider_id}"))?;

        let (input_tx, input_rx) = mpsc::channel::<simply_voice::VoiceInput>(64);
        let (event_tx, event_rx) = mpsc::channel::<simply_voice::VoiceEvent>(64);

        // Prefer realtime if available, fall back to STT pipeline
        if let Some(ref realtime) = provider.realtime {
            spawn_realtime_pipeline(Arc::clone(realtime), input_rx, event_tx);
        } else if let Some(ref stt) = provider.stt {
            spawn_stt_pipeline(Arc::clone(stt), input_rx, event_tx);
        } else {
            anyhow::bail!("provider '{provider_id}' has no STT or realtime capability");
        }

        Ok(simply_rpc::StreamHandle::new(input_tx, event_rx))
    }

    async fn synthesize(&self, text: &str, provider_id: &str, voice: &str) -> anyhow::Result<simply_voice::Audio> {
        tracing::info!(provider_id, voice, text_len = text.len(), "synthesize called");
        let provider = self.providers.get(provider_id)
            .ok_or_else(|| anyhow::anyhow!("unknown voice provider: {provider_id}"))?;
        let tts = provider.tts.as_ref()
            .ok_or_else(|| anyhow::anyhow!("provider '{provider_id}' has no TTS capability"))?;
        let result = tts.synthesize(text, voice).await;
        tracing::info!(ok = result.is_ok(), "synthesize done");
        result
    }

    async fn list_voices(&self, provider_id: &str) -> anyhow::Result<Vec<simply_voice::Voice>> {
        let provider = self.providers.get(provider_id)
            .ok_or_else(|| anyhow::anyhow!("unknown voice provider: {provider_id}"))?;
        let tts = provider.tts.as_ref()
            .ok_or_else(|| anyhow::anyhow!("provider '{provider_id}' has no TTS capability"))?;
        tts.voices().await
    }

    async fn voice_disconnect(&self, _session_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

