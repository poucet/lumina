# Roadmap

> Horizons and what a version means — only what is not done. Landed work: [CHANGELOG.md](CHANGELOG.md) · live work: [BOARD.md](BOARD.md). What belongs here: @AGENTS.md §2.

---

## v1.0 — one daemon, every client

v1.0 unifies Lumina (Discord) and Noema (desktop) into a single Rust workspace: a `simply-daemon` hub with `simply-core` underneath, every client — Lumina, Noema, Aurora, Telegram, admin UI — connecting to it and sharing tools, knowledge, and automation. Foundation, Lumina, Voice, Content & RAG, Unified Frontend, UCM unification, Vault-backed Markdown, Aurora and simply-terminal are landed (see CHANGELOG). Still open for v1.0, in order:

### 1. Events & Intents

Reactive event system — timers, platform events (Discord, desktop), LLM-compiled intents with action ASTs. Scheduled prompts, automated workflows. **Design:** [proposals/ACTIONS.md](../designs/proposals/ACTIONS.md), [proposals/AGENTIC.md](../designs/proposals/AGENTIC.md). Stage 1.1–1.3 (event bus, timer source, intent documents) landed 2026-06-01.

**Stage 1 — Event bus + timer source (remainder)**
- 1.4 Intent execution table (SQLite) — stores runtime state (last fired, next fire, status)
- 1.5 Action AST: `Expr` with `Literal` and `Template` variants (minimal subset)
- 1.6 Action handlers: `notify`, `emit_event`
- 1.7 Engine loop: process queue -> check timers -> fire ready intents -> sleep

