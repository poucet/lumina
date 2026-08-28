# BOARD — live operational state

## In-flight

| Status | Date       | Track                | Description |
|--------|------------|----------------------|-------------|
| 🔄     | 2026-08-28 | simply-terminal      | Subagent, fence simply-terminal/** (new crate) + root Cargo.toml + lumina wiring (+ config/src/paths.rs additive helper): pure-Rust PTY TerminalSkill (portable-pty, NO tmux) — agent drives a claude/pi session while the human shares it via `simply-term attach <name>` (per-session unix socket, raw-mode bridge, SIGWINCH resize); start/send/read/list/stop, Terminal.app auto-attach, ANSI-stripped reads. Sessions die with host process (accepted vs tmux). Optional feature for lumina; aurora wiring follows as a separate pass |
| 🔄     | 2026-08-28 | mcp-retry-race       | Subagent, fence simply-core/src/mcp/registry.rs: owner-checked cleanup (generation/token identity guard) so a cancelled-and-replaced retry task can't clobber its successor's token/status |
| 🔄     | 2026-08-28 | embeddings-pure-rust | Subagent, fence simply-core/llm/** + simply-daemon/{builder.rs,Cargo.toml}: candle BGE-small embedding provider replacing fastembed/onnxruntime (last C++ dylib out). Constraint: reproduce fastembed's bge pipeline (CLS pooling + L2 norm, prefixes) so stored 384-dim vectors stay valid; else re-embed |

## Queue

(empty)

## Later / icebox

| Track         | Description |
|---------------|-------------|
| stt-quantized | Expose stt.model_file in AuroraConfig to load a quantized .gguf variant (provider already handles gguf) — shrinks the 1.84 GiB STT download/RAM if kyutai ships one. Not urgent/important per Chris (2026-08-28) |
| duplex-aec    | Acoustic echo cancellation so [audio] echo_gate="duplex" is safe on open speakers (today: headphones recommended) |
| workspace-gc  | `jj workspace forget` + delete .jj-workspaces/{aurora-orb,aurora-voice} (multi-GB target/ each; chains landed); prune unused mobile icon strays under aurora-orb/icons/ (rm is permission-blocked for Claude) |

## Recently merged (2026-08-28, newest first)

| Track                    | Landed |
|--------------------------|--------|
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
