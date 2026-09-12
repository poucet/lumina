# Design: Tool Call Approval Flow

**Status:** Accepted — scheduled under Multi-user polish in [ops/ROADMAP.md](../../ops/ROADMAP.md)
**Affects:** SessionApi, Lumina chat, MCP tool execution

---

## Problem

When the LLM wants to call an MCP tool during a conversation, some tools need user confirmation before execution (e.g., sending an email, posting to a channel, deleting data). The daemon currently executes tools immediately. We need a way for clients (Lumina, Noema) to intercept, display, and approve/reject tool calls.

---

## Proposed Flow

```
User sends message in Discord
  → Lumina sends to daemon session
  → LLM decides to call tool "send_email"
  → Daemon pauses execution, emits ToolCallPending event
  → Lumina receives event, posts embed in Discord:
      "🔧 Wants to use: send_email(to: alice@..., subject: ...)"
      [✅ Approve] [❌ Reject]
  → User clicks ✅
  → Lumina calls daemon: confirm_tool_call(session_id, tool_call_id, approved: true)
  → Daemon executes the tool
  → DaemonEvent::ToolResult streamed back
  → LLM continues with the result
```

If rejected:
```
  → User clicks ❌
  → Lumina calls confirm_tool_call(..., approved: false)
  → Daemon sends synthetic ToolResult with rejection message
  → LLM adjusts its response
```

---

## API Changes

### SessionApi additions

```rust
/// Approve or reject a pending tool call.
async fn confirm_tool_call(
    &self,
    session_id: &SessionId,
    tool_call_id: &str,
    approved: bool,
) -> anyhow::Result<()>;
```

### New DaemonEvent variant

```rust
DaemonEvent::ToolCallPending {
    id: String,
    name: String,
    arguments: serde_json::Value,
}
```

Distinct from `ToolCall` (which means "already executed"). `ToolCallPending` means "waiting for approval."

### Tool approval policy

Not all tools need approval. The policy could be:
- **Per-tool metadata:** Tools declare `requires_approval: bool` in their MCP definition
- **Per-session config:** Session options specify which tools need approval
- **Client-controlled:** Client tells the daemon "pause on all tool calls" or "auto-approve these tools"

A simple starting point: session-level flag `tool_approval: ToolApproval` where:

```rust
enum ToolApproval {
    /// Execute all tools immediately (current behavior)
    AutoApprove,
    /// Pause and wait for client confirmation on every tool call
    RequireAll,
    /// Auto-approve listed tools, require approval for others
    AllowList(Vec<String>),
}
```

Lumina sessions would default to `RequireAll`. Noema might use `AutoApprove` or `AllowList`.

### CreateSessionOptions change

```rust
pub struct CreateSessionOptions {
    pub persistence: Option<Persistence>,
    pub system_prompt: Option<String>,
    pub model_id: Option<String>,
    pub seed: Option<Vec<SeedMessage>>,        // NEW: inline seed
    pub tool_approval: Option<ToolApproval>,    // NEW: approval policy
}
```

---

## Timeout

If the user doesn't respond to an approval request within a timeout (e.g., 5 minutes), the daemon should auto-reject and let the LLM know the tool call was not approved due to timeout.

---

## Noema Integration

Noema could show a similar approval UI — a modal or inline confirmation in the chat. The same `ToolCallPending` / `confirm_tool_call` API works for both clients.

---

## Open Questions

1. Should the daemon queue multiple tool calls that need approval, or pause after each one?
2. Should there be a "remember this approval" mechanism (approve once vs. approve always)?
3. How does this interact with tool call batching (LLM requesting multiple tools at once)?
