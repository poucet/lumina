# Design: UCM Storage Architecture

**Status:** Decided — UCM stays in daemon. Landed; see UCM Unification in [ops/CHANGELOG.md](../../ops/CHANGELOG.md)
**Affects:** Content phase, daemon architecture

---

## Decision

UCM storage is a core part of the daemon. Not a separate service.

---

## Rationale

1. **The daemon IS the storage + agent hub.** Pulling storage out would just recreate the daemon elsewhere.
2. **Conversation history needs fast storage interactions.** A network hop for every message read/write is unnecessary overhead.
3. **RAG is core to an LLM daemon.** Embedding, indexing, and retrieval are fundamental to agent quality, not an add-on.
4. **MCP exposure is selective.** The LLM needs MCP tools to interact with documents (`create_document`, `search`, `query`), but not every internal storage method needs to be an MCP tool. Internal APIs stay internal.

---

## Architecture

```
┌──────────────────────────────┐
│  Daemon                       │
│  ├─ simply-core               │
│  │  ├─ UCM storage (SQLite)   │
│  │  ├─ RAG / indexing         │
│  │  └─ document CRUD          │
│  ├─ Agent orchestration       │
│  ├─ MCP tool registry         │
│  │  ├─ built-in doc tools ◄── agent calls these
│  │  └─ external services      │
│  └─ Sessions                  │
└──────────────────────────────┘
     ▲           ▲
     │ WS        │ MCP
     │           │
   Noema      Lumina (Discord tools)
```

**MCP tools exposed to agents:**
- `create_document`, `update_document`, `delete_document`
- `query_documents` (frontmatter filters)
- `search` (semantic / RAG)
- `get_document` (by name or ID)

**Internal-only APIs (not MCP):**
- Revision management, tab operations, blob storage
- Conversation history read/write
- Index maintenance
