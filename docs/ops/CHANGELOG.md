# Changelog

> What landed, per landing, newest first. Open work: [ROADMAP.md](ROADMAP.md) · live state: [BOARD.md](BOARD.md). What belongs here: @AGENTS.md §2.

## 1.0 — in progress since ~February 2026

Version 1.0 unifies Noema (Rust desktop) and Lumina (Python Discord bot) into a single Rust workspace with a shared daemon hub. Everything talks to `simply-daemon`.

---

## Aurora — local voice agent + floating orb (2026-08-27/28)

New always-on macOS voice agent on the shared daemon, fully offline-capable
voice, shipped as an installable app in a two-day arc.

- **`aurora` crate** — `VoicePipeline` library (mic → Kyutai STT → daemon
  session → pocket-tts → speaker) with two thin frontends: a trial CLI
  (stderr word trace, `/model /devices /reset` escape hatches that bypass the
  LLM) and the orb
- **`aurora-orb` crate** — Tauri v2 floating dot: frosted-glass vibrancy
  (teal/magenta/orange states: ripple listening, comet thinking, waveform
  speaking; hearing overlay + queued badge), NSStatusWindowLevel all-Spaces
  hover, tray (model switcher, device pickers, New Conversation, shape),
  bundled as ad-hoc-signed `Aurora.app` + dmg with mic entitlement
- **Local voice in `simply-voice`** — `PocketTtsProvider` (candle port,
  Metal, voice cloning: wav → .safetensors embedding) and `KyutaiSttProvider`
  (1B streaming STT, word partials, model-internal end-of-turn VAD);
  C++ sentencepiece replaced by a pure-Rust decoder after a protobuf symbol
  clash with onnxruntime aborted at startup
- **`aurora-voice` CLI** — voice design: audition stock voices, ElevenLabs
  design → local clone, record/preview/import; Aurora runs on a custom
  `aurora.safetensors` voice
- **Conversation model** — utterance queuing while the agent thinks;
  `echo_gate = gate|duplex` with barge-in; sentence-pipelined TTS (first
  audio after first sentence) + per-turn timing logs; spoken-output prompt
  hardening + TTS markdown sanitizer
- **Daemon additions** — `CreateSessionOptions.tool_filter` allow-list
  (local-model prompt slimming), Ollama `keep_alive` (kills recurring cold
  loads), MCP auto-disable after 8 failed connects, model-list warning
  hygiene
- **Config over env** — everything in `aurora.toml` ([stt], [tts], [audio],
  [orb], [agent]); HF weights fetched via the standard token file, no env
  vars anywhere

---

## OAuth persistence + Lumina deploy (2026-06-03/04 · mpnzuuul → xlkuoplq)

- **OAuth tokens survive restarts** — mirrored to `data/tokens.json` (0600), refresh token captured, expiry as wall-clock seconds; expired tokens refresh on read (lyxrqyks, nqxrpwpo)
- **Lumina server deploy** — Docker + host nginx + web setup wizard (multiple LLM keys, live model fetch, resume, file logs, reset); post-receive auto-deploy hook; Caddyfile removed
- **Lumina** — `/config` slash command (status channel + AI category), Discord user identity threaded through `/tool call`, `/auth` removed in favour of the real OAuth route (`/auth/mcp/google`), survives Discord auth failure; gdocs `/google import` autocomplete substring-matches titles server-side

---

## Events & Intents — Stage 1 (2026-06-01 · ntppqpqq → yrkomnpq)

- **Event bus** in `simply-core` — pub/sub with typed event payloads
- **Timer event source** — cron, interval, one-shot, fuzzy time expressions
- **Intent documents** in UCM with `type: intent` frontmatter
- **Telegram bot** skeleton crate (xnwxrvxo)

---

## Vault-Backed Markdown (2026-05-04 · zxzwmkwk → oulllowr)

Human-authored `document::*` bodies become normal Markdown files in a configurable vault; SQLite stays canonical for identity, relations, access policy, runtime state, embeddings and indexes. **Design:** [VAULT_BACKED_MARKDOWN.md](../designs/VAULT_BACKED_MARKDOWN.md).

