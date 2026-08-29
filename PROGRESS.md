# jcode-fusion: Progress Tracker

**Read this file first at the start of every session.** It's the source of truth for what's done, what's in progress, and what's next — DESIGN.md has the *what/why*, this file has the *where we are*.

---

## Setup decisions (locked in 2026-08-29)

- **Project name: Fusion.** Considered Alloy/Crucible/Waypoint as alternatives; kept "Fusion" (shortened from the working label `jcode-fusion`) since it required zero rebranding — already the binary name, folder name, and terminology used throughout this file and DESIGN.md.
- **Base**: jcode, pinned to `v0.81.2` (matches all research in DESIGN.md — not tracking upstream `main`, to avoid the base shifting mid-build).
- **Fork location**: `Desktop/ClaudeCode/jcode-fusion/jcode/` — a fresh clone, kept fully separate from the user's real working jcode install at `~/.local/bin/jcode` / `~/.jcode/`. **Never touch those paths.**
- **Binary/package name**: root `[[bin]] name` renamed `jcode` → `jcode-fusion` in `Cargo.toml` (package name left as `jcode` internally — low risk, doesn't land on PATH). Other bin targets (`test_api`, `jcode-harness`, benches) left as-is for now, not installed to PATH by normal use.
- **Hosting**: pushed to GitHub as of 2026-08-29 — `github.com/tirthfx/jcode-fusion` (private). Local checkout's remotes: `origin` = the new fork repo, `upstream` = the real `1jehuang/jcode` (renamed from the default `origin` the clone came with). Working branch is `main`, created from the `v0.81.2` tag checkout (which was in detached HEAD).
- **Sandboxing sequencing**: macOS (Seatbelt) first, since dev machine is macOS. Linux (bwrap) comes later, validated via Docker/CI rather than bare-metal (this machine can't run bwrap natively).
- **License note**: jcode is MIT. Reimplement patterns from Codex/Grok Build in idiomatic jcode-style Rust — don't copy-paste Apache-2.0 source (see DESIGN.md §6 license note).

## Phase status

| Phase | Contents | Status |
|---|---|---|
| **0 — Foundation** | Unified Mission Engine (#1) + provable-safe rewind (#4) | **Phase 0 complete.** Mission Engine (4/4 slices) + provable-safe rewind, all tested and pushed. |
| **1 — Safety** | Guardian reviewer (#3), execpolicy (#6), macOS sandboxing first (#5) | **Phase 1 complete.** Sandboxing (macOS), Guardian (ambient-scoped, deny-only), execpolicy (Starlark, extends jcode-command-risk) all shipped. |
| **2 — Swarm rework** | Worktree-per-subagent isolation (#2) | **Phase 2 complete.** Creation + cleanup + merge-back all shipped. |
| **3 — Ecosystem** | ACP support (#7), orchestration-as-script (#8) | Not started |
| **4 — Memory** | Two-phase consolidation (#9) | Not started |

Item **#0 (multi-provider OAuth)** is jcode's existing feature — kept as-is, never modified.

## Session log

### 2026-08-29 — Setup
- Design doc finalized (`DESIGN.md`): base-harness decision, swarm resolution, size estimates (source-size-based and real on-disk footprint from GitHub release assets).
- Diagnosed user's existing local jcode install: `~/.jcode` was 7.9GB, almost entirely a stale Self-Dev scratch checkout (`scratch/jcode-fix-688/`, 5.5GB — mostly an abandoned `target/` build cache) plus 276MB of retained old binary versions. Not a property of jcode's real footprint — flagged as a build-cache-hygiene gap worth avoiding in the fused harness (see DESIGN.md open risks). User asked to delete it; that's a manual step for them (file deletion is outside what I do directly) — command given in chat.
- Cloned `1jehuang/jcode` fresh, pinned to `v0.81.2`, into `jcode-fusion/jcode/` (558MB on disk incl. `.git`). Confirmed `git describe --tags` = `v0.81.2` exactly.
- Renamed root `[[bin]] name` from `jcode` → `jcode-fusion` in `Cargo.toml` to prevent any collision with the user's real install at `~/.local/bin/jcode`.
- Ran `cargo build --bin jcode-fusion` (plain cargo, not the repo's `scripts/dev_cargo.sh` wrapper — that script pulls in remote-config loading and its own telemetry/action-logging, more than wanted for a first sanity build). **Succeeded** (exit 0). Verified: `target/debug/jcode-fusion` exists (338MB, unstripped debug Mach-O arm64 binary), runs, and prints `jcode v0.81.2-dev (3453b8b61, dirty)`.
  - Note: the version string still says `jcode`, not `jcode-fusion` — it's derived from the package name / a build script, not the `[[bin]] name`. Cosmetic, not urgent; fix later if it matters.
  - "dirty" in the version string is expected — it's reflecting our intentional `Cargo.toml` bin-rename edit, not a problem.
  - `target/` is already 4.8GB after just this one debug build of the workspace. This is **normal for a from-scratch debug build of a ~90-crate workspace**, not a repeat of the earlier scratch-checkout bug — but it's exactly the kind of directory that shouldn't be forgotten. `rm -rf target/` (or `cargo clean`) is always safe here — unlike the git checkout, it's pure build cache, nothing of value in it.

## Source-level findings (2026-08-29, real code read — supersedes doc-based assumptions in DESIGN.md §4.1/§5.2)

**Big one: jcode already has a `Goal`/`GoalStatus` data model.** `crates/jcode-task-types/src/lib.rs` defines `Goal` (id, title, scope, status, description, success_criteria, milestones, progress_percent, updates) and `GoalStatus` (`Draft/Active/Paused/Blocked/Completed/Archived/Abandoned`), with a full CRUD layer in `crates/jcode-base/src/goal.rs` and an agent tool at `crates/jcode-app-core/src/tool/goal.rs`. **This means Phase 0 is not "build a new Goal Engine from scratch" — it's "extend this existing model with the two things it's missing":**
- **No automatic budget enforcement.** `progress_percent`/`success_criteria` are free-text the model sets itself; nothing tracks tokens/cost/turns or halts a run on a budget.
- **No automatic completion verification.** Nothing checks whether a goal is *actually* done before marking it complete — this is exactly where Grok Build's adversarial-verifier idea (DESIGN.md item #1) plugs in.

**The Overnight subsystem (`crates/jcode-app-core/src/overnight.rs`, `run_supervisor`) is the closest existing analog to a Goal Engine driver loop, and gives us the architecture pattern to copy**: it drives the agent from *outside* the turn loop (`agent.run_once_capture(prompt)` in an outer `loop{}`), deciding continue/stop externally and injecting a new prompt each iteration — rather than modifying `run_turn` itself. **A Goal Engine should be built the same way**: a new outer supervisor (modeled on `run_supervisor`) that wraps the existing `Goal` struct instead of `OvernightManifest`, adds real budget enforcement (missing today — Overnight only has an advisory one-shot usage *projection*, `build_usage_projection`, that never halts anything) and real completion verification (missing today — Overnight only asks the model to self-report via unverified task-card JSON).

**Rewind: a `/rewind` feature already exists** (`Agent::rewind_to_message`/`undo_rewind`, `turn_execution.rs`) but has real, specific gaps vs. "provable-safe":
- `rewind_undo_snapshot` is **in-memory only** on `Agent` — never persisted, doesn't survive a restart, and only holds **one** snapshot (a second rewind overwrites the first, no stack).
- Message-only — no filesystem/tool-side-effect awareness.
- Good news: **compaction never actually destroys raw messages** (`CompactionManager` only tracks a `compacted_count` cursor over an immutable slice) — so "reconstruct pre-compaction state" is mostly already possible from data that's already there, not a from-scratch problem.
- Reusable primitives: `Session::rewind_target_stored_indices()`, `truncate_messages`, `replace_messages` (`jcode-base/src/session.rs`).

**Any turn-loop-level change must account for two parallel implementations that need to stay in sync**: `turn_loops.rs::run_turn` (blocking/CLI) and `turn_streaming_mpsc.rs::run_turn_streaming_mpsc` (TUI/production path) — same control flow, duplicated. The existing `tool_calls.is_empty()` branch (`turn_loops.rs:831-877`) with its `maybe_continue_*` pattern is the natural hook point if a change needs to happen *inside* a turn rather than wrapping it Overnight-style from outside.

**Ambient/Self-Dev deep-dive landed — major revision below, read before doing anything else.**

### Critical correction: rename "Goal Engine" → "Mission Engine" (naming collision found)

jcode already ships an **unrelated, actively-used feature also called `Goal`** — `crates/jcode-task-types`, `jcode-base/src/goal.rs`, exposed as the `"initiative"` agent tool (`create|list|show|resume|update|checkpoint|focus`), persisted at `~/.jcode/goals/`, with its own side-panel UI and a `TelemetryToolCategory::Goal` mapping. **This is a durable, manual, cross-session task/milestone tracker — not an autonomous loop, and not what we want to build.** Calling our consolidation project "Goal Engine" would directly collide with this shipped feature's vocabulary. **Renaming our concept to "Mission Engine" from here on** — which turns out to fit perfectly, because:

**We found the real foundation to build on: `crates/jcode-app-core/src/mission.rs` — dead code, but already shaped almost exactly right.** `Mission { session_id, objective, long_horizon_intent, status: MissionStatus, success_criteria, validation_plan, checkpoints, ... }` with `MissionStatus { Active, Paused, Blocked, NeedsDecision, BudgetLimited, Complete, Abandoned }` — **`BudgetLimited` already exists as a first-class state**, closer to Codex's `ThreadGoal` state machine than anything else in the codebase. But `mission::set()` (the only write path) has **zero callers anywhere** — only the read side is wired (`mission::active_system_reminder`, called from 3 sites in `crates/jcode-tui/src/tui/app/input.rs` to inject a reminder into turns). **Someone already half-designed this and never finished it.** Our Phase 0 work is finishing that wiring, not designing from scratch.

### Revised understanding of the three "fragmented" subsystems

- **Ambient Mode is NOT a state machine to unify — it's a plain polling loop.** "Gardening/scouting/working" is prompt text (`ambient/prompt.rs`), not code — there's no `enum CyclePhase`. Its only real state is `AmbientStatus { Idle, Running, Scheduled, Paused, Disabled }`. **Also found: its "adaptive, usage-aware scheduling" is dead code in production** — `AdaptiveScheduler::calculate_interval` has real projection math, but both real call sites always pass `None` for usage data, collapsing it to a fixed interval + exponential backoff. `UsageLog::record()` (the only way to populate real usage data) is never called outside its own unit tests.
- **Overnight Mode is the most mature and closest existing template** — one long-lived `Agent` across an entire run, wall-clock-milestone-driven prompt switching, and (unlike Ambient) **real working budget wiring**: `crate::usage::fetch_all_provider_usage()` → `ProviderUsage` is an actually-functioning cross-provider usage source, computed at preflight. **Mission Engine's budget tracking should build on this (`crate::usage`), not on Ambient's dead `UsageLog`/`AdaptiveScheduler`.**
- **Self-Dev Mode is arguably a different category of thing entirely, not a task-completion loop** — it's a build/reload pipeline (`BuildRequestState`, `ReloadPhase`), genuinely separate machinery with no shared code with Ambient or Overnight. **Open question to resolve before Phase 2**: does Self-Dev actually belong in this consolidation at all, or does "Mission Engine" only unify Ambient + Overnight (+ revived Mission), leaving Self-Dev as its own separate subsystem? Leaning toward the latter — forcing a build-pipeline state machine into a task-completion-loop abstraction may not be a real win.

### Revised Phase 0 plan

Not "design a new state machine" — it's: **(1)** finish wiring `Mission`'s write path (give `mission::set()` real callers), **(2)** add budget enforcement sourced from `crate::usage::fetch_all_provider_usage()` (Overnight's pattern, not Ambient's dead one) that actually halts a run on `BudgetLimited`, **(3)** add completion verification (Grok Build's adversarial-verifier idea) that gates `Complete`, **(4)** build the outer driver loop modeled on `overnight.rs::run_supervisor` (drive `agent.run_once_capture()` from outside, don't touch `run_turn`/`run_turn_streaming_mpsc` internals). Ambient Mode's dead adaptive-scheduler bug is a pre-existing jcode issue, not ours to fix unless it's in our way.

## Phase 1 source-level findings (2026-08-29 — real code read, done before Phase 1 starts per the review pass)

**`pre_tool` hook checks out exactly as docs described** — `GateDecision::Allow/Block` (`jcode-base/src/hooks.rs`), genuinely synchronous (`tokio::time::timeout` around child-process wait), fail-open on anything but exit code 2. Call site: `Registry::execute` (`tool/mod.rs:672`), before `tool.execute()` runs. No correction needed here — safe to build on directly.

**Two real gaps that change the Guardian/sandboxing design:**

1. **There is no general "allow this command?" TUI prompt for risky tool calls in normal interactive sessions — this assumption from the original web research was wrong.** The only approval-request system that exists (`SafetySystem`/`request_permission`, `jcode-base/src/safety.rs`) is **restricted to ambient/autonomous sessions only** (`ensure_ambient_session` check) — normal interactive sessions can't even call it. Worse: `PermissionRequest.wait: bool` (meant to mean "block until user decides") **is a no-op** — never read anywhere except where it's written. `request_permission` always returns `Queued` immediately and non-blocking; the only thing that ever resolves a queued request is a debug-socket command handler (`ambient:approve:<id>`/`ambient:deny:<id>`), not any TUI dialog. **A Guardian auto-approver plugs in by adding an automated path that answers these requests instead of/before the debug-socket human path** — but note it only covers ambient-session calls, not regular interactive bash tool calls (those go through a completely different, already-sophisticated system — see #2).

2. **jcode already has a much more sophisticated command-risk classifier than "execpolicy as Starlark" assumed it'd be adding on top of nothing.** `crates/jcode-command-risk` does blast-radius classification (`RiskLevel::{Safe,Low,Confirm,Catastrophic}`) — deliberately avoiding simple allow/deny lists in favor of tokenizing commands, unwrapping `sudo`/`env`/`xargs` wrappers, and checking redirect targets against protected paths. It already has a "Reflect" mechanic: a `Confirm`-tier command forces the model to resubmit with a substantive `justification` field (min 25 chars, rejects bare "yes/ok") rather than prompting a human. This is wired directly into `BashTool::execute` via `bash_destructive_gate.rs`, before any subprocess spawns. **Open design decision, not yet resolved**: does execpolicy-as-Starlark replace this crate wholesale, layer on top of it, or just migrate its rule tables (`DESTRUCTIVE_COMMANDS`, `CONDITIONALLY_DESTRUCTIVE`, protected-path logic) into Starlark form? Building both side-by-side would mean two overlapping deterministic gates on the same `bash` call — needs a decision before Phase 1 starts, not during it.

**A third finding that's a hard architectural gap for sandboxing specifically, not just a "needs a decision" item**: **file-edit tools never spawn a subprocess at all.** `WriteTool::execute` calls `tokio::fs::write(...)` directly, in-process. A sandbox built by wrapping the bash-tool's subprocess spawn point (`build_shell_command`, `bash.rs:657` — note there are **two** separate spawn sites here, foreground/background *and* a third detached/reload path, `build_detached_shell_wrapper`, that also needs wrapping) would **silently miss file writes/edits entirely**. Real options: (a) sandbox the whole jcode process, not just shell-outs, or (b) re-route file-edit tools through a sandboxed helper process. This needs to be decided before Phase 1 sandboxing work starts — it changes the shape of the whole feature, not an implementation detail.

**Confirmed no existing sandboxing scaffolding at all** (`grep -rniI "sandbox-exec|seatbelt|bubblewrap|landlock|seccomp|bwrap"` — zero matches) — that part of the original assumption was correct, nothing to duplicate there.

## Phase 3/4 source-level findings (2026-08-29 — real code read)

**Big correction: jcode's ACP adapter is NOT a stub — it's a real, actively-maintained JSON-RPC server** (`src/cli/acp.rs`, 2,188 lines). Implements `initialize`, `session/new`, `session/load`/`resume` (with history replay), `session/prompt` (with streaming `session/update` notifications), `session/cancel`/`close`, config options, and even a jcode-specific extension mechanism. Confirmed via changelog cross-check — this has shipped multiple real feature releases, not abandoned scaffolding. **This flips Phase 3/item #7 from "extend a partial adapter to full coverage" to "close specific, identified gaps"**:
- Never sends client-side callback methods (`fs/read_text_file`, `fs/write_text_file`, `session/request_permission`, `terminal/*`) — does all file I/O and permission handling server-side, never delegates to the ACP host, which real hosts (Zed etc.) generally expect for editor-integrated UX.
- No auth negotiation via ACP itself (`authMethods` hardcoded empty).
- Session-scoped MCP servers explicitly rejected ("not supported yet").
- `initialize` ignores the client's requested protocol version, always pins to jcode's own v1.

**`VersionedPlan` is already durably persisted — not "ephemeral server-memory state" as assumed.** `crates/jcode-app-core/src/server/swarm_persistence.rs` is a mature persistence layer: atomic writes with backup rotation, CAS-style version checks, tombstone deletion, dormant-plan GC (7-day default) — written on ~27 call sites across nearly every plan mutation. **Orchestration-as-script (#8) doesn't need to build persistence from scratch** — it extends this existing layer with a new capability: turning a `VersionedPlan` into a reusable, parameterized, replayable template (which doesn't exist today in any form).

**Zero embedded-scripting dependency exists today** (`rhai`/`starlark`/`mlua`/`wasmtime` — no hits in `Cargo.toml` or `Cargo.lock`, direct or transitive). Both execpolicy-as-Starlark (#6) and workflow-as-script (#8) are genuinely greenfield dependency additions.

**Memory consolidation: docs are accurate here (rare case where they weren't stale).** `docs/MEMORY_ARCHITECTURE.md`'s "Phase 8: Ambient Garden" checklist is explicitly unimplemented, every item unchecked, confirmed by real code inspection. Only one sliver is wired: embedding backfill runs fire-and-forget after each ambient cycle (`ambient/runner.rs:774`). Real APIs to build on: `MemoryManager` (`jcode-base/src/memory.rs`) and `MemoryGraph` (`jcode-memory-types/src/graph.rs`, has `cascade_retrieve` already).

**Important cross-cutting finding, affects Phase 3 AND Phase 4 AND ties back to Phase 0: there is no generic background-job scheduler in jcode at all.** The only periodic-loop primitive that exists is Ambient Mode's own purpose-built runner, tightly coupled to spawning a full LLM agent session. **This means Mission Engine's supervisor loop (Phase 0, modeled on `overnight.rs::run_supervisor`) is a candidate to become the shared scheduling primitive Phases 3–4 also need**, rather than each phase inventing its own ad hoc scheduling — worth deciding explicitly rather than defaulting to 3 separate bespoke schedulers.

## Phase 2 source-level findings (2026-08-29 — real code read)

**Issue #1090 is already fixed** — commit `0a66fbcd2` ("fix: preserve live headless workers during idle checks"), an ancestor of our `v0.81.2` checkout. DESIGN.md cited this as Phase 2's justification; that premise is now stale/wrong and has been dropped from DESIGN.md. Worktree isolation is still worth doing, just purely on conflict-resolution merits, not this bug.

**`VersionedPlan` confirmed already persisted to disk** (`server/swarm_persistence.rs`) — this independently confirms what the Phase 3/4 agent found too. Good cross-validation.

**Zero worktree isolation exists today** — workers share the parent's working dir outright by default; no `git2`/`gix` dependency, no `git worktree add` call anywhere in the codebase. Reusable scaffolding found: `git_common_dir_for()`/`swarm_id_for_dir()` already detect worktree `.git` layouts and derive a shared swarm id from the common `.git` dir — worth building on rather than duplicating.

**Two smaller corrections**: the TUI's "worktree manager" role is aspirational comment text with zero backing implementation (no such role exists in the actual role-ranking code) — don't assume it as prior art. And "one-level fan-out" is actually a root/deep-mode boolean gate, not a depth counter — deep-swarm mode has no depth limit today at all (only a 1000-member cap + concurrency budget). File-touch conflict detection is confirmed exactly as docs describe: pure post-hoc notification, no locking.

## Full review pass complete (2026-08-29) — all 5 phases now source-verified

Every item in DESIGN.md §6 has been checked against jcode's real `v0.81.2` source, not just docs/web research. DESIGN.md has been updated throughout to reflect this (executive summary, autonomy table, full feature table, phased roadmap, size estimates, open risks). Summary of what changed per phase is in each phase's "source-level findings" section above and in DESIGN.md §6 directly.

**Real open decisions surfaced that need a call before their phase starts** (not mechanical corrections — genuine forks in approach):
1. **Guardian's scope** (Phase 1, item #3): only auto-answer ambient-session permission requests (narrower, matches what exists today), or also build a new general-session review path (broader, more work, nothing to build on)?
2. **Execpolicy vs. `jcode-command-risk`** (Phase 1, items #3/#6): jcode already has a real deterministic command-risk classifier wired into `BashTool`. Does Starlark-based execpolicy replace it, layer on top, or absorb its rule tables?
3. **File-edit-tool sandboxing gap** (Phase 1, item #5): file tools never spawn a subprocess, so a bash-subprocess-wrapping sandbox would miss them entirely. Sandbox the whole process, or route file tools through a sandboxed helper?
4. **Shared scheduler question** (cuts across #1/#8/#9): should Mission Engine's supervisor loop become the one shared "run this periodically in the background" primitive for orchestration-as-script and memory consolidation too, instead of 3 separate ad hoc schedulers?

None of these block Phase 0 (the first slice — finishing `Mission`'s write path — doesn't touch any of them), so they don't need answers today, but they do need answers before Phase 1/3/4 actually start.

## Phase 0 first slice: DONE (2026-08-29) — `Mission`'s write path now has real callers

Built a `mission` agent tool (`crates/jcode-app-core/src/tool/mission.rs`, registered in `tool/mod.rs` as `mission`, alongside `initiative`/`memory`/`todo` in the base tool set — available in every session, not gated to ambient-only). Actions: `set` (declare/reactivate objective), `show`, `status` (validated against a new `MissionStatus::parse`, added to `mission.rs`), `checkpoint`, `clear`. This is the first genuinely new code written for Fusion — everything before this was research/planning/setup.

- **Verified, not just written**: `cargo check -p jcode-app-core` clean (2 pre-existing unrelated warnings only), and a new test (`tool/mission_tests.rs`, `mission_tool_full_round_trip`) passes — covers the full lifecycle (empty → set → reminder-injected-while-active → show → checkpoint → status-to-blocked → reminder-stops-once-not-active → invalid-status-rejected → clear → operations-on-cleared-mission-fail-cleanly).
- **Deliberately did NOT do** in this slice (by design, per the "smallest coherent first slice" plan): no budget enforcement, no completion verification, no outer supervisor loop, no change to `set()`'s signature (doesn't yet accept `success_criteria`/`validation_plan` — the existing functions were wired up as-is, not extended).
- **Confirmed real-world effect**: `crate::mission::active_system_reminder()` (the pre-existing read path, wired into the TUI's turn-reminder injection) now actually has something to read, since a session can genuinely have an active mission for the first time.

## Phase 0 first slice: manually verified end-to-end (2026-08-29)

Ran the actual production code path live (not the automated test) via a new example, `crates/jcode-app-core/examples/mission_tool_demo.rs` (`cargo run --example mission_tool_demo -p jcode-app-core`) — drives the real `MissionTool` through the full lifecycle against an isolated `JCODE_HOME`, no mocks. All steps behaved correctly: reminder absent → set → reminder present → checkpoint → status→blocked → reminder absent again → invalid status rejected → clear → post-clear operations fail cleanly.

Also tried the real `jcode-fusion run` headless one-shot command against an isolated data dir (never touched the user's real `~/.jcode`) — confirmed it correctly refuses without configured provider credentials ("No credentials configured for provider auto-detection"), which is expected and correct; a full live-LLM/TUI verification would need the user's own login, done in their own terminal, not something to fake here.

**Real finding worth recording**: `MISSION_CONTINUATION_TEMPLATE` (`crates/jcode-base/src/prompt/mission_continuation.md`, injected while a mission is `Active`) already contains detailed self-audit instructions — "treat completion as unproven," "treat uncertain/stale/missing evidence as not achieved," explicit verification-discipline guidance. **This is real prior art for the completion-verification idea (Grok Build's `/goal`)** — it's prompt-level self-auditing, not code-enforced independent verification, so the actual gap (nothing *blocks* a status transition to `Complete` if the model ignores this guidance) still stands and is still worth building. But the prompt scaffolding to build on top of is more mature than assumed — worth having the eventual verifier reuse/reference this template's own audit criteria rather than inventing separate ones.

## Phase 0 second slice: budget enforcement — DONE (2026-08-29)

Added `mission::enforce_budget(session_id)` — checks real provider usage via `crate::usage::fetch_all_provider_usage()` (the same actually-working source Overnight uses, not Ambient's dead one, per the earlier findings) and transitions a mission to `BudgetLimited` if any connected provider is genuinely hard-limited. Decision logic split into a pure `any_provider_hard_limited()` function so it's unit-testable without network/credentials — 3 new tests, all passing. Exposed as a new `check_budget` action on the `mission` tool.

**Documented, deliberate scope limit**: checks *any* connected provider's hard-limit status, not specifically the provider the current session is using (mission.rs is provider-agnostic and doesn't have a session→active-provider lookup available to it yet). Flagged in code comments as a known simplification to refine later, not silently shipped as if it were fully correct.

**Test gap, also documented**: the `check_budget` tool action's end-to-end path (through real `fetch_all_provider_usage()`) isn't covered by an automated test, same constraint as the live-TUI verification — it needs real network/credentials. The pure decision logic underneath it is fully tested; the thin async glue on top isn't.

## Repo pushed to GitHub (2026-08-29)

`github.com/tirthfx/jcode-fusion` (private). **Not a full-history push** — the real `1jehuang/jcode` history (~7,237 commits, ~349MB) repeatedly hit GitHub HTTP 408 timeouts from this connection, on both the full-history push and an orphan-commit squash of it. Root cause turned out to be `assets/` (169MB of README demo GIFs/MP4s, confirmed via source grep to be unreferenced by any `include_bytes!`/`include_str!` — not needed to build or run). Excluded `assets/` and pushed a single squashed commit instead — landed cleanly.

**What's actually on GitHub now**: one commit containing the full `v0.81.2` source tree (minus `assets/`) plus Fusion's Phase 0 changes, plus `DESIGN.md`/`PROGRESS.md` at the repo root (copied in from the parent `jcode-fusion/` dir, which they lived in locally and were missed on the first push). **Not** the full upstream commit history — that stays available locally via `git remote add upstream https://github.com/1jehuang/jcode.git` (already configured on the local checkout) if ever needed, e.g. to properly rebase against upstream changes later.

Local branch `fusion-main` tracks `origin/main`. The original full-history branch (`main`, from the tag checkout) still exists locally, untouched, just not pushed.

**Follow-up (2026-08-29, same day)**: the first push included jcode's own `.github/workflows/` (9 files — release publishing, Windows/FreeBSD smoke tests, TestFlight, etc.), which auto-triggered on push and failed (no matching secrets/runners/repo context), sending a batch of GitHub failure-notification emails. Removed all 9 workflow files and pushed the removal — future pushes won't retrigger anything. Optional extra safety net not done (no tool access to it): flipping the repo's Settings → Actions → General to fully disabled.

## Phase 0 third slice: completion verification — DONE (2026-08-29)

Self-certified completion is now closed off: `update_status()` refuses `Complete` outright. Real flow: `success_criteria` (new — declares what "done" means; a prerequisite slice 1 deliberately skipped) → `claim_complete` (evidence required, bare affirmations like "done"/"ok" rejected via a substantiveness check mirroring `jcode-command-risk`'s existing `Justification::is_substantive()` pattern — reused the convention rather than inventing a new one) → `verify_completion` (the actual gate: refuses with no criteria set, refuses if evidence count doesn't cover criteria count, only then transitions to `Complete`).

**Two scope limits, documented in the code itself, not silently glossed over**:
1. Verification is a real, enforced *structural* check (evidence coverage vs. criteria count) — not yet a genuine LLM-based independent review of whether the evidence is actually *true*. A real semantic verifier (spawning a fresh `Agent` via `Agent::run_once_capture`, the same primitive `overnight.rs::run_supervisor` uses, with a "try to refute this" prompt) is the natural next step once this scaffold exists.
2. Nothing yet stops the same session/turn that filed the claim from also being the one that calls `verify_completion` — true decoupling (a different identity doing the verifying) isn't enforced yet.

9 new tests, all passing (25/25 across the full mission suite, 1 pre-existing unrelated test skipped as always). Full `jcode-fusion` binary rebuilds clean. Committed and pushed.

## Phase 0 fourth slice: the outer supervisor gate — DONE (2026-08-30, via /loop)

`mission::supervisor_gate(session_id)` — called once per turn from *inside* `overnight.rs::run_supervisor`'s existing loop (right after its cancel-check), rather than duplicating Overnight's substantial `Agent`/`Session`/`Provider` construction in a parallel supervisor. Stops the loop (via the existing `mark_completed`) if the mission is `BudgetLimited` or a pending completion claim gets `verify_completion`-confirmed; a *refuted* claim does not stop the loop, the agent just keeps working. Opt-in and backward compatible — a session with no mission set always continues unaffected. Fails open on error (mirrors `pre_tool` hook's own fail-open policy).

6 new tests, all passing (31/31 across the full mission suite). Full binary rebuilds clean, zero new warnings. **This completes all four planned Mission Engine slices** — Phase 0 itself isn't fully done yet, since it also includes provable-safe rewind (#4, not started).

**Note on this session**: run via `/loop`, autonomously. Was asked to use `agy` with its new read-only GitHub MCP access for the "verify against real source" pass at the start of this slice — attempted twice (with and without `--dangerously-skip-permissions`), both blocked by Claude Code's own permission classifier (not an agy/GitHub problem — the classifier refuses the Bash invocation itself). Did not keep retrying past the second attempt per established policy; fell back to reading the already-cloned local source directly instead, which achieves the same verification goal. **If future loop iterations should actually use agy+GitHub MCP for this, the user needs to either add a narrower Bash permission rule, or confirm they're fine with local-clone verification as the standing fallback** — not re-litigated further autonomously.

## Phase 0 complete: provable-safe rewind — DONE (2026-08-30, via /loop)

New `crates/jcode-app-core/src/rewind_store.rs`: a persisted, multi-level, integrity-checked undo stack, replacing the old single in-memory `RewindUndoSnapshot`. Closes all three documented gaps — survives restart (`~/.jcode/rewind/<session>.json`), real multi-level undo (`Vec<RewindSnapshot>`, repeated `undo_rewind` walks back through multiple prior rewinds), and a SHA-256 integrity check per snapshot (using `sha2`, already a direct dependency — **no new dependency added**) that refuses a tampered/corrupt snapshot rather than applying it. Wired into `Agent::rewind_to_message`/`undo_rewind` with the same public signatures — no caller changes needed. 5 new tests, including one that deliberately corrupts a persisted snapshot on disk and confirms it's refused, not applied.

**Real regression found and fixed**: running the *full* jcode-app-core test suite for the first time (previous slices only ran mission-scoped filters) surfaced that jcode enforces tool-description token caps (20 tokens/tool, 25/parameter) — a real, tested convention slices 1-3 didn't know about and violated (mission's tool description was ~60 tokens, several params and the packed `action` enum description were also over). Trimmed everything to match the terse convention `goal.rs` (jcode's own `initiative` tool) already follows. Verified via the two actual cap tests: mission no longer appears in either's over-cap list.

**Also discovered, deliberately NOT fixed**: the full suite run surfaced 29 failures total. Spot-checked several individually (isolated, single-threaded) and confirmed via `git diff` that the affected files (`restart_snapshot.rs`, `tool/todo.rs`, `tool/jcode_docs.rs`, `tool/computer/`) have **zero uncommitted changes** — these are pre-existing failures in the `v0.81.2` base itself, unrelated to Fusion. Not our job to fix upstream jcode bugs as part of this project. Recorded here so a future session doesn't mistake these for a Fusion regression and waste time chasing them.

**This completes Phase 0 in full** (Mission Engine, 4/4 slices, + provable-safe rewind).

## Phase 1 first slice: whole-process macOS sandboxing — DONE (2026-08-30)

User's decision: whole-process (not helper-process-for-file-tools). New `crates/jcode-app-core/src/sandbox_macos.rs`, wired into `src/main.rs` right before the tokio runtime starts. Opt-in (`JCODE_FUSION_SANDBOX=1`, default off) — re-execs the entire binary under `sandbox-exec` via jcode's own existing `crate::platform::replace_process` utility (reused, not reimplemented). Deliberately conservative: `(allow default)` + `(deny file-write* ...)` against a curated credential/secret path list (`~/.ssh`, `~/.aws`, `~/.docker`, `~/.kube`, etc.) — blast-radius reduction, not a full deny-by-default lockdown (getting that wrong risks breaking the whole app).

**Real bug found and fixed before shipping, not after**: manually verified the deny rule with a raw `sandbox-exec` test first, and it silently did nothing. Root cause: macOS Seatbelt matches `subpath` rules against the *canonicalized* path — `/var/folders/...` (what `mktemp`/`TMPDIR` hand out) is actually a symlink to `/private/var/folders/...`, so a profile built from the symlinked form matches zero real writes. Fixed by canonicalizing `$HOME` before joining protected subpaths (falls back to the raw path if canonicalization fails). Re-verified with the same raw test using the canonical path — deny rule actually blocked the write this time. Added a regression test with a real symlink in a tempdir so this exact bug class can't silently reappear.

**Deliberately did not test against the user's real `~/.ssh`/`~/.aws`/etc.** — even a "should be denied" write attempt against real credential paths isn't a good risk/confidence tradeoff. Verification chain instead: unit-tested canonicalization + manually-proven Seatbelt mechanism (raw `sandbox-exec` runs) + live-tested that `jcode-fusion` actually re-execs under `sandbox-exec` without breaking normal startup (identical behavior sandboxed vs. unsandboxed against an isolated `JCODE_HOME`). 8 new tests, all passing.

**Still open for Phase 1**: Linux (bwrap) sandboxing not started (later, per the macOS-first decision). Guardian and execpolicy not started.

## Phase 1: Guardian auto-approval reviewer — DONE (2026-08-30)

Ambient-scoped, deny-only, per the earlier decision. New `crates/jcode-app-core/src/guardian.rs`: deterministic keyword-based adjudication against Codex's own four risk categories (destructive action, credential probing, data exfiltration, persistent security weakening), wired into `tool::ambient::RequestPermissionTool::execute` right before the existing `system.request_permission(request)` call. A `Deny` verdict skips the human queue entirely; `Undecided` falls through completely unchanged.

**Deliberately never auto-approves** — same honesty principle as everywhere else in this project: a trustworthy auto-approve verdict needs a real semantic judge (LLM call, not reliably testable without live credentials here) or much more structured input than the free-text fields callers currently provide. Auto-approving on keyword-*absence* is a much weaker signal than denying on keyword-*presence*, so only the safe half (deny obviously-bad requests before a human even sees them) is implemented. The interruption-reduction half (the actual stated point of Codex's Guardian) is honest future work, not faked with a weak heuristic.

7 new tests, all passing. Ran the full `jcode-app-core` test suite (not just scoped filters, per the lesson from the rewind slice) — 1202 passed, 29 failed, and confirmed the failure count and every individual test name are identical to the last full-suite run — zero new regressions from Guardian. Full binary rebuilds clean.

## Phase 1 final slice: execpolicy (Starlark-configurable command rules) — DONE (2026-08-30)

User's decision: extend `jcode-command-risk`, don't replace it. New `crates/jcode-app-core/src/execpolicy.rs` (new `starlark = "0.14"` dependency, added to `jcode-app-core` — deliberately **not** to `jcode-command-risk` itself, which is zero-dependency by design; see the crate's own doc comments, "not another model"). Wired into `tool/bash_destructive_gate.rs`, checked only after the built-in classifier already returned `Allow` — user rules can only escalate (`Confirm`/`Deny`), structurally cannot downgrade an existing built-in restriction.

Rule format is deliberately simple: `"prefix|decision|reason"` strings in a `RULES` list, not Starlark dicts — used only the `starlark` crate's list/string APIs (the parts gotten right confidently), not dict-introspection or native-function registration. The *script* itself is still real Starlark — a test proves a list comprehension can generate the `RULES` list programmatically.

**One real API-guessing miss, caught and fixed properly, not shipped blind**: `Module::new()` doesn't exist in `starlark` 0.14 — found the actual vendored crate source on disk and confirmed the real API is closure-based (`Module::with_temp_heap(|module| {...})`), fixed it, rebuilt clean. Everything else (`ListRef::from_value`, `unpack_str`, `module.get`) was correct on the first attempt (also verified against source, not assumed).

**Two honest gaps, documented not hidden**:
1. `UserDecision::Confirm` and `Deny` currently behave identically (unconditional refusal) — no resubmit-with-justification retry flow for user rules yet, unlike the built-in classifier's own `Confirm`.
2. The `OnceLock` caching wrapper isn't independently integration-tested (process-global state can't cleanly reset between tests sharing one binary) — the pure logic underneath is fully covered by 10 new tests. Also couldn't live-test the full bash-tool path via an example binary the way mission/sandboxing were, since `bash` is a private module and making it `pub` just for a test felt like a broader change than warranted.

Full test suite: 1212 passed, 29 failed — confirmed identical (count and every test name) to the prior baseline. Zero regressions. Full binary rebuilds clean.

**This completes Phase 1** — sandboxing, Guardian, and execpolicy all shipped.

## Phase 2 first slice: worktree-per-subagent isolation (creation only) — DONE (2026-08-30)

New `crates/jcode-app-core/src/swarm_worktree.rs`, wired into `server/comm_session.rs::spawn_swarm_agent` right after `resolve_spawn_working_dir` resolves — opt-in (`JCODE_FUSION_SWARM_WORKTREES=1`), applies only in the "would otherwise share the parent's directory" fallback path (an explicit `working_dir` request is left exactly as asked), fails open on any error.

**Real, thorough live testing this time** — unlike some earlier slices, this one could be fully tested against actual `git` operations (real `git init` + commit in tempdir repos, not mocks): 9/9 passing, covering opt-in default-off, unique worker labels, repo-path bucketing by canonical path, resolving the repo root from a nested subdirectory, failing cleanly outside a repo, a worktree actually containing a real working checkout, **two concurrent worktrees proven genuinely independent** (a file written in one doesn't leak into the other or the original checkout), and `remove_worktree` actually removing the directory.

**Deliberately creation-only, documented not hidden**: no automatic merge-back of a worker's worktree branch into the coordinator's tree (Grok Build's own pattern is an explicit "apply" step, not automatic — a reasonable model for later, not implemented here), no automatic cleanup of abandoned worktrees (crashed workers, cancelled swarms). `remove_worktree()` exists and is tested but nothing calls it automatically yet. The existing advisory file-touch conflict *detection* is untouched — this adds a structural layer on top, not a replacement.

This touched core swarm spawn code directly (`comm_session.rs`), not just a new isolated module — ran the full test suite as a result: 1221 passed, 29 failed, confirmed identical (count and every test name) to the established baseline, including confirming `comm_session`'s own `prepare_visible_spawn_session_*` tests were *already* in that pre-existing failure list before this change touched the file. Zero regressions. Full binary rebuilds clean.

## Phase 2 second slice: worktree cleanup — DONE (2026-08-30)

Wired into jcode's **existing** terminal-member pruning sweep (`server.rs::prune_expired_terminal_swarm_members`, already runs periodically via `swarm_terminal_member_gc_interval`) rather than inventing new scheduling — when a terminal member's retention window expires and gets pruned, its worktree (if it has one) is removed in the same pass.

**How it tells "has a worktree" from "ordinary shared dir"**: `swarm_worktree::is_managed_worktree_path()` — a path-prefix check against `~/.jcode/worktrees/`, deliberately not a new field on `SwarmMember` (would touch that struct's definition and persistence format, a much bigger change than warranted).

**A git assumption verified by hand before relying on it**: `SwarmMember` only stores `working_dir` (the worktree path), not the original repo root. Confirmed with a real `git worktree add` + `git -C <worktree> worktree remove <worktree>` that removal works self-contained from just the worktree's own path — git worktree commands operate on repo-wide state shared via the main `.git` directory.

Fire-and-forget (`tokio::spawn`) so a slow `git` call never blocks the pruning sweep; failures are logged, not disruptive (a leftover worktree is disk usage to deal with later, not a correctness issue).

3 new tests. Full `swarm_worktree` suite: 12/12. Touched `server.rs`'s core pruning logic directly — ran the full suite: 1224 passed, 29 failed, identical to the established baseline. Zero regressions.

**Phase 2 status: creation ✅, cleanup ✅, merge-back ✅ — Phase 2 complete.**

## Phase 2 third slice: worktree merge-back — DONE (2026-08-30, Session 2)

Closed the last piece of Phase 2: applying a worker's Fusion-managed worktree branch back into the coordinator's tree, on explicit request only (Grok Build's own "apply" pattern, never automatic).

- **`crates/jcode-protocol/src/lib.rs`**: `AgentInfo` gained `worktree_path: Option<PathBuf>` — populated server-side (`server/client_comm_context.rs::handle_comm_list`) only when the member's `working_dir` passes `swarm_worktree::is_managed_worktree_path()`, so an ordinary shared directory can never be mistaken for something safe to merge against.
- **`swarm_worktree.rs`**: three new primitives — `branch_name_for_worktree` (pure inverse of `create_worktree`'s own naming), `worktree_is_clean` (`git status --porcelain`), and `merge_worktree_branch` (`MergeOutcome::{Merged, Conflict}`). Merges are always `--no-ff` (explicit merge commit, never indistinguishable from the coordinator's own history); a dirty worktree is refused before git ever runs; a conflicted merge is unconditionally `git merge --abort`ed before returning, so the coordinator's tree is never left mid-merge.
- **`tool/communicate.rs`**: new `apply` action on the `swarm` tool (aliases `merge`/`merge_back`/`apply_worktree`), takes `target_session`. Runs entirely in-process — no new server `Request`/`ServerEvent` pair — since tool execution already shares a process with `SwarmState`; `fetch_swarm_members` already scopes to the caller's own swarm, so a `target_session` outside it is rejected as an ownership check, not just a lookup miss.
- **Deliberately not done**: no automatic conflict resolution (reported back as text, a human or follow-up turn handles it), no automatic worktree/branch deletion after a successful merge (the existing terminal-member pruning sweep from the cleanup slice already handles the worktree once the member goes terminal; the branch ref is left behind, same as `remove_worktree`'s own documented behavior), no support for targeting anything other than whatever the coordinator currently has checked out.
- **Real, thorough testing** — 7 new tests in `swarm_worktree.rs`, all against real `git` operations, not mocks: branch-name derivation round-trips through an actual `create_worktree()` call; a dirty worktree is refused and the coordinator's tree is confirmed untouched; a clean commit merges and the coordinator's own working tree gets the file; a genuine conflicting edit on both sides produces `Conflict` *and* the repo is verified to have zero `MERGE_HEAD`/unmerged state afterward; a nonexistent branch fails cleanly.
- **One real regression caught and fixed before it shipped, not after**: adding the `apply` blurb to the `swarm` tool's `action` parameter description initially broke both `tool_parameter_descriptions_stay_under_token_cap` (pushed to ~47 tokens, over the 25 cap) and a pre-existing test asserting the description's exact wording (`schema_requires_a_nonblank_label_for_spawn`, which checks for the substring "spawn requires label"). Trimmed to `"Action. spawn requires label and prompt. apply merges a worktree branch, needs target_session."` (~19 tokens) — keeps the substring the old test depends on, stays under cap.
- Full `cargo test -p jcode-app-core --lib`: **1231 passed, 29 failed** — confirmed via `git stash`/re-run that all 29 are the standing pre-existing baseline (including the 4 known-flaky `communicate_*` end-to-end timing tests, reproduced identically with and without this slice's changes). Zero regressions. Full `jcode-fusion` binary rebuilds clean, zero new warnings.

**This completes Phase 2 in full** (worktree-per-subagent isolation: creation, cleanup, merge-back).

## Merge-back: post-ship review fix (2026-08-30, same session, via agy)

Delegated a code review of the merge-back slice to `agy` (Gemini 3.1 Pro) — full `swarm_worktree.rs` plus the commit diff as context, asked to check correctness, shell-injection risk, merge/conflict/abort edge cases, and simplification opportunities. Every finding was verified against the actual code before acting on it, not taken on faith.

**One real high-severity bug, fixed**: the conflict/abort path checked only `Err(e)` from spawning `git merge --abort`, never its exit status — if abort itself failed (e.g. a held index lock), the function still returned `MergeOutcome::Conflict` as if the repo had been cleanly reverted, while it could actually still be sitting mid-merge. Fixed by checking `MERGE_HEAD` to know whether a merge was genuinely in progress (closes a related edge case the same fix surfaced: a *coordinator*-side dirty tree makes git refuse before `MERGE_HEAD` ever exists, so unconditionally calling abort there would itself fail with a confusing "no merge to abort") and by checking the abort call's real exit status, bailing loudly instead of silently reporting false safety.

**Two low-severity items, also fixed**: conflict-file paths with spaces were reported with literal quote characters from `git status --porcelain`'s quoting — now trimmed. `is_managed_worktree_path` now requires *both* the managed root and the candidate path to actually canonicalize (previously fell back to an uncanonicalized path on either side), closing an edge case where a crafted, nonexistent `.../worktrees/x/../../secrets`-shaped path could pass a component-wise prefix check — real exploitability was negligible since `working_dir` is never populated from arbitrary strings in practice, but the fix was free.

**Confirmed clean, not changed**: the shell-injection question — all `git` calls go through `tokio::process::Command` with separate `.arg()`s, never a shell, so no injection surface regardless of what a branch name or path contains.

New regression test (`merge_worktree_branch_reports_a_dirty_coordinator_tree_cleanly`) covers the coordinator-side-dirty edge case directly. `swarm_worktree.rs`: 20/20 (was 19). Full suite: 1232 passed, 29 failed — same standing baseline, zero regressions.

## Next steps (pick up here)

1. **Phase 2 is done.** Phases 3 (ACP gaps, orchestration-as-script) and 4 (memory consolidation) haven't been started — see the source-level findings sections above for what's already scoped.
2. **Remaining follow-up work from Phase 1, not blocking**: Guardian's auto-approve half (needs a real semantic judge), Linux/Windows sandboxing, execpolicy's resubmit-with-justification flow for user rules.
3. **Follow-up from this slice, not blocking**: no automated conflict-resolution path yet (a conflict just gets reported back as text); merged worktrees/branches aren't proactively cleaned up beyond the existing terminal-member sweep picking up the worktree — a leftover `jcode-swarm/*` branch ref after a successful merge is harmless but will accumulate over time, worth a cheap follow-up.
4. **Lesson from the rewind slice, keep applying**: run the *full* `cargo test -p jcode-app-core --lib` after every slice that touches shared infrastructure — done consistently through all three worktree slices now, caught real issues twice (the token-cap regression here, the original tool-description cap violation in Phase 0).
5. Manual/live TUI verification (running `jcode-fusion` interactively with a real login, and specifically trying `JCODE_FUSION_SWARM_WORKTREES=1` with a real swarm spawn, then a real `apply` call) still hasn't happened beyond example-based/unit-level demos — this remains the single biggest unverified assumption in the whole project. Worth prioritizing over further phases at some point.
6. Update this file at the end of every session — status table, session log entry, next steps — before ending. Session 2 will also write `claude-code-build/SESSION_2_MEMORY.md` alongside `SESSION_1_MEMORY.md` before ending, not silently let it go stale.

## Housekeeping reminder
`jcode-fusion/jcode/target/` is already 4.8GB after one debug build — same kind of build-cache directory that ballooned to 4.9GB in the user's real `~/.jcode/scratch/`. It's safe to `rm -rf target/` any time disk gets tight; nothing of value lives there. Keep an eye on it periodically so this fork doesn't silently repeat that problem long-term.
