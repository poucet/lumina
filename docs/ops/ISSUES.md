# Issues

> A parking lot, not a queue; file it, triage it, don't interrupt a lane for it. What belongs here: @AGENTS.md §2.
> 🐞 open · 🔬 triaged (cause known, fix specced) · 🔄 being fixed · ✅ fixed · 🗑️ won't fix / not a bug. Sizes: S ≈ a session · M ≈ a lane.

| Status | Found | What | Verdict |
|---|---|---|---|
| 🐞 | 2026-04 | OAuth identity linking not working end-to-end (Discord→email user merge). `sltxmzqq` (2026-04-20) links Discord identity on `/google auth`; the full merge still needs verifying | S |
| 🔬 | 2026-04 | `simply-daemon/build.rs` `STRING_TYPES` hardcodes id newtype names | S — auto-discover by parsing `simply-core/src/storage/ids.rs` for `define_id!` invocations instead |
