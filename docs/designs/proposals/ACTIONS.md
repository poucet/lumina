# Action System — Unified Capability & Composition

**Status:** Proposal — tasks under Events & Intents in [ops/ROADMAP.md](../../ops/ROADMAP.md)
**Version:** 1.1
**Parent:** [ARCHITECTURE.md](ARCHITECTURE.md)
**Related:** [AGENTIC.md](AGENTIC.md), [CORE_SERVICE.md](CORE_SERVICE.md)

---

## Current State (as of Lumina Stage 3)

The full `ActionDef` + chain engine described below is the long-term design. What's built so far:

- **MCP is the action protocol** — tools from MCP servers (including Lumina's Discord tools) are the primary action mechanism. No separate `ActionDef` trait yet.
- **Dynamic MCP registration** — `register_ephemeral_mcp` / `unregister_ephemeral_mcp` on `McpApi`. Lumina registers Discord tools on connect.
- **Typed content dispatch** — `IntoContent` / `FromContent` traits in simply-rpc. `BinaryResponse` → `ContentPart::Binary`, `BinaryUpload` ← MCP image/audio content. RPC macro generates `rest_dispatch_as_content`.
- **Tool invocation** — `list_all_tools` + `call_tool_direct` on `McpApi` using rmcp types (`Tool`, `CallToolRequestParam`, `CallToolResult`).
- **DaemonToolService** — exposes REST API methods as tools via `rest_dispatch_as_content` → `ContentPart` → `ToolResultContent`.
- **rmcp `#[tool]` macros** — Lumina uses rmcp's built-in `#[tool]` / `#[tool_router]` / `#[tool_handler]` for tool definition.

What's NOT built yet: `ActionDef` trait, action registry (beyond MCP), chains, transforms, suspension, intent integration.

---

## Problem

The daemon needs to expose capabilities (store documents, fetch from Google Docs, search, emit events) to multiple callers: client UI buttons, LLM agents, the intent engine, and user-defined automations. Today there's no unified way to invoke a capability without routing through an LLM conversation turn. Many operations are mechanical and don't need an LLM.

Additionally, useful workflows often chain capabilities: "fetch this Google Doc, convert to markdown, store it." Callers need a way to compose capabilities without writing custom code for each combination.

## Goals

- **One primitive** — `invoke_action` is the universal way to call any capability
- **One registry** — all capabilities (built-in, MCP sidecar, transform) registered in one place
- **MCP-projectable** — every registered action is automatically available as an MCP tool to LLMs
- **Composable** — actions can be chained with data flow between steps
- **Reentrant** — chains, LLMs, and intents can all invoke actions, including other chains
- **Dynamic** — MCP sidecars can connect/disconnect at runtime; their tools appear/disappear as actions

## Non-Goals

- Visual workflow builder (future)
- Parallel step execution in chains (future — start with sequential)
- Distributed execution across multiple daemons
- Inventing a new programming language — chains are declarative data, all complex logic lives in action implementations

---

## Core Primitive: ActionDef

Every capability in the daemon is an `ActionDef`. One trait, one registration, two access paths (direct call + MCP tool).

```rust
/// Execution context — flows alongside input but is NOT part of the MCP tool schema.
/// LLMs never see this. Actions pull secrets, caller identity, and platform
/// context from here rather than from input args.
struct ActionContext {
    caller: CallerId,           // who initiated (user, agent, intent, chain)
    secrets: SecretStore,       // oauth tokens, api keys — never serialized to output
    platform: PlatformContext,  // channel_id, guild_id, conversation_id
    chain_state: Option<ChainState>, // if mid-chain: completed steps + outputs
    depth: u32,                 // recursion depth for chain safety
}

trait ActionDef: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn input_schema(&self) -> JsonSchema;
    fn output_schema(&self) -> JsonSchema;
    async fn execute(&self, input: Value, ctx: &ActionContext) -> Result<ActionOutcome>;
}

/// An action can complete immediately or suspend for later resumption.
enum ActionOutcome {
    Done(Value),
    Suspended { state: Vec<u8>, resume_on: EventFilter },
}
```

**Key separation: input vs context.** The `input` is what the action operates on — it matches `input_schema` and is visible in MCP tool calls. The `ActionContext` is the side-channel: caller identity, secrets, platform metadata. It flows through chains implicitly. LLMs and MCP clients never see it, never supply it — the daemon populates it from the session/caller.

The action registry holds all `ActionDef` instances. The MCP tool listing is a projection of this registry — every action is automatically discoverable and callable as an MCP tool. The MCP projection exposes only `input_schema`/`output_schema`, never the context.

### Action Kinds

| Kind | Transport | Example |
|---|---|---|
| **Built-in** | Direct function call | `store_document`, `emit_event`, `search` |
| **MCP tool** | MCP call to sidecar | `gdocs/fetch_document`, `github/create_issue` |
| **Transform** | Pure function, in-process | `html_to_markdown`, `extract_frontmatter` |
| **Chain** | Chain engine (recursive) | A saved sequence of steps |

All four kinds implement `ActionDef`. The caller doesn't know or care about the transport.

---

## Action Registry

Single source of truth for all capabilities.

```
Action Registry
 ├── store_document        [built-in]    → direct fn call
 ├── search                [built-in]    → direct fn call
 ├── emit_event            [built-in]    → direct fn call
 ├── html_to_markdown      [transform]   → pure fn
 ├── gdocs/fetch_document  [mcp:gdocs]   → MCP client call
 ├── github/create_issue   [mcp:github]  → MCP client call
 ├── invoke_chain          [built-in]    → chain engine
 └── ...
```

### Dynamic MCP Registration

When an MCP sidecar connects (via REST `POST /register` or UCM config at startup):

1. Daemon connects to the MCP server, discovers tools
2. Each tool is wrapped as an `ActionDef` and registered with a namespaced name (`sidecar_name/tool_name`)
3. Tools become available as actions and as MCP tools to LLMs
4. When the sidecar disconnects, its actions are marked unavailable (calls return error or are deferred)
5. On reconnection, actions become available again

### MCP Projection

The daemon's own MCP tool listing is generated from the action registry:

- External MCP clients (LLMs, other systems) see all registered actions as MCP tools
- Calling an MCP tool on the daemon = calling `invoke_action`
- This includes `invoke_chain` itself — an LLM can send a full chain as a single tool call

---

## Invocation

Three entry points, same execution path:

| Caller | Mechanism | Example |
|---|---|---|
| **Client** | WebSocket message | UI button sends `invoke_action { action: "store_document", args: {...} }` |
| **LLM agent** | MCP tool call | Agent calls `invoke_action` or any action directly by name |
| **Intent engine** | Direct dispatch | Intent fires → resolves action from AST → calls registry |
| **Chain step** | Direct dispatch | Chain engine calls next step via registry |

All paths resolve through the action registry to the same `ActionDef::execute`.

---

## Composition: Chains

A chain is an ordered sequence of steps with expression-based data flow between them.

### Chain Definition

```yaml
steps:
  - action: gdocs/fetch_document
    args: { doc_id: "abc123" }
    as: fetched

  - action: html_to_markdown
    args: { input: "{fetched.content}" }
    as: converted

  - action: store_document
    args:
      name: "{fetched.title}"
      content: "{converted}"
      content_type: "text/markdown"
      source: "gdocs"
    as: stored
```

### Data Flow

Each step can reference outputs of prior steps using `{step_name.field}` expressions. This uses the same `Expr` system from the [Action AST in AGENTIC.md](AGENTIC.md):

| Expression | Resolves to |
|---|---|
| `{fetched.title}` | Field from a prior step's output |
| `{input.doc_id}` | Field from the chain's own input args |
| `"literal"` | Literal value |

### Step Types

Steps in a chain are actions — the same `ActionDef` primitive:

1. **Tool call** — invoke an MCP sidecar tool (`gdocs/fetch_document`)
2. **Built-in** — invoke a daemon capability (`store_document`)
3. **Transform** — apply a pure conversion function (`html_to_markdown`)
4. **Sub-chain** — invoke another chain (reentrant, with recursion depth limit)

### Transforms

For cases where tool outputs don't match the next tool's expected input. Named, pure functions registered as actions:

| Transform | Input | Output |
|---|---|---|
| `html_to_markdown` | HTML string | Markdown string |
| `extract_frontmatter` | Markdown with frontmatter | `{ frontmatter, body }` |
| `json_pick` | `{ value, fields: [...] }` | Subset of fields |

Transforms are registered as `ActionDef` — they're actions like everything else. New transforms can be added without changing the chain engine.

### LLM as Transform (Future)

For cases where expressions and named transforms aren't sufficient, a step type that asks an LLM to reshape data. Opt-in per step, not the default. Not needed for v1 — expressions and named transforms cover the mechanical cases. Unlikely to work well for structured/binary content.

---

## Integration with Intent Engine

The [Agentic System](AGENTIC.md) already defines intents that fire actions in response to events. This design unifies the action side:

- Intent actions resolve through the same action registry
- `call_service`, `update_document`, `emit_event`, `forward`, `notify` from AGENTIC.md are all actions in the registry
- An intent can fire a single action or a chain
- A chain step can emit an event (via the `emit_event` action), which can trigger further intents

```
Event → Intent Engine → Action Registry → execute
                                        ↓
                              emit_event action → Event Bus → more intents...
```

### Saved Chains as Recipes

A chain can be saved as a UCM document — a reusable recipe:

```markdown
---
type: recipe
name: import_gdoc
description: Fetch a Google Doc, convert to markdown, store in UCM
input_schema:
  doc_id: { type: string, required: true }
---
steps:
  - action: gdocs/fetch_document
    args: { doc_id: "{input.doc_id}" }
    as: fetched
  - action: html_to_markdown
    args: { input: "{fetched.content}" }
    as: converted
  - action: store_document
    args:
      name: "{fetched.title}"
      content: "{converted}"
      content_type: "text/markdown"
      source: "gdocs"
```

A recipe is an intent with `trigger: manual` — same content model, just invoked directly instead of by an event. An intent can also reference a recipe as its action, combining event triggers with reusable chains.

---

## MCP Boundary

MCP is the *external* protocol. ActionDef is the *internal* abstraction. The daemon never speaks MCP to itself.

```
External                          Boundary              Internal
                                     │
LLM agent ──MCP tool call──►        │
External MCP client ──MCP──►        │    ┌──────────────────┐
                                     ├───►│  Action Registry │──► execute(input, ctx)
Client ──WebSocket msg──►           │    │  (ActionDef)     │
Intent engine ──direct──►           │    └──────────────────┘
                                     │             │
MCP sidecar ◄──MCP call────────────┘◄────────────┘  (outbound, for MCP-kind actions)
```

**Inbound:** MCP tool calls from LLMs/external clients are translated to `invoke_action` calls. The daemon populates `ActionContext` from the session — the MCP caller never supplies secrets or platform context.

**Outbound:** When the registry resolves an action to an MCP sidecar tool, the daemon makes an MCP client call to that sidecar. The sidecar's result is returned as the action's output. The daemon injects side-channel context into outbound calls (see below).

**Projection:** The daemon's MCP tool listing is generated from the action registry. It exposes `input_schema`/`output_schema` per action. Context fields and injected parameters are invisible.

### Side-Channel to External Sidecars

External MCP sidecars need secrets and context (OAuth tokens, user identity) but receive calls via MCP, not `ActionContext`. The daemon acts as a **trusted intermediary** that enriches outbound MCP calls.

The sidecar's actual MCP tool schema may include parameters like `token` or `user_id`. These are **not exposed** in the daemon's MCP projection — LLMs and clients never see them. The daemon injects them from `ActionContext` before making the outbound call.

```
LLM sees:                          Daemon sends to sidecar:
  gdocs/fetch_document               fetch_document
    doc_id: "abc123"                    doc_id: "abc123"
                                        token: "ya29.a0..."  ← injected from ctx.secrets
```

Injection rules are declared at sidecar registration:

```yaml
# UCM document or REST registration
---
type: mcp_server
name: gdocs
endpoint: localhost:9002
injected_params:
  token: { from: "ctx.secrets.google_oauth" }
  user_id: { from: "ctx.caller.id" }
---
Google Docs integration: fetch, list, extract documents
```

The MCP-wrapper ActionDef for `gdocs/fetch_document`:
1. Receives `input` (just `doc_id`) and `ctx` (has secrets, caller info)
2. Resolves injected params from `ctx` per the registration config
3. Merges injected params into the MCP tool call args
4. Calls the sidecar with the enriched args
5. Returns the result, stripping any secret material from the output

This keeps the sidecar simple — it receives everything it needs in the tool call. The daemon is the only component that knows how to map context to parameters. Sidecars don't need a back-channel to the daemon for secrets.

---

## Async Execution & Suspension

Actions are not always instant. Some take minutes (API calls with retries), some take hours or days (waiting for human approval). The chain engine and action runtime handle this without adding language features.

### ActionOutcome

An action returns `Done(Value)` for immediate completion or `Suspended { state, resume_on }` to pause and wait:

```rust
enum ActionOutcome {
    Done(Value),
    Suspended {
        state: Vec<u8>,          // opaque serialized state for the action to resume from
        resume_on: EventFilter,  // what event resumes this action
    },
}
```

### Chain Suspension

When a step in a chain returns `Suspended`, the chain engine:

1. Serializes chain state to disk: completed steps, their outputs, current step, the suspension state
2. Registers an intent that listens for the `resume_on` event
3. Returns `Suspended` to the caller (suspension propagates up)
4. When the event arrives, the engine reloads chain state, resumes the suspended action, and continues with the remaining steps

### Human Approval as an Action

`request_approval` is a regular ActionDef. No special chain syntax needed:

```yaml
steps:
  - action: gdocs/fetch_document
    args: { doc_id: "{input.doc_id}" }
    as: fetched
  - action: request_approval
    args:
      approver: "{ctx.admin}"
      message: "Import '{fetched.title}' into workspace?"
    as: approval
  - action: store_document
    args:
      name: "{fetched.title}"
      content: "{fetched.content}"
```

The `request_approval` action: sends a notification to the approver, returns `Suspended { resume_on: approval_event }`, and resumes when the human responds. The chain engine handles the wait — it doesn't know or care that it's waiting for a human vs an API.

### Design Principle

Async, state persistence, and sandboxing are **runtime concerns in the action implementation**, not language features in the chain definition. Chains stay declarative data. The ActionDef implementation handles complexity:

- A built-in action manages its own async logic
- A sandboxed action runs in a restricted environment (future: WASM)
- A long-lived action serializes its own state for suspension
- The chain engine only knows `Done` vs `Suspended`

---

## Recursion & Safety

Since chains can invoke chains and actions can emit events that trigger intents:

- **Recursion depth limit** — chains enforce a max depth (e.g., 8). Exceeding it returns an error.
- **Cycle detection** — the chain engine tracks the call stack. If the same chain appears twice, it's an error.
- **Timeout** — each chain execution has a wall-clock timeout. Individual steps inherit a share of the remaining budget.

---

## Decided

1. **`ActionDef` is the universal primitive** — one trait, one registry, two access paths (direct + MCP projection)
2. **MCP is external protocol, ActionDef is internal** — daemon never speaks MCP to itself
3. **Side-channel via `ActionContext`** — secrets, caller identity, platform context flow alongside input but are invisible to MCP/LLMs
4. **Async is a runtime concern** — `ActionOutcome::Suspended` + chain state serialization, not a language feature
5. **Human approval is just an action** — `request_approval` suspends and resumes on approval event
6. **No new programming language** — chains are declarative data, complexity lives in action implementations
7. **Start with option A** — chains stay dumb, handlers are smart. Evaluate embeddable languages (Starlark, WASM) only if needed later

## Open Questions

1. **Error semantics in chains** — if step 2 of 4 fails, what happens? Start with fail-fast, but may need per-step error handling later.
2. **Streaming results** — should `ActionOutcome` support a streaming variant? Probably yes eventually, not in v1.
3. **Permissions** — should some actions be restricted to certain callers? `ActionContext` has the caller identity, but policy enforcement is TBD.
4. **Naming convention** — `sidecar/tool` namespacing for MCP tools. Built-ins: flat (`store_document`) or namespaced (`daemon/store_document`)?
5. **Sandboxing model** — future actions from untrusted sources may need sandboxing (WASM?). Not v1 but shapes the `ActionDef` trait design.
6. **`ctx` references in chain YAML** — the `{ctx.admin}` syntax in the approval example implies chains can reference context fields. Which context fields should be exposable, and how do we prevent leaking secrets into step outputs?
