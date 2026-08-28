# BOARD — live operational state

## Quota handoff (2026-08-28 — Claude Code quota near exhaustion for days)

State for the next session (human, pi, or future Claude):
1. simply-terminal LANDED (4c1ff7b3) + aurora wiring (c5acbf6b, compile-unverified 3-liner) + lumina wiring riding uncommitted with Chris's in-flight lumina changes. Chris: `cargo check -p aurora --bins` verifies the wiring; `cargo build -p simply-terminal` puts `simply-term` next to the aurora binary.
2. mcp-retry-race + embeddings-pure-rust subagents were mid-flight at handoff — if their work sits uncommitted on disk (registry.rs rework; new candle embed provider in simply-core/llm), validate with `cargo check -p simply-core` / `-p llm -p simply-daemon` and commit per their BOARD rows; if absent, the rows below are ready-to-run specs. Both are pi-able: tight file scope, spec in the row, acceptance = cargo check clean (write tests first per flux supervisor protocol if delegating).
3. Recovery per docs/OPERATIONAL.md: read it + this board; `jj log` shows every landed chain.

## In-flight

| Status | Date       | Track                | Description |
|--------|------------|----------------------|-------------|
| 🔄     | 2026-08-28 | simply-terminal      | Subagent, fence simply-terminal/** (new crate) + root Cargo.toml + lumina wiring (+ config/src/paths.rs additive helper): pure-Rust PTY TerminalSkill (portable-pty, NO tmux) — agent drives a claude/pi session while the human shares it via `simply-term attach <name>` (per-session unix socket, raw-mode bridge, SIGWINCH resize); start/send/read/list/stop, Terminal.app auto-attach, ANSI-stripped reads. Sessions die with host process (accepted vs tmux). Optional feature for lumina; aurora wiring follows as a separate pass |
| ✅     | 2026-08-28 | mcp-retry-race       | LANDED (ff356c0a): generation-guarded retry-task ownership, all cleanup sites owner-checked incl. per-iteration status and success path; bonus fix — retry tokens now self-register so cancel_retry actually reaches them |
| ✅     | 2026-08-28 | embeddings-pure-rust | LANDED (1ce08bf9): candle BERT bge-small replaces fastembed; fastembed/ort/onnxruntime have ZERO lockfile entries — no C++ dylibs left in the process. Runtime caveat: output parity with fastembed not runtime-verified — spot-check RAG recall; re-embed corpus if similarity degrades |
| ✅     | 2026-08-28 | simply-terminal-verify | VERIFIED by pi.dev (qwen3.6 local, headless, zero Claude quota): all four commands PASS, zero fixes needed, tree untouched (diff-verified). simply-term built at target/debug/. simply-terminal is DONE — remaining verification is Chris's runtime trial ("Aurora, start a claude session in <repo>") |

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