- **Storage** — SQLite migration runner (`schema_migrations`), configurable `vault_root`, `vault_files` / `vault_conflicts` projection tables
- **Markdown** — frontmatter parser + serializer (opt-in identity fields `id`/`kind`/`origin`/`privacy`, user metadata preserved), asset-reference extraction
- **Reconciliation** — scanner-first: same-id and body-hash moves, missing files (recoverable status), duplicate/changed-id conflicts, unmanaged files; projection-first or frontmatter identity modes; `.noema/vault-index.json` sidecar snapshot
- **Read/write path** — export of `document::tabbed` trees and flat documents; content resolver reads vault files and falls back to `content_blocks`; atomic temp-file writes with stale-hash checks; external edits become new `content_blocks` snapshots and re-embed; `entity_assets` rebuilt from references
- **Watcher + conflicts** — debounced polling watcher; conflict-resolution API (restore id / fork / accept path / unmanage / bind) with admin and Noema `VaultControls`
- **Hardening** — centralized entity access policy and delete lifecycle, `structure::contained_in` invariants, batched entity summaries, SQLite WAL with split read/write handles

---

## UCM Unification (2026-04-21 · wkllqyzy → xuzlnzxz)

Documents, tabs and revisions collapse onto `entities` + `entity_relations` + `content_blocks`; only the import skill knows what a Google Doc becomes. **Design:** [UNIFIED_CONTENT_MODEL.md](../designs/UNIFIED_CONTENT_MODEL.md).

- **Schema** — `entities.content_block_id` + `origin`, `entity_relations.position`, `entity_assets`; namespaced `EntityType` (`document::*`, `system::*`) and `RelationType` (`structure::contained_in`, `reference::to`, …); slug/is_archived dropped
- **Coordinator** — generic entity + content + relation primitives (create/update content, ordered children, `move_entity`, cascade delete, asset GC)
- **`EntityApi`** — graph-edge API with lazy content and `EntitySummary` capabilities, batch `get_entities`; replaces `DocumentApi` — `DocumentApi`, `DocumentStore`, `document_resolver` and the `documents`/`document_tabs`/`document_revisions` tables deleted; `DocumentRef` → `EntityRef` over `EntityResolver`
- **UI** — admin entity-first pages (`EntitiesPage`, type-change support), shared `@simply/entity-ui` package; Noema documents panel + read-only view
- **RAG pivot** — `VectorChunk` keyed on `content_block_id` with `entity_kind` and `EntityFilter`; Lumina dedupes hits by block
- **RPC** — snake_case wire keys

---

## Unified Frontend & Tool Architecture (2026-04-18)

Major architecture overhaul consolidating on a single frontend and unified tool dispatch.

- **Admin UI** — Astro + Svelte 5 web UI served by daemon: chat with streaming, settings, MCP management, Google Docs import
- **Transport layer** — `HttpTransport` with REST for RPC + WebSocket for events, auto-detects base URL
- **`simply-daemon-api` subcrate** — extracted API traits, `ToolProvider`, `Skill`, `RemoteDaemon`, `SkillCallContext` into lightweight shared crate
- **`ToolProvider` trait** — unified abstraction for MCP servers, WS-connected clients, and embedded skills (all speak rmcp types)
- **`ToolRegistry`** — replaces `CompositeToolService`; holds `Vec<Arc<dyn ToolProvider>>`, dispatches with `RequestContext` carrying user identity + OAuth tokens
- **Concrete providers** — `McpToolProvider` (shared/on-demand), `WsToolProvider` (reverse RPC), `EmbeddedToolProvider` (wraps Skill), `ClientToolProvider` (wraps ToolCallHandler)
- **Skill system** — skills take `Arc<dyn Daemon>`, declare `OAuthRequirement`s, daemon handles auth and injects tokens
- **`RequestContext.tokens`** — OAuth tokens flow through the RPC context, no explicit params
- **GDocsSkill** — Google Docs as a skill registered by Lumina (not hardcoded in daemon)
- **Chat store** — Svelte 5 runes with WS event subscription, streaming message assembly
- **CORS** — CorsLayer for cross-origin dev (Astro dev server → daemon)

