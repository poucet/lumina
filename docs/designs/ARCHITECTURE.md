# Simply Platform Architecture

**Status:** Current
**Version:** 1.0

---

## Vision

Simply is a unified AI platform where Noema (desktop), Lumina (Discord), and future clients share a common daemon for LLM orchestration, voice, storage, and reactive automation.

### Guiding Principles

1. **Local-first**: Data lives on your machine. Cloud is opt-in.
2. **Content is immutable**: Text and assets are stored once, referenced many times.
3. **Structure is mutable**: Conversations, documents, views can reorganize without moving content.
4. **Everything is addressable**: @mention any entity. Fork, reference, organize.
5. **Code provides capabilities, content provides configuration**: Event sources and action handlers are code. All routing, wiring, and workflow is content (UCM documents).

---

## Platform Architecture

```
  Admin UI (Svelte)  ──http/ws──┐
  Lumina (Discord)   ──ws───────┤   Integration Services
  Noema (Tauri)      ──embed────┤   ┌──────────────┐
                                ▼   │ MCP servers   │
                       simply-daemon│ (github, etc) │
  ┌─────────────────────────────────└───────┬───────┘
  │ ToolRegistry                            │
  │  ┌─ McpToolProvider ←── MCP servers (rmcp)
  │  ├─ WsToolProvider  ←── WS clients (reverse RPC)
  │  ├─ EmbeddedToolProvider ←── Skills (in-process)
  │  └─ DaemonToolService ←── REST APIs as tools
  │
  │ Sessions → ToolAgent → LLM providers
  │ Storage (SQLite + FS blobs + sqlite-vec)
  │ Embedding & RAG pipeline
  │ REST + WS server + admin UI
  └─────────────────────────────────────────────
      ↓         ↓         ↓
  LLM APIs   MCP servers   Voice
```

**Key distinctions:**

- **simply-daemon** is the hub. HTTP REST + WS on a single port. Admin UI served at `/admin/`.
- **simply-daemon-api** defines API traits (`Daemon`, `ToolProvider`, `Skill`) and types shared by daemon, skills, and clients.
- **simply-core** is an internal library. LLM providers, MCP, agent orchestration, storage traits.
- **ToolRegistry** is the unified tool dispatcher. All tool sources (MCP servers, WS-connected clients, embedded skills) implement `ToolProvider` and are registered identically. The daemon doesn't care about transport.
- **Rich clients** (Lumina, admin UI) connect via WS + REST. Noema can embed the daemon in-process.
- **Skills** implement `simply-daemon-api::Skill`, take `Arc<dyn Daemon>` for daemon API access, and declare `OAuthRequirement`s. The daemon handles auth flows and injects tokens via `RequestContext`.

---

## Workspace Structure

```
lumina/                    # workspace root
├── simply-core/           # Internal library: LLM + MCP + agent + storage traits + event bus
│   └── llm/               # Multi-provider LLM client (Claude, OpenAI, Gemini, Mistral, Ollama)
├── simply-daemon/         # The hub: services, storage, REST/WS server
│   ├── api/               # API traits + types (shared by daemon, skills, clients); ts/ = @simply/client
│   ├── src/
│   │   ├── builder.rs     # DaemonBuilder — wires all services
│   │   ├── embedded.rs    # EmbeddedDaemon (in-process impl)
│   │   ├── services/      # registry.rs, providers.rs, tools.rs, model, asset, entity, voice, search...
│   │   ├── mcp/           # MCP service, OAuth, config
│   │   └── net/           # REST + WS server, admin API, auth
│   └── admin/             # Astro + Svelte 5 admin UI
├── simply-rpc/            # Transport-agnostic RPC framework, #[rpc_service] macro
├── simply-voice/          # Voice providers: STT, TTS, Realtime, VAD (Pocket TTS + Kyutai STT local, Voxtral, Whisper, ElevenLabs, Gemini)
├── aurora/                # Voice-agent pipeline lib + trial CLI + aurora-voice design tool
├── aurora-orb/            # macOS floating-dot Tauri app (ships as Aurora.app/dmg)
├── simply-terminal/       # PTY host + simply-term attach client (depends on nothing in this repo)
├── simply-terminal-skill/ # Daemon adapter: agent-driven claude/pi sessions shared with the human via PTY
├── lumina/                # Discord bot (serenity + songbird)
├── telegram-bot/          # Telegram bot (long polling + daemon sessions)
├── mcp-gdocs/             # Google Docs MCP server + GDocsSkill
├── noema/                 # Tauri desktop shell (src-tauri/) + Svelte UI (ui/)
├── commands/              # Command framework with completion
├── config/                # Settings, encrypted credentials
└── xtask/                 # cargo xtask (e.g. `cargo aurora install`)
```

---

## Unified Content Model (UCM)

Three-layer storage foundation. See [UNIFIED_CONTENT_MODEL.md](UNIFIED_CONTENT_MODEL.md) for the full specification.

```
┌─────────────────────────────────────────────────────────────┐
│                    ADDRESSABLE LAYER                        │
│  Unified identity, naming, and relationships                │
│  entities + entity_relations                                │
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                    STRUCTURE LAYER                          │
│  Conversations  │  Documents (tabs, revisions)  │ Collections│
└─────────────────────────────────────────────────────────────┘
                              │
┌─────────────────────────────────────────────────────────────┐
│                     CONTENT LAYER                           │
│  Immutable content (text blocks) + assets/blobs (CAS)      │
└─────────────────────────────────────────────────────────────┘
```

### Features on UCM — Content as Convention

All features map onto UCM primitives. **Structured metadata lives in content as frontmatter, not in code as typed fields.** Most "features" are documents with different frontmatter conventions.

| Feature | UCM Primitive | Frontmatter Convention |
|---------|--------------|----------------------|
| Notes | `Document` | `type: note`, `tags: [...]` |
| TODOs | `Document` | `type: todo`, `done: bool`, `due: date` |
| Intents | `Document` + execution table | Content in UCM, execution state in lightweight table |
| RAG Knowledge | `Document` | `type: knowledge`, `source: url/pdf/gdoc` |

**What needs dedicated services:** Event bus, intent engine, identity/auth, search/RAG, voice orchestration.

---

## Design Documents

| Document | Purpose |
|----------|---------|
| [UNIFIED_CONTENT_MODEL.md](UNIFIED_CONTENT_MODEL.md) | UCM three-layer architecture (detailed specification) |
| [EMBEDDING_AND_RAG.md](EMBEDDING_AND_RAG.md) | Embedding providers, vector storage, retrieval API |
| [AUTH_AND_IDENTITY.md](AUTH_AND_IDENTITY.md) | Multi-user auth, OAuth, per-user MCP tokens |
| [CORE_SERVICE.md](CORE_SERVICE.md) | Daemon communication protocol (WS, REST, MCP) |
| [VOICE.md](VOICE.md) | Voice pipeline architecture |
| [ADMIN_UI.md](ADMIN_UI.md) | Admin web UI design |
| [DOCUMENT_UI.md](DOCUMENT_UI.md) | Document browser and editor (planned) |
| [proposals/ACTIONS.md](proposals/ACTIONS.md) | Action system — unified capability primitive (proposal) |
| [proposals/AGENTIC.md](proposals/AGENTIC.md) | Event & Intent engine — triggers, Action AST (proposal) |
| [proposals/TOOL_APPROVAL.md](proposals/TOOL_APPROVAL.md) | Tool call approval flow (proposal) |
