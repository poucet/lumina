#### Debug lumina's voice from Discord (`DEBUG`) · 2026-09-26 13:54 · wlurummp

- **`/debug voice`** (owner only, ephemeral): one line per voice stage for this guild.
  - Stages: `rx audio`, `stt final`, `llm turn`, `tts synth`, `playback`.
  - Each shows the age of its last event and its count.
  - `⚠️ in flight Ns`: work started and has not finished. A stage stuck here is a dead-lock.
  - `dropped N` on `rx audio`: audio lumina threw away because STT was not keeping up. Counts moving while nothing comes out is a live-lock.
  - Header: session mode, STT provider, TTS provider/voice, and `sessions lock held` when that lock is taken.
- **`/debug runtime`**: uptime, tokio workers, alive tasks, global queue depth, and each worker's busy share over 1 s. A worker at 100% is spinning.
- **`/debug logs [lines]`**: the last lines of today's `lumina.log.*` as a file. Default 100, max 2000. Reads at most the last 1 MiB.
- **Watchdog**: when a stage stays in flight longer than `watchdog_secs`, lumina DMs the owner the `/debug voice` snapshot.
  - Once per stuck episode. It re-arms when the stage clears.
  - `[voice] watchdog_secs` in `lumina.toml`, default 30.
  - `stt final` alerts only when `rx audio` is also dropping. An empty transcription leaves STT looking in flight until the next speech.
  - Off when `discord.owner_id` is unset or 0.
- **Cost**: two relaxed atomics per 20 ms voice tick. Nothing else on the audio path.
- **Blind spots**: the watchdog and `/debug` both go through the Discord gateway and HTTP client. A hung gateway, a starved runtime, or a dead process means neither answers. Then it's the log file on the host.