---

## Content & RAG (2026-04-15)

Embedding pipeline, vector search, and Google Docs import.

- **Embedding providers** — local ONNX (bge-small-en-v1.5), Ollama, Mistral, Gemini, Voyage
- **sqlite-vec VectorStore** — chunks table + vec virtual table
- **Embedding queue** — background worker with debounce, content hash dedup, flush signals
- **SearchApi** — embed query → vector search → return hits with doc metadata
- **Lumina auto-RAG** — query from last N messages, inject relevant chunks into system prompt
- **Google Docs import** — `mcp-gdocs` skill with per-user OAuth, tab tree with topological sort, image assets
- **Per-user MCP OAuth** — `TransientTokenStore`, OAuth flow on daemon's main port, token injection

---

## Docs Cleanup (2026-04-06)

Consolidated and cleaned up v1.0 documentation after completing Foundation, Lumina, and Voice phases.

- Retired `TODO.md` (manual test checklist) and `JOURNAL.md` (testing notes)
- Removed `phases/` directory — all 11 files across 6 subdirectories (foundation, lumina, voice, content, events, rtc)
- Consolidated into single `v1.0/TASKS.md` with next phase tasks across 4 workstreams
- Rewrote `GOAL.md` and `ROADMAP.md` to reflect current state
- Moved [VOICE.md](../designs/VOICE.md) from proposals to designs (implemented)
- Moved [AGENTIC.md](../designs/proposals/AGENTIC.md) and [ACTIONS.md](../designs/proposals/ACTIONS.md) into proposals (not yet built)
- Folded [TOOL_APPROVAL.md](../designs/proposals/TOOL_APPROVAL.md) and [UCM_SERVICE.md](../designs/proposals/UCM_SERVICE.md) into task plan
- Fleshed out next phase: Content & RAG, Events & Intents, RTC (Google Meet), Multi-user & OAuth

---

## Voice Phase

### Stage 1 — Voice Library

New `simply-voice` crate with provider abstraction.

- **`SttProvider` trait** — streaming speech-to-text
- **`TtsProvider` trait** — text-to-speech
- **`RealtimeProvider` trait** — bidirectional audio streaming
- **Voxtral** — STT + TTS via local MLX voice server (Apple Silicon) or Docker/vLLM
- **Whisper** — STT via OpenAI-compatible API
- **ElevenLabs** — TTS with voice selection and autocomplete
- **Gemini Realtime** — bidirectional audio streaming
- **VAD module** — voice activity detection
- **Audio types** — `Audio` with format metadata, PCM conversion utilities

### Stage 2 — Daemon Integration

- **STT stream** — `StreamHandle<VoiceInput, VoiceEvent>` via bidirectional WebSocket
- **TTS endpoint** — `POST /voice/tts`
- **`ServiceRouter`** — replaced `RestDispatcher`, cleaner routing
- **`RemoteXxxApi` structs** — replaced `impl_remote_xxx!` macros with generated client types
- **`RpcConnection` trait** — unified client connection abstraction
- **Voice provider registration** from `settings.toml`
- **Plaintext API keys** in settings (no env var requirement)
- **500 error retry** at protocol level for transient provider failures
- **Voice API hidden from LLM tools** — infrastructure, not agent-callable

### Stage 3 — Desktop Voice (Noema)

- CPAL mic capture -> daemon STT stream -> transcript into chat
- Auto-TTS via CPAL audio output (native, not webview Web Audio)
- Decoupled STT/TTS provider selection
- Voice provider + voice dropdown UI in settings

### Stage 4 — Discord Voice (Lumina)

See Lumina Stage 6 below — implemented as part of Lumina's voice commands.

---

## Lumina Phase

### Stage 1 — Discord Bot Crate

- `lumina` crate with serenity, connects to daemon via `RemoteDaemon`
- `#[slash_command]` + `#[command_group]` proc macros
- Guild-specific command registration, `.sync` owner command
- `LuminaContext` passed to all handlers (daemon + config access)

### Stage 2 — LLM Chat

