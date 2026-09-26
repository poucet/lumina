#### Lumina speaks with your own pocket-tts voice (`POCKET-VOICE`) · 2026-09-26 13:40 · strnxxko

- **Custom voices.** Drop `<name>.safetensors` (or `<name>.wav`) into
  `data/models/voices/` and `/voice set-voice` offers `<name>` for the
  `pocket-tts` provider, beside the stock voices. Needs the simply-voice
  commit that adds `PocketTtsProvider::with_voice_dir` (lock bump pending).
- **The server image ships pocket-tts** (`--features pocket-tts`), running on
  the CPU. Its model cache is bind-mounted at `data/hf-cache/`, so a rebuild
  no longer re-downloads it.
- **`/voice provider` autocomplete follows `kind`**: with `tts` filled in it
  lists only TTS providers, with `stt` only STT ones.
- **`POCKET_TTS_VOICE` is gone.** The default pocket-tts voice is `alba`; pick
  another with `/voice set-voice`.
