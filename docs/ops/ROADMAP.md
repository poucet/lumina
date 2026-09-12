# Simply Platform — Post-v1 Roadmap

Features beyond the v1.0 scope. For v1.0 goals, see [v1.0/GOAL.md](v1.0/GOAL.md).

---

## Search & Knowledge (extensions)

Basic embedding + semantic search is built. These are extensions:

| Feature | Complexity | Description |
|---------|------------|-------------|
| Wiki-style cross-linking | Medium | `[[doc:Title]]` syntax, backlinks panel |
| Hierarchical tags | High | Multi-tagging, tag hierarchy for documents and conversations |
| Frontmatter-aware search | Medium | Filter by arbitrary key-value conditions (`tags contains "urgent"`) |

---

## Intent System Use Cases

These build on the Events phase (event bus + intent engine) once it's delivered.

| Feature | Intent Pattern | Description |
|---------|----------------|-------------|
| Dynamic Typst functions | `render.before.*` → transform | Evaluate Typst functions at render time |
| Auto-journaling | `conversation.turn_produced` → `execute_prompt` | Extract insights from conversations |
| Active context engine | Intents + context documents | Contextual nudges and awareness |
| Scheduled prompts | `cron` → `execute_prompt` | Replaces Python Lumina's ScheduleCog |

---

## Multimodal

| Feature | Complexity | Description |
|---------|------------|-------------|
| Image generation | Medium | Stable Diffusion, DALL-E, Flux — exposed as MCP tools |
| PDF extraction | Medium | OCR, image extraction, text conversion |
| Video transcription | Medium | Whisper on video audio tracks |

---

## External Integrations

Implemented as MCP tool servers or Skills that register with the daemon.

| Integration | Tools | Events (future) |
|-------------|-------|------------------|
| GitHub | `create_issue`, `comment_pr`, `list_prs` | `github.pr_opened` |
| Notion | `update_page`, `create_page`, `search` | `notion.page_updated` |
| Google Calendar | `create_event`, `list_events` | `calendar.event_starting` |
| Email | `send_email`, `search_inbox` | `email.received` |
| Brave/Google Search | `web_search` | — |

---

## Future Platforms

New crates that connect to `simply-daemon` via `RemoteDaemon` or embed it.

| Platform | Notes |
|----------|-------|
| Telegram | Same `Daemon` trait + `RemoteDaemon` pattern as Lumina |
| WhatsApp | Same pattern |
| WebRTC / simply-chris.ai/meet | Voice + video via daemon's `VoiceApi` |
| Cloud sync / multi-device | Requires persistent auth layer |
