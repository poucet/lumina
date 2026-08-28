# OPERATIONAL — how this project runs

**Bootstrap**: this file is the entry point for an empty context window. Load it
plus the docs it names and the working system is restored. State lives in
[BOARD.md](BOARD.md); this file is the *system*.

## The doc web

- [BOARD.md](BOARD.md) — live operational state: in-flight tracks, queue,
  recently merged. Chris edits freely; Claude re-reads before every
  integration and keeps it current.
- [PROJECT.md](PROJECT.md) — what Simply is, current focus.
- [FUTURE_ROADMAP.md](FUTURE_ROADMAP.md) — milestone plan and completion state.
- designs/ — design docs. **Changes need human review before committing**
  (per global rule); Claude drafts, Chris approves.
- v1.0/, CHANGELOG-1.0.md — historical.
- `.claude/CONTEXT.md` — architecture map (crates, traits, data flow); keep
  current when structure changes.

Layout rule: top level holds only living system docs (OPERATIONAL, BOARD,
PROJECT, FUTURE_ROADMAP, CHANGELOG); everything else goes in designs/ or an
archive dir.

## Roles

- **Chris (human)**: direction, priorities, review; runs all builds and tests
  (Claude never runs `cargo build/test` or type generation); edits BOARD
  freely — Claude picks edits up at the next checkpoint.
- **Main agent (Claude)**: coordinator, kept free — launches and steers
  subagents, integrates their work, maintains docs, answers design questions,
  commits landed tracks. ALL engineering goes to subagents — even one-line
  code fixes; the main agent never edits code itself. (Reading code to
  diagnose, review, or verify is main-agent work; writing it is not.)
- **Subagents**: engineering tracks, one per track, each with an explicit
  file-ownership fence (prompts enumerate the files/crates they may touch;
  overlaps are deliberate, not accidents), committing incrementally with jj,
  reporting on completion.

## Conventions

- **jj, not git** — always `jj commit` (never `git commit`, never
  `jj describe`). Commit with explicit paths when the working copy carries
  someone else's in-flight changes.
- **Linear history** — rebase chains, never merge commits.
- **Docs before eng; externalize by default** — decisions, ideas, and plans
  never live only in a context window; capture them in the doc web the moment
  they occur. Idea/direction lands → docs first (BOARD queue entry, design
  doc as appropriate) → then engineering.
- **Verification is Chris's** — Claude hands over compiling-intent code and
  states what needs testing; Chris runs `cargo build/test` and reports back.
  Iterate on his error output.
- **Simplify by generalizing** — prefer changes that make the system smaller
  and more general: one trait behind parallel mechanisms (`ToolProvider` is
  the house example) rather than a sibling mechanism. New abstractions must
  come out ahead on a concepts/LOC ledger.
- **Capability parity** — anything a user can do through a client, the AI can
  do through tools (and vice-versa over time).
- No co-author lines in commits. Docs under docs/, never inside crates.

## Track lifecycle

1. Direction lands → BOARD entry (+ design doc if non-trivial) before code.
2. Launch subagent with: jj-only rules, file-ownership fence, no build/test
   rule, docs placement, required final report.
3. Agent works with incremental jj commits (crash-safe; chains survive dead
   agents — relaunch with the same fence to resume).
4. Land → main agent re-reads BOARD for Chris's edits → rebases the chain
   onto main → tells Chris what to build/test → updates BOARD, and records
   the outcome durably: append/extend the CHANGELOG entry for the arc and
   keep PROJECT.md's current-focus line and structure tree honest (finished
   work that lives only on the BOARD is not recorded).
5. Recovery from a crashed session: read this file + BOARD.md, then `jj log`
   and compare against the BOARD's in-flight table.

## The same system, inside Aurora

Aurora (the voice agent) runs on the identical operating pattern, one level
down — the concepts map directly:

- **Main loop kept free** — the voice loop (mic → STT → fast reply → TTS) is
  Aurora's "main agent": it must stay responsive and never block on long
  work. Anything slow (research, multi-step tool chains, file surgery) is
  delegated to a background daemon session — the "subagent track" — while
  the voice loop acknowledges and stays available. Completion is announced
  by voice when the background session finishes.
- **Linearized history** — every exchange, including background-session
  results, is folded back into one linear conversation in UCM storage, the
  same way agent chains rebase onto main: one history, no forks left dangling.
- **Docs as externalized memory** — Aurora persists decisions and durable
  facts through daemon storage (entities/documents) instead of holding them
  in a context window, mirroring "externalize by default". Context windows
  are disposable; storage is the system of record.
- **Ownership fences** — background sessions get scoped tool access
  (RequestContext), not the full registry, just as subagents get file fences.
- **Capability parity** — every Aurora voice gesture has a tool counterpart
  (e.g. switching its own reasoning model is a tool, so both Chris by voice
  and the agent by choice can invoke it).
