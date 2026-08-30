# jcode-fusion: Porting Codex Harness + Grok Build's Best Features into jcode

**Status:** Design doc only — no code written yet.
**Author context:** Synthesized from three deep research passes (Aug 2026) into `1jehuang/jcode`, `openai/codex`, and `xai-org/grok-build`, plus a base-harness decision pass and a source-size-based sizing estimate.
**Goal:** Pick the single best of the three harnesses as the foundation, then graft the other two's best orchestration/looping/swarm/safety features onto it — producing a "best of the best" unified harness, maintained as an independent fork.

---

## 1. Executive summary

**jcode is the chosen base harness.** The deciding factor isn't RAM efficiency or its TUI — it's **multi-provider OAuth/multi-account auth**, which neither Codex Harness nor Grok Build has (both are single-vendor tools tied to their own subscription or API billing). jcode's 40+ provider profiles with built-in OAuth login is the entire reason a "bring your own subscriptions, no API billing" harness is possible at all, so it's preserved untouched as the anchor feature — not one item in a feature list.

On top of that base, this doc recommends porting nine specific features from Codex Harness and Grok Build across five phases, explicitly rejects a handful of features that don't transfer well, and flags one important due-diligence item (Grok Build's disclosed telemetry incident) that must gate any direct code reuse from that repo. It also resolves a specific tension: jcode's own swarm mode is weaker than Grok Build's, but jcode's swarm *coordinator* (the cross-process orchestration layer) is stronger than anything Codex or Grok Build have — so the fix is to keep jcode's coordinator and replace only its conflict-resolution mechanism (see §2.2).

**Verification status**: All nine items have now been checked against jcode's actual cloned source (`v0.81.2`, not just docs/web research) and revised accordingly — see §6 for the specific corrections per item. This process found real, material corrections in every single phase, not just Phase 0 — including one outright wrong premise (issue #1090, cited as justification for Phase 2, turned out to already be fixed) and one pleasant surprise (jcode's ACP adapter and swarm plan persistence are both far more mature than the original web-research-based doc assumed). **Takeaway for future phases of this project**: doc/web research is a reasonable starting hypothesis but consistently needed correction against real source — never skip the source-read step for a new phase just because it worked out fine before.

Because jcode's maintainer (`1jehuang`) explicitly discourages large external PRs (see §3), this project should be planned as an **independently maintained fork**, not an upstream contribution.

---

## 2. Base harness decision

### 2.1 Why jcode, not Codex Harness or Grok Build

| | Codex Harness | Grok Build | jcode |
|---|---|---|---|
| Auth model | OpenAI-only: ChatGPT subscription or OpenAI API key | xAI-only: Grok/SuperGrok subscription or xAI API key | **40+ provider profiles, built-in OAuth per provider, multi-account switching (`/account`)** — Claude, OpenAI, Gemini, Copilot, Azure, OpenRouter (itself proxying dozens more), custom OpenAI-compatible endpoints |
| Vendor lock-in | Single vendor | Single vendor | **None — this is the point** |
| RAM/session | Not optimized for this; large multi-hundred-crate, cloud-integrated design | Same | ~28MB (embeddings off) |
| Cross-process swarm coordinator | No — subagents are in-process only | No — subagents are in-process only, or manual multi-terminal + git | **Yes** — server-owned `VersionedPlan`, DM/broadcast messaging, role model |
| License | Apache-2.0, CLA required to contribute back | Apache-2.0, no external PRs accepted at all | MIT, no CLA, simple to fork |

Nothing in Codex's or Grok Build's own auth systems is worth porting — jcode's multi-auth is strictly broader. **This is item #0 in the feature list below: keep as-is, do not touch.**

### 2.2 Resolving the swarm tension

