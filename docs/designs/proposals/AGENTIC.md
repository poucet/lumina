# Agentic System — Event & Intent Engine

**Status:** Proposal — tasks under Events & Intents in [ops/ROADMAP.md](../../ops/ROADMAP.md)
**Version:** 1.1
**Parent:** [ARCHITECTURE.md](ARCHITECTURE.md)

---

## Current State

None of the event/intent system is built yet. The Events phase depends on Foundation (complete) and soft-depends on Content. Lumina's schedule system (Python ScheduleCog) was deferred to this phase.

What exists that feeds into this design:
- `DaemonEvent` enum (session events: TextDelta, ToolCall, TurnComplete, etc.)
- `InboundEvent` type on SessionApi (stub for push events)
- MCP tool infrastructure (actions are MCP tools in the current architecture)

---

## Overview

The core provides an **event-driven reactive system** where intents (registered reactions) fire in response to events from any source. Time is just one event source among many.

**Core principle: code provides capabilities, content provides configuration.** Event sources and action handlers are code (traits implemented by developers). But all routing — which events trigger which actions, with what parameters, targeting whom — lives in UCM markdown documents.

```
Event Sources                    Intent Engine                Actions (typed, not all need LLM)
┌─────────────┐                 ┌──────────────┐            ┌──────────────────────┐
│ Timer/Cron   │──►             │              │         ──►│ forward (no LLM)     │
│ Discord      │──►             │  Match event │         ──►│ notify (no LLM)      │
│ WebRTC       │──►  events ──►│  against     │──► fire ──►│ emit_event (no LLM)  │
│ Conversation │──►             │  registered  │         ──►│ update_doc (no LLM)  │
│ Agent life-  │──►             │  intents     │         ──►│ platform_action      │
│  cycle       │──►             │              │         ──►│ execute_prompt (LLM) │
│ Document     │──►             │              │         ──►│ resume_conv (LLM)    │
│ User-defined │──►             │              │         ──►│ call_tool            │
└─────────────┘                 └──────────────┘            └──────────────────────┘
```

**Key property:** Actions can emit events. This enables chaining — agent A completes → emits event → triggers intent that resumes mainline → mainline completes → emits event → triggers reminder to user.

---

## Event Sources

Event sources are **open and extensible**. The core provides built-in sources; platforms register their own; users and agents can define custom sources.

| Source | Events | Built-in / Platform |
|--------|--------|-------------------|
| **Timer** | `time.exact`, `time.cron`, `time.interval` | Built-in |
| **Agent lifecycle** | `agent.task_completed`, `agent.task_failed`, `agent.heartbeat` | Built-in |
| **Conversation** | `conversation.turn_produced`, `conversation.completed` | Built-in |
| **Intent lifecycle** | `intent.fired`, `intent.completed`, `intent.failed` | Built-in |
| **Document** | `document.created`, `document.updated`, `document.deleted` | Built-in |
| **Service lifecycle** | `service.connected`, `service.disconnected`, `service.tools_changed` | Built-in |
| **Client lifecycle** | `client.connected`, `client.disconnected` | Built-in |
| **Discord** | `discord.member_joined`, `discord.message`, `discord.reaction` | Lumina |
| **WebRTC** | `rtc.user_joined`, `rtc.user_left`, `rtc.session_started` | /meet |
| **User-defined** | Any custom event name | Runtime |

Platform-specific sources are registered when the platform connects to simply-daemon. The daemon doesn't need to know about Discord — it just sees events with typed payloads. Service/client lifecycle events are emitted automatically by the daemon when peers connect or disconnect.

---

## Intents

An intent is a registered reaction: **"when this event pattern occurs, do this action."** Intents are UCM documents (searchable, embeddable, LLM-readable) with a lightweight execution table for runtime state.

**Intent triggers can be:**

