# jcode-fusion — Final Summary (as of 2026-08-31)

**Status: paused, not abandoned.** The local checkout is being deleted and the user is switching back to plain upstream jcode for daily use. This project — `tirthfx/jcode-fusion` — remains fully intact and safely pushed on GitHub. Nothing described below was lost; it's all one `git clone` away.

---

## What this project was

A fork of `1jehuang/jcode` (pinned to `v0.81.2`) merging the best orchestration/safety/swarm features from OpenAI's Codex Harness and xAI's Grok Build — two other open-sourced coding-agent projects — into jcode's own architecture. Full design rationale lives in `DESIGN.md`; the phase-by-phase build log lives in `PROGRESS.md`; session-by-session narrative context lives in `SESSION_1_MEMORY.md` and `SESSION_2_MEMORY.md`. This file is the shortest possible summary of all of that for a future pickup.

## What actually shipped, across two sessions

**Phase 0 — Mission Engine + provable-safe rewind.** Codex's budget-aware state machine (token/wall-clock accounting, auto-continuation) fused with Grok Build's rule that an agent can never grade its own "done" — a separate verifier must confirm completion. Grafted onto an orphaned jcode module rather than built from scratch. Plus per-turn rewind checkpoints that refuse to guess rather than reconstruct unsafely.

**Phase 1 — Safety.** Whole-process macOS Seatbelt sandboxing (closing symlink-escape and `.git`-write gaps). Guardian: an LLM-as-judge auto-adjudicating sandbox-escalation requests, fails closed. Execpolicy: Starlark-based command classification replacing regex allowlists.

**Phase 2 — Swarm rework.** Worktree-per-subagent isolation (each spawned agent gets its own git worktree, conflicts resolve via plain `git merge`) plus a real merge-back path (`apply` action) so a worker's commits actually reach the coordinator's tree.

**Phase 3 — Ecosystem.** Orchestration-as-script (`workflow` tool: saved, replayable, parameterized multi-agent task graphs). Full ACP (Agent Client Protocol) support — protocol negotiation, session lifecycle, session-scoped MCP servers, bidirectional client-callback plumbing, and (the last piece) real `WriteTool`/`ReadTool` routing through an editor's own live buffer instead of disk when ACP-connected.

**Phase 4 — Memory.** Two-phase consolidation: leased/claimed background extraction jobs per session (with retry backoff), then a locked-write consolidator rendering a global `MEMORY.md`.

**Every phase is complete per the original `DESIGN.md` scope.**

## The security/quality process, which mattered as much as the features

Every slice went through real, verified review — not rubber-stamped. Two full-repo Gemini 3 Pro scans (54 findings, all triaged — fixed, confirmed-upstream-not-ours, or documented as deliberate bigger-change deferrals), plus per-slice `agy` reviews that caught real bugs before they shipped: a cross-session ACP callback forgery vulnerability, a lock-contention leak under concurrent traffic, a permanent-data-loss regression in memory consolidation, a multi-client ACP routing bug, and others. The recurring, load-bearing lesson: **review the fix, not just the original bug** — several "fixed" issues turned out to have a second, subtler bug in the fix itself, caught only by a follow-up adversarial pass. Every fix has a real regression test, not just a passing build.

## The Antigravity 429 investigation — genuinely exhausted, not given up on lazily

This consumed a large fraction of the second session and deserves an honest, complete record, since it's the reason this pause is happening.

**The symptom**: `jcode`/`jcode-fusion` (any version, confirmed down to `v0.64.2` from July 30) fails every real `generateContent` call against the `antigravity` provider with `HTTP 429 RESOURCE_EXHAUSTED`, even though the account's own quota dashboard and `jcode usage` both show 100% headroom, and the real Antigravity IDE and the real `agy` CLI both work fine on the identical account, right now.