jcode's own swarm *conflict-resolution* is genuinely weaker than Grok Build's (optimistic/no-lock/social DM-based resolution vs. Grok Build and Codex's independently-converged worktree-per-subagent isolation). But jcode's swarm *coordinator* — the actual cross-process orchestration layer with a persisted plan graph and structured agent roles — doesn't exist in either competitor; their "swarm" is in-process subagents only, or (for Grok Build) literally just "run multiple terminals and let git sort it out."

**Resolution: keep jcode's coordinator, replace its conflict mechanism.** jcode's `VersionedPlan`/coordinator becomes the merge-orchestrator sitting on top of per-worker git worktrees (the pattern both Codex and Grok Build landed on independently). Conflicts then surface as ordinary git merge conflicts at merge-back time instead of requiring agents to negotiate via DM mid-edit. Best of both, not a swap of one for the other — see item #2 in §5.

---

## 3. Current state: jcode baseline

Source: `1jehuang/jcode` on GitHub, ~18.7k stars, ~7,237 commits (nearly all from a single maintainer).

| Aspect | Current state |
|---|---|
| **Language / build** | Rust, Cargo workspace, ~90 member crates under `crates/`. Custom profiles (`release`, `selfdev`, `release-lto`). |
| **License** | MIT (Jeremy Huang, 2025). No CLA. Simple to fork. |
| **Auth** | **40+ provider profiles with built-in OAuth login flows and multi-account switching (`/account`)** — the anchor feature, see §2.1. Keep as-is. |
| **Agent loop** | `crates/jcode-app-core/src/agent/` — `turn_execution.rs`, `turn_loops.rs`, `streaming.rs`, `compaction.rs`, `interrupts.rs`, etc. |
| **Swarm** | `crates/jcode-app-core/src/server/swarm.rs` + `jcode-swarm-core`. **Optimistic, no locking** — conflicts are *detected* via file-touch notifications and resolved socially (DM between agents), not prevented. One-level fan-out by default; `swarm-deep` mode allows recursion under a budget cap. Plan state lives in a server-owned `VersionedPlan`, not a repo file. See §2.2 for the fix. |
| **Autonomy** | **Three subsystems, verified against real source (not just docs)**: Ambient Mode is a plain polling loop, not a state machine — "gardening/scouting/working" is prompt text only; its "adaptive usage-aware scheduling" is dead code in production (real call sites always pass `None`). Overnight Mode (`overnight.rs::run_supervisor`) is the mature one — one long-lived agent, wall-clock-milestone-driven, with genuinely working budget wiring via `crate::usage::fetch_all_provider_usage()`. Self-Dev mode is a build/reload pipeline (`BuildRequestState`, `ReloadPhase`) — a different category of thing, not a task-completion loop. **Also found: an orphaned `Mission`/`MissionStatus` module** (`crates/jcode-app-core/src/mission.rs`) already shaped almost exactly like a unified state machine (`Active/Paused/Blocked/NeedsDecision/BudgetLimited/Complete/Abandoned`) — dead code, only its read-path is wired, never finished. **Separately, jcode also has an unrelated, shipped `Goal`/"initiative" tool** (`jcode-task-types`, `jcode-base/src/goal.rs`) — a manual, durable cross-session task/milestone tracker, nothing to do with autonomous loops; naming anything here "Goal Engine" would collide with it (see §6). |
| **Memory** | Local ONNX embeddings (`all-MiniLM-L6-v2` via `tract-onnx`, no Python dependency), graph-based cascade retrieval (BFS over `HasTag`/`InCluster`/`RelatesTo`/`Supersedes` edges, `0.7^depth` decay), JSON-on-disk storage under `~/.jcode/memory/`. A real differentiator — already comparable to what Codex/Grok Build do. "Ambient Garden" deep consolidation is explicitly a documented TODO, not yet implemented. |
| **Extensibility** | No formal plugin API. Extensibility = MCP servers (`~/.jcode/mcp.json`), lifecycle hooks (`pre_tool` synchronous gate + `post_tool`/session observers, `docs/HOOKS.md`), embedding-similarity-injected skills, or literal self-modification of Rust source via Self-Dev mode. |
| **Sandboxing** | No evidence of OS-level sandboxing (bubblewrap/Seatbelt/ACL-based) beyond hook-based permission gating. |
| **Protocol/embeddability** | `src/cli/` includes an ACP protocol adapter, but scope/completeness wasn't confirmed as covering the full base ACP surface. |
| **Maintainer posture** | `CONTRIBUTING.md` explicitly states large/generated PRs are treated as proposals, not mergeable diffs — "may be closed even when the underlying idea is good." Confirms this must be a **hard fork**, independently maintained. |
| **Architecture docs** | Unusually thorough for a solo-maintainer project: `docs/CRATE_OWNERSHIP_BOUNDARIES.md`, `docs/MODULAR_ARCHITECTURE_RFC.md` — a real map for where to graft new code without destabilizing the TUI or provider layers. |
| **Known open issue** | ~~[#1090](https://github.com/1jehuang/jcode/issues/1090)~~ **— already fixed as of this fork's base version.** Original web research found it open ("patch ready, not yet merged"); a direct source/`git log` check found it's actually resolved by commit `0a66fbcd2` ("fix: preserve live headless workers during idle checks"), included in `v0.81.2`. Left here as a record that doc/web research can be stale even for "still open" claims — always re-check against actual source before using an issue as justification. |

---

## 4. Feature source: Codex Harness (`openai/codex`, Apache-2.0)

OpenAI's Aug 20, 2026 release formally open-sourced "Harness" — the core execution engine (`codex-rs/`) that had already been Apache-2.0 since the original April 2025 release. The standout, source-verified capabilities:

### 4.1 Goal system (`codex-rs/ext/goal/`)
A persisted `ThreadGoal` record with a real state machine: `Active → Paused/Blocked/UsageLimited/BudgetLimited/Complete`. Token and wall-clock budget accounting (`accounting.rs`) is enforced by the runtime, not the prompt. After each turn, if the goal isn't done, a continuation-steering prompt is auto-injected and the next turn starts automatically. A hard-coded rule prevents declaring `blocked` before the same blocking condition recurs for **3 consecutive turns** — a concrete anti-premature-failure guard.

### 4.2 Guardian (`codex-rs/core/src/guardian/`)
An LLM-as-judge that auto-adjudicates sandbox-escalation approval requests against a codified risk taxonomy (`guardian/policy.md`): Data Exfiltration, Credential Probing, Persistent Security Weakening, Destructive Actions. It fails closed on timeout (90s), execution failure, or malformed output, and hard-caps consecutive denials per turn (3 general / 1 for "cyber" actions) to prevent thrash. Critically, it never *grants* access the sandbox itself wouldn't otherwise enforce — it only decides whether to interrupt the human.

### 4.3 Layered OS sandboxing
- **Linux**: bubblewrap-based, with path-specificity-aware nested read/write policy resolution (a narrower writable child can re-open under a denied parent path), symlink-escape defense (`/dev/null` bind-mounted over symlinked protected paths), `PR_SET_NO_NEW_PRIVS` + seccomp-BPF network filter. Legacy Landlock LSM fallback exists.
- **macOS**: Seatbelt (`sandbox-exec`) profiles, keeps `.git`/`.codex` read-only even under workspace-write.
- **Windows**: a genuinely deep dedicated crate (`windows-sandbox-rs`) — restricted tokens, ACL-based deny-read walker, Windows Filtering Platform (WFP) network policy, elevated/unelevated backend split. Fails closed rather than silently running weaker if a policy can't be enforced.

### 4.4 Network proxy (`codex-rs/network-proxy/`)
Local HTTP (127.0.0.1:3128) + SOCKS5 (127.0.0.1:8081) proxy enforcing domain allow/deny lists **and method-level policy even over HTTPS** (local ephemeral-CA MITM lets it restrict to GET/HEAD/OPTIONS), header-stripping hooks (e.g. strip auth on GitHub write endpoints), audit events per decision.

### 4.5 Execpolicy (`codex-rs/execpolicy/`)
Starlark-based command-prefix classification (`allow`/`prompt`/`forbidden`) with load-time unit-testable rule files (`match`/`not_match` examples baked in) — materially more rigorous than regex allowlists.

### 4.6 Multi-agent: spawn graph + worktree binding
`agent-graph-store` persists a directed parent→child spawn graph with `Open/Closed` edge lifecycle — real, inspectable lineage. `codex-rs/worktree` binds each spawned subagent to its own git worktree, giving conflict-free concurrent file writes across agents. Spawn depth is capped.

### 4.7 Two-phase memory consolidation (`codex-rs/memories/`)
Phase 1: parallel, leased/claimed background jobs extract structured memories per session with retry backoff. Phase 2: a single global lock serializes consolidation via a dedicated no-network, no-approval sub-agent writing `MEMORY.md`/`skills/` under a git-baselined directory.

### 4.8 `app-server`
The engine exposed as a standalone JSON-RPC server, decoupled from the CLI/TUI — precedent for pluggable frontends (IDE extensions, desktop apps).

**License**: Apache-2.0, standard terms, CLA required only for *contributing back*, not for use/forking.

**Explicitly not worth porting**: cloud-tasks client and enterprise-auth crates (`aws-auth`, `workload-identity`) are OpenAI-hosted-product-specific; `code-mode`'s gRPC/tonic execution path is a separate architectural bet (generate-and-execute-code-as-tool-call) not clearly a win for jcode's model. **Auth system**: not portable/relevant — single-vendor by design (see §2.1).

---

## 5. Feature source: Grok Build (`xai-org/grok-build`, Apache-2.0)

Open-sourced July 15, 2026 — reactively, three days after a security researcher found the CLI silently uploading full developer repos (including tracked `.env`/SSH keys) to an xAI-controlled GCS bucket. **This history means any literal code reuse from this repo must be security-audited first; treat it as a source of design patterns to reimplement, not code to copy.**

### 5.1 `/goal` mode — adversarial completion verification
The standout idea. The working agent is **explicitly forbidden from self-grading completion** — a decoupled, independent verifier (sub-agent or test suite) whose entire job is to *refute* the completion claim must sign off before a goal is marked done. This is complementary to Codex's Goal system, not redundant: Codex contributes budget/state rigor, Grok Build contributes completion-integrity rigor.

### 5.2 Rewind / rollback
Every turn creates a checkpoint. `/rewind` does a dry-run preview, detects externally-modified files before touching anything, and — the key safety property — **refuses to rewind rather than guess** if it can't prove a safe reconstruction of pre-compaction state from its internal event log.

### 5.3 `/workflow` — orchestration-as-script
Multi-agent orchestration compiled into an actual executable script — confirmed via GitHub's language-breakdown API (`Rhai: 22,893 bytes` in the repo), corroborating the earlier secondary-source claim that Rhai (an embeddable Rust scripting language) backs this feature. Saved to `.grok/workflows/`, replayable as a slash command with arguments, hard agent-call budget (default 128, cap 1024), live dashboard of phases/token counts.

### 5.4 Worktree-per-subagent isolation
Identical pattern to Codex's `worktree` crate, arrived at independently: each subagent gets its own git worktree; conflicts surface at merge time via plain git rather than a bespoke resolver. **Two independent frontier labs converging on the same pattern is a strong signal this is the right approach** — this is what jcode's swarm conflict-resolution is upgraded to, per §2.2.

### 5.5 ACP (Agent Client Protocol) support
First-class JSON-RPC implementation (`initialize`, `session/new`, `session/prompt`, `session/update`) with vendor-specific capability layered cleanly via `x.ai/*` extension namespacing (`x.ai/fs/*`, `x.ai/git/*`, `x.ai/git/worktree/*`, plus rewind/task-state extensions). This is what gets Grok Build embedded into Zed, Neovim, Emacs, and marimo for free — one protocol implementation, many editor integrations.

### 5.6 What Grok Build does *not* have
Despite "swarm" marketing language, there is **no first-party cross-process/multi-machine swarm coordinator** — no leader election, no shared distributed task queue. "Swarm" in their own ecosystem talk means either in-process subagents (worktree-isolated) or manually running multiple terminals and letting git resolve overlap. This is exactly why jcode's coordinator is worth keeping (§2.2) rather than replacing wholesale.

**License**: Apache-2.0. xAI does not accept external PRs/patches (source-transparency release, not community-governed) — irrelevant to forking rights, relevant only to the (moot) question of upstreaming. **Auth system**: not portable — single-vendor by design (see §2.1).

---

## 6. Synthesis: recommended features, prioritized

> **Naming correction (post source-read):** jcode already ships an unrelated, shipped `Goal`/"initiative" tool (durable manual task/milestone tracker — `jcode-task-types`, `jcode-base/src/goal.rs`). Calling item #1 "Goal Engine" would collide with it, so it's renamed **Mission Engine** — fittingly, since the actual foundation to build on is an orphaned module literally called `Mission` (`crates/jcode-app-core/src/mission.rs`), already shaped almost exactly right (`MissionStatus` already has a `BudgetLimited` state) but never finished being wired up.
>
> **Scope decision: Self-Dev mode is excluded from Mission Engine.** It's a build/reload pipeline (queue states, live-reload handshake), not a task-completion loop — forcing it into the same abstraction as Ambient/Overnight isn't a real win. Mission Engine unifies **Ambient + Overnight + the revived Mission module** only.

| # | Feature | Source(s) | Disposition / why |
|---|---|---|---|
| 0 | **Multi-provider OAuth / multi-account auth** | jcode | **Keep as-is — do not touch.** This is *why* jcode is the base (§2.1), not one feature among several. |
| 1 | **Unified Mission Engine** *(renamed from "Goal Engine" — see callout below)* — Codex's budget-aware state machine + Grok's adversarial-verifier completion gate | Codex + Grok Build | **Revised after a real source read (see callout below).** Consolidates Ambient + Overnight (**not** Self-Dev — see callout) into one coherent, auditable primitive with both budget rigor *and* completion-integrity rigor. Grafts onto the existing but orphaned `Mission`/`MissionStatus` (`crates/jcode-app-core/src/mission.rs`) — finishing its write path, not designing a new state machine — with budget sourced from `crate::usage::fetch_all_provider_usage()` and a driver loop modeled on `overnight.rs::run_supervisor`. |
| 2 | **Worktree-per-subagent swarm isolation** under jcode's existing coordinator | Codex + Grok Build (convergent) | **Shipped in full: creation + cleanup + merge-back.** New `crates/jcode-app-core/src/swarm_worktree.rs`. **Creation**: wired into `spawn_swarm_agent` right after working-dir resolution — opt-in (`JCODE_FUSION_SWARM_WORKTREES=1`), only in the "would otherwise share the parent's dir" fallback path, fails open on error. **Cleanup**: wired into jcode's *existing* terminal-member pruning sweep (`server.rs::prune_expired_terminal_swarm_members`, already runs periodically) rather than new scheduling — when a terminal member's retention window expires, its worktree (if any, detected via a path-prefix check against `~/.jcode/worktrees/`, not a new `SwarmMember` field) is removed in the same pass, fire-and-forget so a slow `git` call never blocks the sweep. `remove_worktree_self_contained` needed only the worktree's own path (no separately-tracked repo root) — verified by hand with a real `git worktree add` + `git -C <worktree> worktree remove <worktree>` that this actually works before relying on it. **Merge-back** (Session 2): a new `apply` action on the `swarm`/`communicate` tool (`target_session` required) — applies a worker's worktree branch into the coordinator's own tree via `git merge --no-ff`, on explicit request only (Grok Build's own "apply" pattern, never automatic). Runs entirely in-process (`AgentInfo` gained a `worktree_path` field, gated to only surface genuinely Fusion-managed paths); refuses to merge a dirty worktree; a conflicted merge is unconditionally aborted before returning, so the coordinator's tree is never left mid-merge — verified with a real two-sided conflicting edit, confirming zero leftover `MERGE_HEAD` state. `VersionedPlan` confirmed already durably persisted, not ephemeral. The existing advisory file-touch conflict *detection* is untouched — this adds a structural layer on top. Fully live-tested against real `git` operations (not mocked) across all three slices — 19/19 in `swarm_worktree.rs` alone, including two concurrent worktrees proven genuinely independent and a conflicted merge proven to leave the repo clean. Also confirmed stale: issue #1090 already fixed upstream, dropped as justification. And: "one-level fan-out" is a root/deep-mode boolean gate not a depth counter, and the TUI's "worktree manager" role is aspirational dead comment text. **Still open, deliberately deferred**: no automatic conflict resolution, no automatic branch/worktree deletion after a successful merge (existing cleanup sweep still handles the worktree; the branch ref is left behind). |
| 3 | **Guardian-style auto-approval reviewer** | Codex | **Decided and shipped, ambient-scoped, deny-only.** New `crates/jcode-app-core/src/guardian.rs`, wired into `tool::ambient::RequestPermissionTool::execute` right before the existing `system.request_permission(request)` call — a `Deny` verdict skips the human queue entirely; `Undecided` falls through unchanged. Deterministic keyword-based adjudication against Codex's own four risk categories (destructive action, credential probing, data exfiltration, persistent security weakening). **Deliberately never auto-approves** — a trustworthy approve verdict needs either a real LLM judge (not reliably buildable/testable without live provider credentials in this dev environment) or a far more structured risk signal than the free-text fields callers provide today; auto-approving on keyword-*absence* would be a much weaker, overclaiming signal than denying on keyword-*presence*. Interruption reduction (the auto-approve half) is honest follow-up work, not faked here. **Still open**: execpolicy (#6) vs. the existing `jcode-command-risk` classifier — does it replace, layer on top, or migrate rule tables into Starlark? Not yet decided. |
| 4 | **Provable-safe rewind** (refuse-rather-than-guess after compaction) | Grok Build | **Revised after a real source read.** jcode already has `/rewind` (`Agent::rewind_to_message`/`undo_rewind`, `turn_execution.rs`) — the gap isn't "no rollback exists," it's specific: the undo snapshot is **in-memory only** (doesn't survive a restart), holds only **one** level (a 2nd rewind overwrites the 1st), and is message-only (no filesystem/tool-side-effect awareness). Genuinely good news found alongside this: compaction (`compaction.rs`) never actually destroys raw messages — it's a cursor over an immutable slice — so reconstructing pre-compaction state is mostly already possible from data that's already there, not a from-scratch problem. |
| 5 | **Layered OS sandboxing** (bwrap/Landlock Linux, Seatbelt macOS, ACL/WFP Windows) | Codex | **Decided and macOS slice shipped**: whole-process (re-exec the entire binary under `sandbox-exec`, not just wrap the bash tool's subprocess spawn — avoids the file-edit-tool blind spot, since `WriteTool` etc. write in-process via `tokio::fs::write` with no subprocess to wrap). First slice (`crates/jcode-app-core/src/sandbox_macos.rs`) is deliberately conservative — `(allow default)` + deny writes to a curated credential-path list, not Codex-style deny-by-default — opt-in via `JCODE_FUSION_SANDBOX=1`. **Real bug caught during verification**: macOS Seatbelt matches `subpath` rules against the canonicalized path; `/var/folders/...` (mktemp/TMPDIR) is actually a symlink to `/private/var/...`, so an unresolved-path profile silently protects nothing. Fixed by canonicalizing `$HOME` before building the profile — a genuine, non-obvious gotcha worth remembering for any future path-based sandbox rule in this project. Linux (bwrap)/Windows not started yet. |
| 6 | **Execpolicy-as-Starlark** command classification | Codex | **Decided and shipped: extends `jcode-command-risk`, doesn't replace it.** New `crates/jcode-app-core/src/execpolicy.rs`, checked after the built-in classifier already returned `Allow` (user rules can only escalate to `Confirm`/`Deny`, never downgrade an existing built-in restriction — enforced structurally). Lives in `jcode-app-core`, not inside `jcode-command-risk` itself, since that crate is deliberately zero-dependency by design (its own doc comments: "not another model") — adding a Starlark interpreter there would contradict its whole ethos. Rules are simple `"prefix\|decision\|reason"` strings in a `RULES` list rather than Starlark dicts (kept to the parts of the `starlark` crate's API this could be gotten right confidently), but the *script* itself is real Starlark — loops/comprehensions/`def` all work for generating the list. **Honest gap**: `Confirm` and `Deny` currently behave identically (no resubmit-with-justification flow for user rules yet, unlike the built-in classifier's own `Confirm`). **This completes Phase 1.** |
| 7 | **ACP support** (close specific gaps + `jcode.dev/*` vendor extensions) | Grok Build | **Revised after a real source read: jcode's ACP adapter (`src/cli/acp.rs`, 2,188 lines) is NOT a stub** — it's a real, actively-shipped JSON-RPC server (`initialize`, `session/new`, `session/load`/`resume`, streaming `session/prompt`, `session/cancel`/`close`, extension mechanism), confirmed via changelog cross-check as actively developed. The actual gap is narrower: it never sends client-side callback methods (`fs/read_text_file`, `session/request_permission`, `terminal/*`) — everything is handled server-side, never delegated to the ACP host, which real hosts (Zed etc.) generally expect for editor-integrated UX. Also: no auth negotiation over ACP, session-scoped MCP explicitly rejected, `initialize` ignores the client's requested protocol version. |
| 8 | **Orchestration-as-script** (`/workflow`-style templates) | Grok Build | **Shipped: templates + tool wiring, no scripting yet.** New `crates/jcode-app-core/src/workflow_template.rs` (`WorkflowTemplate`/`TemplateNode`/`WorkflowParameter`, `{{param}}` substitution) plus a dedicated `crates/jcode-app-core/src/tool/workflow.rs` tool (`save`/`list`/`run`, `run` seeds a real task graph via the existing `Request::CommSeedGraph`). Persisted at `~/.jcode/workflows/<name>.json` via the same atomic-write-with-recovery primitives `rewind_store.rs` already uses. **Two scoping corrections, both source-verified before/during implementation**: `starlark = "0.14"` was already added to this crate in Phase 1 (for execpolicy) — stays greenfield-free by not adding `rhai`, reuse Starlark later if template scripting is ever needed. And: `TemplateNode` was originally modeled on `PlanItem`, then retargeted to match `TaskGraphNodeSpec` (the shape the real `task_graph`/`seed_graph` integration point actually takes — `kind`/`depends_on`/`priority: u8`, not `subsystem`/`file_scope`) once that integration point was actually read, before any tool wiring got built on the wrong shape. `VersionedPlan` confirmed already durably persisted (item #2) — this extends that with the templating capability, doesn't rebuild persistence. 20 tests total (13 template + 7 tool), real-disk round-trip and daemon-free fast-fail paths tested (not mocked). A second agy review pass on the tool wiring caught and fixed a real issue: `list()` echoed a template's `name` verbatim into tool-output text with no charset check, so a crafted name could inject a fake list entry or spoof terminal output — `validate()` now restricts `name` to a safe charset. **Not yet done**: `run`'s daemon round trip has never been exercised live; no Starlark scripting inside a template. |
| 9 | **Two-phase memory consolidation** (leased jobs + single-locked consolidator) | Codex | Formalizes jcode's own documented "Ambient Garden" TODO — confirmed via real code read that this is genuinely unimplemented (only embedding-backfill is wired today, fire-and-forget after each ambient cycle). Builds on existing `MemoryManager`/`MemoryGraph` (`jcode-base/src/memory.rs`, `jcode-memory-types/src/graph.rs`, which already has `cascade_retrieve`). |

**Cross-cutting finding affecting #1, #8, and #9 together**: jcode has **no generic background-job scheduler** anywhere — the only periodic-loop primitive is Ambient Mode's own runner, tightly coupled to spawning a full LLM session. Mission Engine's supervisor loop (#1, modeled on `overnight.rs::run_supervisor`) is a real candidate to become the shared scheduling primitive that orchestration-as-script (#8) and memory consolidation (#9) both also need, rather than each phase inventing its own ad hoc scheduling. Worth deciding explicitly before Phase 3/4 start.

### Explicitly not recommended
- Codex's and Grok Build's **auth systems** — single-vendor, strictly narrower than jcode's (§2.1).
- Codex's cloud-tasks client and enterprise-auth crates (`aws-auth`, `workload-identity`) — not portable to a local harness.
- Codex's `code-mode` gRPC/tonic path — separate architectural bet, not a clear win here.
- **Any Grok Build telemetry/upload-adjacent code** — audit and discard given the disclosed incident; don't reuse even as a reference implementation.

### License note
jcode is MIT; both sources are Apache-2.0. Porting *ideas and patterns* into a reimplementation is clean. If any literal Apache-2.0 code is copied rather than reimplemented, retain NOTICE attribution for those specific portions even though the overall project stays MIT-licensed (Apache-2.0 carries a patent grant + NOTICE-passthrough obligation). **Project rule going forward: reimplement in idiomatic jcode-style Rust rather than copy-pasting source** — cleaner licensing, and necessary anyway since crate boundaries and existing abstractions differ between all three codebases.

---

## 7. Size estimate

Measured via the GitHub API (`/repos/{owner}/{repo}` and `/repos/{owner}/{repo}/languages`) on 2026-08-29 — real data, not guesses:

| Repo | Rust source (bytes) | Repo size (incl. all langs/history/assets) | Stars |
|---|---|---|---|
| `1jehuang/jcode` | 24.9 MB | 451 MB | 18.8k |
| `openai/codex` | 54.8 MB | 582 MB | 119.7k |
| `xai-org/grok-build` | 65.3 MB | 41 MB | 26.2k |

(Grok Build's small repo-size-vs-source-size ratio suggests a lean checkout with little bundled history; jcode and Codex both carry substantially more non-source weight — likely assets, docs, and in jcode's case a separate iOS client.)

**Rough LOC estimate** (bytes-of-source ÷ ~40–55 bytes/line, a standard heuristic — not a verified line count):
- **jcode (current base): ~450k–620k LOC**
- Codex Harness: ~1.0M–1.4M LOC (reference only — not adopted wholesale)
- Grok Build: ~1.2M–1.6M LOC (reference only — not adopted wholesale)

**Projected fused harness — jcode base + the 9 ported features, reimplemented (not copy-pasted) in idiomatic jcode-style Rust:**

| Feature | Estimated added LOC | Notes |
|---|---|---|
| #1 Unified Mission Engine | 1,000–3,000 | Lower than originally estimated — finishing an existing orphaned module (`mission.rs`) plus budget/verifier hookup, not designing a state machine from scratch |
| #2 Worktree-per-subagent isolation | 2,000–4,000 | Mostly glue over existing git tooling |
| #3 Guardian reviewer | 1,500–3,000 | Policy taxonomy + judge harness |
| #4 Provable-safe rewind | 1,000–2,000 | Layers onto existing `compaction.rs` |
| #5 Layered sandboxing (Linux+macOS first) | 8,000–15,000 | The largest single addition; full Windows parity roughly doubles this later |
| #6 Execpolicy-as-Starlark | 1,000–2,000 | Bulk of the work is an external `starlark-rust` dependency |
| #7 ACP support | 3,000–6,000 | JSON-RPC base + `jcode.dev/*` extensions |
| #8 Orchestration-as-script (Rhai) | 2,000–4,000 | `rhai` is an external interpreter crate; this is the API surface around it |
| #9 Two-phase memory consolidation | 1,500–3,000 | Builds on existing memory crates |
| **Total net-new/adapted code** | **~21,000–42,000 LOC** | **≈ 4–7% growth over jcode's current base** |

**Crate count**: revised down slightly now that #1 is confirmed as extending existing in-tree code (`crates/jcode-app-core/src/mission.rs`) rather than a new crate. Roughly 9–14 new crates (`jcode-sandbox-linux`, `jcode-sandbox-macos`, `jcode-execpolicy`, `jcode-acp`, `jcode-workflow-script`, `jcode-guardian`, etc. — **not** `jcode-goal`, to avoid the naming collision noted in §6), taking the workspace from ~90 to **~99–104 crates**. *(This list is itself only confirmed for #1 — items #2/#3/#5/#6/#7/#8/#9 haven't had their exact crate-vs-in-tree-module split verified against real source yet; expect adjustment as each phase gets verified.)*

**Runtime footprint projection** (jcode's own README benchmarks are the only *measured* numbers we have — 27.8MB RAM/session with embeddings off, 167MB with embeddings on, ~14ms startup):
- **RAM, baseline single session**: most new features are lazy/on-demand — sandboxing only activates per-command, Guardian only spins up on an escalation request, ACP server mode is an alternate run mode, Rhai loads only when a saved workflow runs. Expect baseline to stay close to today's number: **projected ~30–35MB** (small increase from static linkage of new deps even when unused).
- **Binary size**: not measured in research for jcode's actual compiled binary (only source size was available). Comparable Rust CLIs with similar dependency weight (ONNX runtime, TUI rendering, TLS) typically land 30–80MB release/LTO; expect **+5–15MB** growth from the new crates (Starlark and Rhai interpreters are the biggest binary-size additions; sandboxing crates are mostly thin syscall wrappers).
- **Swarm/concurrent RAM**: scales per active worktree-bound worker, same cost model jcode already has today — unchanged.

### 7.1 Actual on-disk footprint (measured from GitHub release assets)

The LOC/RAM numbers above are about code size and memory, not disk space. Pulled directly from each repo's GitHub Releases API (jcode `v0.81.2`; Codex `rust-v0.150.1`; Grok Build has **no GitHub Releases published at all** — no prebuilt binaries distributed that way, so no comparable figure exists):

| | jcode (compressed download) | Codex CLI (compressed download) |
|---|---|---|
| macOS (arm64) | 48.2 MB | 87.2 MB (`.tar.gz`) / 61.8 MB (`.zst`) |
| macOS (x64) | 50.9 MB | 95.3 MB / 68.4 MB |
| Linux (x64) | 46.1 MB | 98.3 MB / 70.2 MB |
| Windows (x64) | 39.6 MB (`.tar.gz`) / 122.5 MB (raw `.exe`) | 99.1–103.9 MB (`.tar.gz`/`.zip`) |

Two important caveats on reading this table:
- These are **compressed download sizes**, not installed size. Rust binaries typically decompress to roughly 1.5–2.2x their compressed size — so jcode's actual installed binary is more realistically **~75–110 MB** on disk, not 46–51 MB.
- **This is Codex's core CLI binary alone.** A full Codex install that also wants `app-server` (JSON-RPC engine, ~50–80 MB compressed per platform) and the sandboxing helper (`bwrap`, small) stacks meaningfully higher — realistically **300–500+ MB on disk** for the full engine, consistent with Codex being the heaviest, most cloud-integrated of the three. jcode's single-binary distribution model is a real structural advantage here, not just a RAM-benchmark talking point.
- Codex also ships debug-symbol bundles (`codex-symbols-*.tar.gz`) up to **473.9 MB** — these are optional, not part of a normal install, included only for reference on how much heavier the unstripped build is.

**Projected on-disk footprint for the fused harness**, scaling jcode's real numbers by the LOC-growth estimate above and accounting for the two heaviest additions (Starlark and Rhai interpreters add fixed binary bulk disproportionate to their own LOC; sandboxing crates are thin syscall/ACL wrappers and add relatively little):
- **Compressed download**: ~55–65 MB per platform (up from jcode's current 46–51 MB)
- **Installed binary on disk**: ~85–130 MB (up from jcode's current ~75–110 MB estimate)
- **Fork's git checkout** (if building from source rather than downloading a release binary): jcode's own repo is 451 MB on disk as a full clone (history + assets + the separate iOS client); the fork would start near that and grow modestly with new crates' source — call it **~460–490 MB**.
- **Cargo build cache** (`target/`, transient — not part of "installed" size, deletable after building): commonly several GB for a ~100-crate Rust workspace. Not a real research figure, just standard Rust-tooling knowledge — expect **2–6 GB** during active development, none of it needed by an end user who just downloads a release binary.
- **Runtime data directory** (`~/.jcode/`, grows with usage, not with the codebase): memory graph + embedding vectors stay small even after heavy use (each memory's vector is 384 floats ≈ 1.5 KB; thousands of memories is still only a few MB). The main long-term grower is session transcript history (JSONL), which — like any long-running agent CLI's session logs — could reach tens to low hundreds of MB after months of heavy daily use. New worktrees from the swarm rework (#2) are transient, cleaned up after each merge, and share the `.git` object store, so they cost roughly one working-tree's worth of disk per concurrently active swarm worker, not a full repo clone each.

**Bottom line for "how much disk space will it consume": realistically ~85–130 MB installed for the binary itself, plus a runtime data directory that stays in the tens-of-MB range for a long time under normal use** — nowhere close to Codex's ~300–500 MB full-engine footprint. Building from source instead of installing a release binary adds the ~460–490 MB checkout and multi-GB (but deletable) build cache on top of that.

**Caveat**: these are estimates from source-size heuristics and feature-scope judgment, not measurements of code that exists yet. Real numbers only come from actually implementing Phase 0 and re-measuring.

---

## 8. Phased roadmap

**All five phases below are now source-verified against jcode's actual `v0.81.2` code (not just docs/web research) as of 2026-08-29** — each phase's description reflects real findings, not the original assumptions. See §6 for the specific corrections per item.

- **Phase 0 — Foundation**: Unified Mission Engine (#1) + provable-safe rewind (#4). These touch the core turn loop; everything else assumes a stable loop underneath. Concrete first slice: finish `Mission`'s write path (give `mission::set()` real callers) before touching budget enforcement, verification, or the supervisor loop.
- **Phase 1 — Safety** (parallelizable with Phase 0): Guardian reviewer (#3) + execpolicy (#6) + layered sandboxing (#5, macOS/Seatbelt first — dev machine is macOS, Linux/bwrap validated later via Docker/CI). Self-contained, doesn't depend on Mission Engine. **Two open decisions must be resolved before starting** (see §6 items #3/#5/#6): Guardian's scope (ambient-only vs. general sessions) and execpolicy's relationship to the already-existing `jcode-command-risk` classifier; plus the file-edit-tool sandboxing coverage gap (whole-process vs. helper-process sandbox).
- **Phase 2 — Swarm rework**: worktree-per-subagent isolation (#2). Issue #1090 is **already fixed** in this fork's base version — dropped as justification; the rework is now purely about conflict-resolution quality, still worth doing.
- **Phase 3 — Ecosystem**: ACP support (#7, now scoped narrowly to client-callback delegation gaps, not a from-scratch build) + orchestration-as-script (#8, extends existing `VersionedPlan` persistence). Additive, surface-level, lowest risk.
- **Phase 4 — Memory**: two-phase consolidation (#9). Lowest urgency since jcode's memory system is already competitive as-is. **Resolved (PROGRESS.md)**: Mission Engine's supervisor loop is fused to agent-turn-continuation semantics, not a generic periodic-task primitive — don't force it into this role. Orchestration-as-script (#8) turned out not to need periodic scheduling at all (it's on-demand template execution); memory consolidation's periodic piece should model on Ambient Mode's existing runner instead.

---

## 9. Open risks & considerations

- **Fork posture is mandatory, not optional.** jcode's `CONTRIBUTING.md` discourages large/generated PRs; this must be planned and resourced as an independently maintained fork from day one.
- **Solo-maintainer velocity risk.** jcode has ~7,237 commits with ~98% from a single author. A fork inherits divergence risk every time upstream jcode moves fast — rebasing strategy should be decided before Phase 0 starts.
- **Grok Build provenance.** Given the disclosed telemetry incident, any Phase 2–3 work touching Grok Build-derived patterns needs an explicit security review step before merging, not just before initial porting.
- **Sandboxing scope is large.** #5 alone (three OS-specific sandbox backends) is comparable in scope to the rest of the roadmap combined — a Linux-first, macOS-second, Windows-later sequencing is recommended given jcode's own stated cross-platform support (Linux/macOS/Windows/Termux); reflected in the Phase 1 note and the size estimate above.
- **Size estimates are pre-implementation.** The §7 numbers are directional, not committed — re-measure after Phase 0 lands and adjust later-phase estimates accordingly.
- **Verification is done for all 5 phases (as of 2026-08-29), but stays perishable.** All nine items were checked against real source and corrected — see §6. However, this fork is pinned to `v0.81.2`; if upstream jcode moves meaningfully before a given phase actually starts, re-check that phase's specific claims again rather than trusting this pass indefinitely (issue #1090 being fixed between when web research found it "open" and when the source read found it closed is a direct example of exactly this kind of drift).

---

## 10. Sources

- [`1jehuang/jcode`](https://github.com/1jehuang/jcode) — README, `docs/SWARM_ARCHITECTURE.md`, `docs/SWARM_TASK_GRAPH.md`, `docs/AMBIENT_MODE.md`, `docs/MEMORY_ARCHITECTURE.md`, `docs/SERVER_ARCHITECTURE.md`, `docs/CRATE_OWNERSHIP_BOUNDARIES.md`, `docs/MODULAR_ARCHITECTURE_RFC.md`, `CONTRIBUTING.md`, [issue #1090](https://github.com/1jehuang/jcode/issues/1090), GitHub API (`/repos/1jehuang/jcode`, `/languages`)
- [`openai/codex`](https://github.com/openai/codex) — `codex-rs/ext/goal/`, `codex-rs/core/src/guardian/`, `codex-rs/linux-sandbox/`, `codex-rs/windows-sandbox-rs/`, `codex-rs/network-proxy/`, `codex-rs/execpolicy/`, `codex-rs/agent-graph-store/`, `codex-rs/worktree/`, `codex-rs/memories/`; [Subagents docs](https://learn.chatgpt.com/docs/agent-configuration/subagents); [Security/Sandboxing docs](https://learn.chatgpt.com/docs/security); [Open Source For You](https://www.opensourceforu.com/2026/08/openai-open-sources-codex-harness/); GitHub API (`/repos/openai/codex`, `/languages`)
- [`xai-org/grok-build`](https://github.com/xai-org/grok-build) — user-guide docs (`15-agent-mode.md`, `16-subagents.md`, `17-sessions.md`, `19-plan-mode.md`, `20-background-tasks.md`); [x.ai/news/introducing-goal](https://x.ai/news/introducing-goal); [x.ai/news/workflows](https://x.ai/news/workflows); [zed.dev/acp](https://zed.dev/acp); [DevOps.com on the telemetry incident](https://devops.com/xai-open-sources-grok-build-coding-agent-after-cloud-upload-exposes-ssh-keys-repos/); GitHub API (`/repos/xai-org/grok-build`, `/languages`)