| Trigger Type | Example | Resolution |
|---|---|---|
| **Single event** | `discord.member_joined` | Fire on each matching event |
| **Time (exact)** | "April 5th at 3pm" | Timer source emits at datetime |
| **Time (fuzzy)** | "Tomorrow morning" | LLM resolves to concrete time, stored alongside original |
| **Time (recurring)** | Cron / interval | Timer source emits repeatedly |
| **Condition (all)** | Agent A done AND agent B done | Accumulate events, fire when all matched |
| **Condition (any)** | Agent A done OR timeout | Fire on first match |
| **Compound** | Agent A done AND 1h elapsed | Multiple trigger types combined |

**Intent frontmatter examples:**

```markdown
# Time-based: reminder
---
type: intent
trigger:
  source: timer
  original: "tomorrow morning"
  resolved: "2026-04-01T09:00:00"
  precision: fuzzy
action: resume_conversation
conversation_id: "conv-abc-123"
target: "user:chris@discord:12345"
created_by: "user:chris"
---
Check in on the voice migration progress
```

```markdown
# Event-based: welcome new Discord members
---
type: intent
trigger:
  source: discord
  event: member_joined
  recurrence: every
action: execute_prompt
prompt: "Welcome the new member {event.member.name} to the server"
target: "channel:welcome@discord:67890"
created_by: "user:chris"
---
Generate a personalized welcome message for new members
```

```markdown
# Condition: wait for subtasks, then resume
---
type: intent
trigger:
  mode: all
  conditions:
    - source: intent_lifecycle
      event: intent.completed
      intent_id: "intent-research-a"
    - source: intent_lifecycle
      event: intent.completed
      intent_id: "intent-research-b"
action: resume_conversation
conversation_id: "conv-mainline-456"
target: "user:chris@discord:12345"
created_by: "agent:lumina"
---
Synthesize findings from both research agents
```

---

## Actions

Actions are **typed and generic**. Not every action involves an LLM — many are pure plumbing.

| Action Type | Involves LLM? | Description |
|---|---|---|
| `forward` | No | Stream event payload to a target |
| `notify` | No | Send a fixed message to a target |
| `emit_event` | No | Emit another event into the bus (enables chaining) |
| `update_document` | No | Modify a UCM document |
| `platform_action` | No | Platform-specific side effect (add role, pin message, etc.) |
| `call_service` | No | Call a registered service by name — protocol-agnostic |
| `execute_prompt` | Yes | Run a prompt through the agent |
| `resume_conversation` | Yes | Reopen and continue a suspended conversation |

---

## Compile Once, Execute Forever — The Action AST

Users express intents in natural language. The LLM compiles this into a declarative AST once, and the engine executes the compiled form directly.

```rust
/// The compiled action AST — what the engine actually executes
enum Action {
    Forward { target: Expr },
    Notify { target: Expr, message: Expr },
    EmitEvent { event_type: Expr, payload: HashMap<String, Expr> },
    UpdateDocument { document_id: Expr, updates: HashMap<String, Expr> },
    PlatformAction { action: String, params: HashMap<String, Expr> },
    CallService { service: String, method: String, args: HashMap<String, Expr> },
    ExecutePrompt { prompt: Expr, target: Expr },          // LLM in the loop
    ResumeConversation { conversation_id: Expr },           // LLM in the loop
}

/// Expressions support late binding — values resolved at execution time
enum Expr {
    /// Known at compile time: "channel:general", 42, true
    Literal(Value),

    /// Resolved from the triggering event's payload
    EventField(FieldPath),

    /// Resolved from the creator's context at execution time
    ContextRef(ContextKey),

    /// Resolved by querying the system
    Lookup(LookupExpr),

    /// String interpolation: "Welcome {event.member.name} to {context.guild.name}!"
    Template(Vec<TemplatePart>),
}
```

**Late binding via `Expr`:** Not all values are known at compile time.

| Expr Type | Example | Resolved When |
|---|---|---|
| `Literal` | `"channel:general"`, `true`, `42` | Compile time (immediate) |
| `EventField` | `event.member.name`, `event.timestamp` | From the triggering event's payload |
| `ContextRef` | `context.channel`, `context.user`, `context.guild` | From the creator's runtime context at fire time |
| `Lookup` | `lookup(user, discord_id: event.member.id)` | System query at fire time |
| `Template` | `"Welcome {event.member.name}!"` | String interpolation after resolving inner expressions |