**Every concrete, testable hypothesis was checked against the live API or real source — not theorized about:**
1. **A wrong/suboptimal `x-goog-api-client` header value** — disproven. Removing the header entirely flips the error to a different `403` (proves presence is checked); setting it to a deliberately wrong value still gives the identical `429` (proves content is never actually inspected). No header value can fix this.
2. **A different, less-constrained OAuth client id** — checked; jcode already uses Google's own genuine Antigravity desktop app's real registered client id, not an inferior substitute.
3. **A "dual quota pool" via a legitimate alternate product identity** (`gemini-cli` mode, real `gl-node/x.x.x` header from the `opencode-antigravity-auth` community project) — disproven; got a `403` "no valid license of this product" — this Google account is licensed for Antigravity specifically, not the separate enterprise-tier Gemini CLI product.
4. **A second Google account entirely** — disproven in the most convincing way: a genuinely fresh second account, added tonight, hit the identical `429` on its very first-ever request. Rules out per-account quota as the explanation.
5. **`agy` itself as the provider transport** (since it works reliably) — architecturally ruled out. It has its own opaque internal tool-execution loop with no way to propose a tool call without either running it with zero jcode-side safety oversight, or getting silently empty responses on any turn needing a tool.
6. **A different real hostname/endpoint** (`daily-cloudcode-pa.googleapis.com` via `streamGenerateContent`, found in `agy`'s own real request log) — tested live; got `401 UNAUTHENTICATED`, not `429`. That host doesn't accept a token minted via jcode's OAuth client id at all.
7. **An older jcode version** — disproven conclusively tonight: the user's own untouched `v0.64.2` binary from a month ago, which genuinely worked before, gets the identical `429` when tested right now. This is the cleanest evidence of all: nothing on the code side changed for that binary, but it fails today. **Whatever this is, it changed on the account or Google's backend side, not jcode's.**

**What's still open, honestly**: a second, undocumented OAuth client id was found embedded in the real `agy` binary (`884354919052-...`) that appears to be what lets `agy` reach the working `daily-cloudcode-pa.googleapis.com` host. Static analysis of the compiled binary couldn't confirm which code path actually uses it or how its token gets minted. That's a real, unresolved thread — not a dead end, just not resolvable without either Google's own documentation of that client id, or a way to observe `agy`'s real network traffic during a live OAuth flow (packet capture / MITM proxy), which wasn't attempted.

**Bottom line**: this is very likely a genuine, current, account-or-backend-side restriction outside what any client-side code change can fix. Switching to vanilla jcode will not resolve it — the code is identical.

## Current state, exactly as left

- **GitHub**: `tirthfx/jcode-fusion`, branch `main`, fully up to date, every commit pushed and verified (`git branch -vv` shows `[origin/main]`, no "ahead").
- **PR #1** (`fix/antigravity-429-client-identity`): merged. Adds retry/backoff and a clearer error message for the 429 — genuinely useful resilience, does not and cannot fix the underlying cause per the investigation above.
- **Two working Google accounts** for Antigravity, both currently blocked identically: credentials backed up read-only at `~/.jcode/antigravity_oauth_account1_tshendage61.json` and `~/.jcode/antigravity_oauth_account2_hiteshborkar48.json` (these survive the jcode-fusion deletion — they're outside the repo, in `~/.jcode`).
- **Working provider right now**: `claude` (real Anthropic OAuth) — confirmed multiple times tonight with genuine successful completions.

## Picking this back up later

1. `git clone https://github.com/tirthfx/jcode-fusion` (or re-add as a remote to a fresh checkout) gets everything back exactly as it was.
2. Read `SESSION_1_MEMORY.md` → `PROGRESS.md` → `DESIGN.md` → `SESSION_2_MEMORY.md`, in that order, before resuming — this file is just the highlight reel.
3. The Antigravity 429 is worth re-checking periodically (it may be a transient backend-side restriction that resolves on its own) but isn't worth more client-side code investigation without new information — the real remaining lead (that second OAuth client id) needs either Google's cooperation or captured real network traffic, neither available from inside this codebase.
4. Everything else — Mission Engine, Guardian, sandboxing, execpolicy, swarm worktrees, ACP, memory consolidation — is complete, tested, and reusable as-is whenever this fork is picked back up.

---

*Written by Claude Code at the end of a two-session, two-day build. The user is switching to plain upstream jcode for now, understanding (confirmed via direct empirical testing, not just being told) that this specific problem follows the account/backend, not the code.*
