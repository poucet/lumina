# Voice Architecture

**Status:** Implemented
**Version:** 1.0
**Parent:** [ARCHITECTURE.md](ARCHITECTURE.md)

---

## Overview

STT and TTS live in simply-daemon via the `simply-voice` crate. Clients convert platform audio to a common format and stream it to the daemon. The daemon runs the full pipeline.

```
  Noema (CPAL)         Lumina (songbird)       Future: WebRTC
  mic -> common fmt    Discord -> common fmt    RTC -> common fmt
       |                    |                       |
       +--------------------+-----------------------+
                            v
                    simply-daemon
                    +- simply-voice (providers)
                    |   +- VoxtralProvider (STT + TTS)
                    |   +- WhisperProvider (STT)
                    |   +- ElevenLabsProvider (TTS)
                    |   +- GeminiRealtimeProvider
                    |
                    +- voice pipeline: VAD -> STT -> Agent -> TTS
                            |
                            v
                    common fmt audio back to client
```

---

## Provider Traits

Defined in `simply-voice/src/provider/`:

- **`SttProvider`** — streaming speech-to-text, returns transcription events
- **`TtsProvider`** — text-to-speech, returns audio bytes
- **`RealtimeProvider`** — bidirectional audio-in/audio-out (Gemini)

Implementations:
- **Voxtral** — STT + TTS via local MLX voice server (Apple Silicon) or Docker/vLLM (NVIDIA)
- **Whisper** — STT via OpenAI-compatible API
- **ElevenLabs** — TTS with voice selection
- **Gemini Realtime** — bidirectional streaming (audio -> audio)

## Daemon Integration

- **STT stream:** `StreamHandle<VoiceInput, VoiceEvent>` via bidirectional WebSocket
- **TTS endpoint:** `POST /voice/tts`
- **Provider registration:** voice providers configured in `settings.toml` with plaintext API keys
- **500 error retry:** protocol-level retry for transient provider failures
- **Voice API hidden from LLM tools** — voice is infrastructure, not a tool the agent calls

## Client Implementations

### Noema (Desktop)
- CPAL mic capture -> daemon STT stream -> transcript into chat
- Auto-TTS via CPAL audio output
- Decoupled STT/TTS provider selection
- Voice provider + voice dropdown UI

### Lumina (Discord)
- Songbird with DAVE encryption
- Commands: `/voice transcribe`, `/voice listen`, `/voice say`, `/voice leave`, `/voice list`, `/voice status`
- `/voice provider`, `/voice set-voice` with autocomplete
- Config persistence per guild
- TTS fallback to text when TTS fails
- Random voice selection when none configured
- Transcript routing to voice channel text chat
- `#[command_group]` macro with autocomplete support
- WAV-in-memory for songbird (no temp files)

## Common Audio Format

Clients and daemon agree on a common streaming format (PCM, specific sample rate/encoding). Clients are responsible only for converting their platform audio to/from this format.

---

## Extensibility

The voice API is generic — any process that produces/consumes the common audio format can stream to the daemon. Future audio sources (WebRTC, Google Meet, phone bridge) don't need changes to the daemon or existing clients.

WebRTC integration is a future platform in [ops/ROADMAP.md](../ops/ROADMAP.md) (WebRTC / simply-chris.ai/meet).
