# BOARD — live operational state

## In-flight

| Status | Date       | Track            | Description |
|--------|------------|------------------|-------------|
| check  | 2026-09-12 | aurora-mcp       | 42bd549c awaits `cargo check -p aurora -p config` (Chris). Changelog bullet held back — CHANGELOG is mid-rename in the working copy.  `[mcp]` section in aurora.toml: Aurora declares the MCP servers she needs (Simply Flux at `http://127.0.0.1:3927/mcp` as the shipped example) and reconciles them into whichever daemon she reaches — embedded or running — through `McpApi` (add / update / start retry). One mechanism for both paths; mcp.toml stays the daemon's working set. Ruled by Chris 2026-09-12 over "config only" and "tray UI". |

Awaiting Chris (runtime trials, no Claude needed): reinstall dmg v2 if the
installed copy predates 986a49dd; `cargo check -p aurora --bins` after
committing the in-flight lumina changes (they carry the lumina terminal
wiring + tool_filter one-liners); trial "Aurora, start a claude session in
<repo>"; spot-check RAG recall after the embeddings swap (re-embed if off).

## Queue

| Status | Date       | Track     | Description |
|--------|------------|-----------|-------------|
| ready  | 2026-09-26 | POCKET-VOICE | Lumina on pocket-tts with custom voices from `data/models/voices/`; image built `--features pocket-tts`, HF cache bind-mounted; `/voice provider` autocomplete filtered by kind. Chain in `.workspaces/pocket-voice`. Needs Chris: push simply-voice, bump the lock, `cargo check`, then land |
| ready  | 2026-09-26 | VA-KYUTAI | simply-voice Kyutai: reset per session, end of turn waits out the ASR delay, close flushes. Chain in `.jj-workspaces/VA-KYUTAI`, rebased, gates green; land it. Then voice-agent drops `KyutaiTurn`'s hold + flush and its fresh-model-per-case test workaround |

## Later / icebox

| Track         | Description |
|---------------|-------------|
| stt-quantized | Expose stt.model_file in AuroraConfig to load a quantized .gguf variant (provider already handles gguf) — shrinks the 1.84 GiB STT download/RAM if kyutai ships one. Not urgent/important per Chris (2026-08-28) |
| duplex-aec    | Acoustic echo cancellation so [audio] echo_gate="duplex" is safe on open speakers (today: headphones recommended) |
| workspace-gc  | `jj workspace forget` + delete .jj-workspaces/{aurora-orb,aurora-voice} (multi-GB target/ each; chains landed); prune unused mobile icon strays under aurora-orb/icons/ (rm is permission-blocked for Claude) |

## Recently merged (2026-08-28, newest first)

| Track                    | Landed |
|--------------------------|--------|
| terminal-standalone      | 5aaa194a self-install + shell/simply-term.zsh + TUI fidelity (SIGWINCH, real size, repaint-on-attach, clean detach, size authority) · d29aa34d crate split: simply-terminal depends on nothing in this repo; adapter lives in simply-terminal-skill |
| terminal-persistence     | 6584a5f2 detached hosts + standalone crate + idle notifications · 013b29b7 simply-term shipped as an Aurora.app sidecar and to ~/.local/bin (`cargo aurora install`) · bc87d0b6 no session state on disk (host answers INFO; socket is only an address; conservative pruning) |
| aurora-overlay           | bc90eef2 — aurora__show_panel: markdown/image panel window beside the orb (vibrancy-matched, unfocused, hide-not-destroy, XSS-safe dep-free renderer, images data-URI'd in Rust); CLI prints instead; system prompt now routes markdown to the panel |
| terminal-discovery       | 0d3df2c9 — layered binary resolution (PATH walk → install dirs → login-shell PATH, cached) fixes claude/pi "not installed" in Finder-launched Aurora.app; PTY child gets a usable PATH too. Verified under simulated launchd env |
| simply-terminal          | 4c1ff7b3 crate (pure-Rust PTY TerminalSkill + simply-term attach client) + c5acbf6b aurora wiring; lumina wiring rides with Chris's in-flight changes. pi-verified: all checks PASS, zero fixes (simply-term at target/debug/) |
| embeddings-pure-rust     | 1ce08bf9 — candle bge-small replaces fastembed; onnxruntime at zero lockfile entries. Runtime parity unverified: spot-check RAG, re-embed if off |
| mcp-retry-race           | ff356c0a — generation-guarded retry-task ownership; bonus: retry tokens now self-register so cancel_retry works |
| orb-context-reset        | 6f895479 — reset_session serialized with turns; /reset CLI; "New Conversation" tray item |
| tool_filter-fallout      | 0cfb6fd3 — telegram-bot initializer fix (lumina one-liners ride with Chris's in-flight changes) |
| aurora-dmg (v2)          | 986a49dd — real ad-hoc+runtime signing + audio-input entitlement (v1 install was mic-blocked); bundled logs → ~/.local/share/noema/logs/aurora-orb.log; Aurora icon; ripple redesign. CLOSED — reinstall from new dmg is the only user action |
| demeanor + loading design| 621a5c38 — no ask-what-to-do prompt; loading = diffuse tri-hue + conic halo condensing into idle |
| latency-tuning           | e815526e — ollama keep_alive 30m; session tool_filter + [agent] tools; TTS voice warmup. Also 15a7eba6 model-list warning hygiene |
| orb-pipeline-integration | b35679db — VoicePipeline lib; orb = full Aurora; utterance queuing; echo_gate gate|duplex + barge-in; TTS sanitizer. AEC → icebox |
| orb-frosted-glass        | 271d1097 — real vibrancy dot, teal/magenta/orange states |
| aurora-audio-devices     | e8cb546a — device config/flags/commands, pipelined TTS + timing logs, EOT tuning, Ctrl-C fix |
| aurora-voice             | 52a215ea + 1ced5be6 — audition/design/preview/record/import; env-var-free HF fetch. Aurora's voice: eponine → custom aurora.safetensors |
| aurora-orb (shell)       | c3bbd563→3b656220 — Tauri dot, hover, position memory, tray |
| aurora core fixes        | 0e5b391d pure-Rust sentencepiece (protobuf abort); 1991f454 MCP auto-disable; f1478d23 log filter |
| aurora-trials foundation | 2d2eac11 aurora CLI; 5b9e0677 pocket-tts + Kyutai STT providers |