- `/chat new` creates dedicated AI channels under configured category
- `on_message` listener responds in AI channels + @mentions
- Discord message history loaded as LLM context (configurable limit, paginated API)
- Streaming responses with progressive message edits
- `/chat pause`, `/chat resume` — per-channel toggle via channel topic tags
- `/chat model` — per-channel model selection with autocomplete from daemon
- Multimodal responses — images and audio as Discord attachments
- System prompt with conversation context instructions

### Stage 2.5 — Architecture Refactor

- `ToolAgent` + `SessionManager` + `AgentStreamSink` architecture
- Unified session management (ephemeral + persistent)
- Clean API separation between daemon and clients

### Stage 3 — Discord MCP Service

- Lumina registers as ephemeral MCP service with daemon on connect
- **15 Discord tools** via rmcp `#[tool]` macros: `list_channels`, `send_message`, `get_channel_history`, `search_messages`, `list_guilds`, `get_guild_info`, `list_members`, `get_member_info`, `list_roles`, `manage_roles`, `create_channel`, `manage_channel`, `pin_message`, `add_reaction`, `create_thread`
- `/tool call` — modal form generated from tool JSON schema, supports all content types
- `/tool list` — paginated embed of all registered MCP tools
- MCP instructions populated with guild/channel map (auto-refreshes on Discord events)
- Tool results with structured formatting and paginated embeds
- Daemon-side: `list_all_tools` + `call_tool_direct` using rmcp types natively
- Tool parameter schemas generated from RPC macro via `JsonSchema`
- Prefixed tool names: `discord.list_channels`, etc.

### Stage 6 — Discord Voice

- **Songbird** integration with DAVE encryption
- `/voice transcribe` — join channel, transcribe speech to text
- `/voice listen` — STT -> LLM session -> TTS -> play response in channel
- `/voice say` — text-to-speech playback in voice channel
- `/voice leave` — disconnect from voice channel
- `/voice list` — list available voices for current TTS provider
- `/voice status` — show current voice state
- `/voice provider` — switch STT/TTS provider with autocomplete
- `/voice set-voice` — select TTS voice with autocomplete
- Config persistence per guild (provider, voice selections)
- TTS fallback to text when synthesis fails
- Random voice selection when none configured
- Transcript routing to voice channel text chat
- WAV-in-memory for songbird (no temp files)
- Non-blocking audio receiver to prevent deadlocks

---

## Foundation Phase

### Stage 1 — Workspace Restructure

Renamed the workspace from `noema-*` to `simply-*` and established the crate boundaries.

- Renamed `noema-core` to `simply-core`, `noema-audio` to `simply-audio`
- Created `simply-daemon` crate with `DaemonApi` trait
- Merged `noema-mcp-core` into `simply-daemon/src/mcp/`
- Renamed `noema-desktop` to `noema`

### Stage 2 — Daemon Hub

Built `simply-daemon` as the central hub that all clients connect to.

- **`EmbeddedDaemon`** — in-process implementation for Noema desktop
- **`RemoteDaemon`** — WS + REST client for remote connections
- **WebSocket server** — rich client sessions with streaming
- **REST server** — asset management, health, admin
- **`simply-rpc` crate** — `#[rpc_service]` proc macro generates WS dispatch, REST routing metadata, and client code from trait definitions
- **Split DaemonApi** into focused traits: `SessionApi`, `ConversationApi`, `AssetApi`, `McpApi`, `OAuthApi`, `ModelApi`, `VoiceApi`, `CoreApi`
- **Smart discovery** — `connect_or_host()` tries remote, falls back to embedded
- **Auto-reconnect** — WS client with exponential backoff (100ms-30s)
- **Decoupled Noema** — all Noema Rust code imports from `simply-daemon` only, no `simply-core` dependency
- **`SessionManager`** — pluggable storage hooks, ephemeral and persistent sessions
- **`ToolService` trait** — `DaemonToolService` (REST methods as tools), later unified via `ToolRegistry` + `ToolProvider`

### Stage 3 — REST-First Transport

Upgraded the RPC framework to be REST-native with full HTTP semantics.