**The intent document IS the serialized AST.** Frontmatter is YAML serialization of the AST. Users who understand the schema can write it directly. Users who don't, describe what they want and the LLM writes it for them. Either way, the engine loads it into the same typed Rust structures.

**The separation:**

```
What developers build (code):        What users/agents configure (content):
┌────────────────────────────┐       ┌──────────────────────────────────────┐
│ EventSource implementations│       │ UCM documents (type: intent)         │
│  - discord, github, email  │       │                                      │
│  - timer, webhook, notion  │       │ "When discord.member_joined,         │
│                            │       │  forward to #welcome channel"        │
│ ActionHandler impls        │       │                                      │
│  - send_email, forward     │       │ "When github.pr_merged on main,      │
│  - send_discord, notify    │       │  notify user:chris on discord"       │
│  - update_document         │       │                                      │
│                            │       │ "Every morning at 9am,               │
│ Intent engine (fixed)      │       │  summarize overnight activity        │
│ Event bus (fixed)          │       │  and email it to the team"           │
└────────────────────────────┘       └──────────────────────────────────────┘
  Rarely changes.                      Changes constantly. No deploy needed.
  Deploy to add capabilities.          Create/edit/delete at runtime.
```

---

## Storage Split

```
UCM Document (the "what")              Intent Execution Table (the "when/status")
┌──────────────────────────┐           ┌──────────────────────────────────┐
│ ---                      │           │ intent_id: <ucm doc id>          │
│ type: intent             │     ┌────►│ status: pending | waiting |      │
│ trigger: { ... }         │─────┘     │         active | completed       │
│ action: resume           │           │ next_fire: 2026-04-01T09:00:00   │
│ target: user:chris       │           │ last_fired: null                 │
│ ---                      │           │ fire_count: 0                    │
│ Body text / context      │           │ conditions_met: { ... }          │
└──────────────────────────┘           │ recurrence: <serialized rule>    │
                                       └──────────────────────────────────┘
```

---

## Engine Loop

```
1. Process event queue:
   - For each incoming event, find intents subscribed to that event type
   - Update conditions_met for matching intents
   - If all/any conditions satisfied, mark intent as ready

2. Check timer-based intents: poll for next_fire <= now, mark as ready

3. For each ready intent:
   a. Read UCM document for full context
   b. Execute action (notify, resume conversation, run prompt, heartbeat, emit event)
   c. Update execution table (last_fired, fire_count, compute next_fire if recurring)
   d. If one-shot and complete, mark status = completed
   e. Emit intent.completed event (enables chaining)

4. Sleep until next timer fires or new event arrives
```

---

## Service Registry — Protocol-Agnostic Service Calls

The `call_service` action calls registered services by name. The core is **agnostic of transport protocol**.

```rust
/// A registered service — the engine doesn't know or care about the transport
trait Service: Send + Sync {
    fn name(&self) -> &str;
    async fn call(&self, method: &str, args: HashMap<String, Value>) -> Result<Value>;
    fn methods(&self) -> Vec<ServiceMethod>;  // For discovery / validation
}

/// Transport adapters — each wraps a different protocol
struct McpService { client: McpClient, server_name: String }
struct GrpcService { channel: tonic::Channel, service_descriptor: ServiceDesc }
struct RestService { base_url: Url, auth: Option<AuthConfig> }
```

The intent author doesn't know or care whether `praxis` is MCP, `calendar` is gRPC, or `github-api` is REST. They just call `service: name, method: name`. The registry handles transport.

---

## Extension Points

### Adding a New Event Source

Implement the `EventSource` trait and register with the core.

```rust
trait EventSource: Send + Sync {
    fn source_id(&self) -> &str;
    async fn start(&self, bus: EventBus) -> Result<()>;
}
```

### Adding a New Action Handler

Implement the `ActionHandler` trait and register with the engine.

```rust
trait ActionHandler: Send + Sync {
    fn action_type(&self) -> &str;
    async fn execute(&self, intent: &IntentDocument, event: &Event) -> Result<ActionOutcome>;
}
```

### Adding a New Service

Implement the `Service` trait with a transport adapter and register at startup.