**Stage 2 — Full action AST + service registry**
- 2.1 Full `Expr` enum: `EventField`, `ContextRef`, `Lookup`, `Template`
- 2.2 Expression resolver — evaluates `Expr` tree against event context
- 2.3 Action handlers: `forward`, `update_document`, `call_service`
- 2.4 Service registry trait with transport adapters
- 2.5 MCP transport adapter (wraps MCP servers as services)
- 2.6 Internal transport adapter (wraps daemon's own services)

**Stage 3 — Platform event sources**
- 3.1 Lumina registers Discord event source with daemon on connect
- 3.2 Discord events emit into bus: `discord.member_joined`, `discord.message`, `discord.reaction`
- 3.3 Noema registers desktop events: app focus, idle detection
- 3.4 Event source registration protocol (clients via WS, services via REST)

**Stage 4 — LLM-compiled intents**
- 4.1 MCP tool: `create_intent(description)` — LLM compiles natural language to AST frontmatter
- 4.2 LLM compilation prompt: natural language -> trigger + action + target YAML
- 4.3 Fuzzy time resolution: "tomorrow morning" -> concrete datetime + original text preserved
- 4.4 Re-compilation flow: edit description -> re-compile AST
- 4.5 AST validation against registered event sources and action handlers

**Stage 5 — Conditions + workflow**
- 5.1 Condition evaluation in intent engine (`all` / `any` modes)
- 5.2 Compound triggers: condition + time combined
- 5.3 Intent chaining: action output -> next intent's trigger
- 5.4 Conversation resumption from intents (reopen suspended conversation with context)
- 5.5 Multi-agent orchestration: spawn sub-agents as intents, mainline waits

### 2. Multi-user polish

Discord role-based access control, admin user management UI. **Design:** [AUTH_AND_IDENTITY.md](../designs/AUTH_AND_IDENTITY.md). Stages 1–2 (connection auth, single-port OAuth, admin page) and per-user MCP OAuth with persistent, refresh-on-read tokens are landed.

**Stage 3 — Per-user MCP OAuth (remainder)**
- 3.4 `auth_required` error response when user has no token for a service
- 3.6 Token revocation (admin or self-service)
- Encrypt-at-rest for the persisted token store (today: `data/tokens.json`, 0600, plaintext like `settings.toml` secrets)

**Stage 4 — Discord role-based access control**
- 4.1 `[mcp_access]` config in `lumina.toml` — map Discord roles to MCP server access
- 4.2 Lumina checks user's Discord roles before MCP tool calls
- 4.3 Graceful denial: "You need the `developers` role to use GitHub tools"
- 4.4 Tool call approval flow (see [proposals/TOOL_APPROVAL.md](../designs/proposals/TOOL_APPROVAL.md))

**Stage 5 — Admin UI user management**
- 5.1 User management: list users, view linked accounts, revoke access
- 5.2 Connection browser: view connected clients, active sessions
- 5.3 Per-service OAuth status display

### 3. UCM — Directories UX (fast-follow to UCM unification)

Backend exists (`EntityApi.move_relation` — atomic reparent + sibling renumber; `system::directory` entity type). **Design:** [UNIFIED_CONTENT_MODEL.md](../designs/UNIFIED_CONTENT_MODEL.md).
- Admin UI: directory tree in nav sidebar; "New folder" button; drag-and-drop filing
- Noema UI: directory tree in DocumentsPanel; drag-and-drop filing

### Dependencies

```
Events Stage 1 (engine loop) ──► Events Stage 2 (Service Registry) ──► Events Stage 3 + 4 (parallel)
Events Stage 3 + 4 ──► Events Stage 5 (Conditions + Workflow)

Multi-user Stage 3 (Per-User MCP OAuth) ──► Multi-user Stage 4 (Role-Based Access)
Multi-user Stage 4 ──► Multi-user Stage 5 (Admin UI)
```

---

## Beyond v1.0

### Search & Knowledge (extensions)

Basic embedding + semantic search is built. These are extensions:

| Feature | Complexity | Description |
|---------|------------|-------------|
| Wiki-style cross-linking | Medium | `[[doc:Title]]` syntax, backlinks panel |
| Hierarchical tags | High | Multi-tagging, tag hierarchy for documents and conversations |
| Frontmatter-aware search | Medium | Filter by arbitrary key-value conditions (`tags contains "urgent"`) |

### Intent System Use Cases

These build on the Events & Intents horizon above (event bus and timers are in; the intent engine is not) once it's delivered.

| Feature | Intent Pattern | Description |
|---------|----------------|-------------|
| Dynamic Typst functions | `render.before.*` → transform | Evaluate Typst functions at render time |
| Auto-journaling | `conversation.turn_produced` → `execute_prompt` | Extract insights from conversations |
| Active context engine | Intents + context documents | Contextual nudges and awareness |
| Scheduled prompts | `cron` → `execute_prompt` | Replaces Python Lumina's ScheduleCog |

### Multimodal

| Feature | Complexity | Description |
|---------|------------|-------------|
| Image generation | Medium | Stable Diffusion, DALL-E, Flux — exposed as MCP tools |
| PDF extraction | Medium | OCR, image extraction, text conversion |
| Video transcription | Medium | Whisper on video audio tracks |

### External Integrations

Implemented as MCP tool servers or Skills that register with the daemon.

| Integration | Tools | Events (future) |
|-------------|-------|------------------|
| GitHub | `create_issue`, `comment_pr`, `list_prs` | `github.pr_opened` |
| Notion | `update_page`, `create_page`, `search` | `notion.page_updated` |
| Google Calendar | `create_event`, `list_events` | `calendar.event_starting` |
| Email | `send_email`, `search_inbox` | `email.received` |
| Brave/Google Search | `web_search` | — |

### Future Platforms

New crates that connect to `simply-daemon` via `RemoteDaemon` or embed it.

| Platform | Notes |
|----------|-------|
| Telegram | Skeleton crate landed 2026-06-01; same `Daemon` trait + `RemoteDaemon` pattern as Lumina |
| WhatsApp | Same pattern |
| WebRTC / simply-chris.ai/meet | Voice + video via daemon's `VoiceApi` |
| Chrome extension | Daemon client for in-browser contexts. Most promising small near-term slice: a "send this page to Simply as an imported document" action that pushes the active tab's content/URL through the gdocs-style import path, creating a `document::tabbed` or flat `document::knowledge` entity with `origin = "browser:<url>"`. Full extension (sidebar chat, Google Meet caption scraping, audio streaming) after that |
| Cloud sync / multi-device | Requires persistent auth layer |