- **REST annotations** — `#[rpc(get = "/path")]`, `#[rpc(post = "/path")]`, etc.
- **`RestDispatcher`** — matchit-based URL routing with path params
- **`RouteMeta`** — `RouteKind::Rest(HttpMethod)` | `RouteKind::Stream`, cache control, binary response flags
- **Axum migration** — replaced hand-rolled hyper server
- **`BinaryResponse`** — typed binary returns with mime type and caching headers
- **`BinaryUpload`** — raw HTTP body upload with Content-Type
- **Single port** — merged WS and REST onto one axum server
- **Client codegen** — `impl_remote_xxx!` macros generate REST HTTP client code
- **Admin page** — active connections, sessions, models, kill endpoint
- **90 tests** — metadata, dispatch, round-trip, raw HTTP/WS, binary transfer

### Stage 4 — Service Extraction

Broke the monolithic `EmbeddedDaemon` into focused services.

- Extracted: `McpService`, `ModelService`, `AssetService`, `VoiceService`, `CoreService`
- `EmbeddedDaemon` delegates to services
- Services individually registered with `RestDispatcher`

### Stage F — Typed Content Dispatch

Added type-aware content transformation for MCP tool results.

- **`IntoContent` trait** — types declare how they become `ContentPart` (JSON or binary)
- **`FromContent` trait** — inverse direction for tool inputs (MCP image/audio -> `BinaryUpload`)
- **`#[derive(IntoContent)]`** — proc macro for all API return types
- **`rest_dispatch_as_content`** — RPC macro generates typed dispatch for tool invocation

---

## Infrastructure

### Local MLX Voice Server
- Python server for Apple Silicon TTS/STT via Voxtral
- Docker/vLLM alternative for NVIDIA
- Auto-detect in daemon startup script

### Developer Tooling
- `bin/daemon` script with start/stop/restart/status
- `bin/lumina` and `bin/noema` launcher scripts (run, split modes)
- Integration test harness with hurl + interactive mode
- File logging with rotation for daemon and Lumina
- `NOEMA_DATA_DIR` env override for test isolation

---

## Pre-1.0 Work (0.2 -> 1.0 transition)

Before the v1.0 restructure, significant work was done on Noema 0.2:

- Google Docs MCP server (`noema-mcp-gdocs`) with OAuth, import, and rendering
- Document panel with markdown rendering
- Model favorites and search/filter in selector
- Parallel model responses (SpanSet)
- Fork/regenerate/edit conversation features
- Document references in chat (`DocumentRef`, `DocumentResolver`)
- HTML-to-markdown paste
- Conversation privacy flag
- `StorageCoordinator` for coordinated multi-store operations
- Entity layer with `EntityStore`, relations, temporal queries
- Collections with items, fields, tags, schema hints
- Cross-references with backlinks
- UCM storage migration (Phases 1-3 of 0.2 roadmap)

---

## Highlights

- **Workspace unification** — single Rust workspace: `simply-core`, `simply-daemon`, `simply-rpc`, `simply-voice`, `noema`, `lumina`, `config`
- **Daemon hub** — `simply-daemon` with 8 API traits, axum server (REST + WS on single port), service extraction
- **RPC framework** — `simply-rpc` with `#[rpc_service]` proc macro, REST annotations, bidirectional streams, binary transfer
- **Discord bot** — full Lumina port: chat, 15 MCP tools, voice with DAVE encryption
- **Voice pipeline** — `simply-voice` crate with 4 providers, desktop + Discord voice, local MLX server

---

## Design Documents

- [designs/ARCHITECTURE.md](../designs/ARCHITECTURE.md) — platform architecture
- [designs/CORE_SERVICE.md](../designs/CORE_SERVICE.md) — daemon protocol (WS, REST, MCP)
- [designs/VOICE.md](../designs/VOICE.md) — voice pipeline architecture
- [designs/UNIFIED_CONTENT_MODEL.md](../designs/UNIFIED_CONTENT_MODEL.md) — UCM storage spec
- [designs/proposals/](../designs/proposals/) — proposals for events, actions, tool approval
