<div align="center">

# jcode-fusion

**A fork of [`jcode`](https://github.com/1jehuang/jcode) fusing in the best orchestration, safety, and swarm ideas from OpenAI's [Codex Harness](https://github.com/openai/codex) and xAI's [Grok Build](https://github.com/xai-org/grok-build).**

[![License: MIT](https://img.shields.io/badge/license-MIT-blue?style=flat-square)](LICENSE)
[![Base](https://img.shields.io/badge/base-jcode%20v0.81.2-blue?style=flat-square)](https://github.com/1jehuang/jcode)
[![Status](https://img.shields.io/badge/status-paused%2C%20not%20abandoned-orange?style=flat-square)](claude-code-build/PROJECT_FINAL_SUMMARY.md)

[What this is](#what-this-is) · [What shipped](#what-shipped) · [Build docs](#build-docs) · [Status](#current-status) · [Credits](#credits)

</div>

---

## What this is

`jcode` was chosen as the base harness over Codex Harness and Grok Build for one reason: **multi-provider OAuth / multi-account auth**. Neither competitor has it — both are single-vendor tools tied to their own subscription or API billing. jcode's 40+ provider profiles with built-in OAuth login is the entire reason a "bring your own subscriptions, no API billing" harness is possible at all — so it's kept completely untouched here, not one feature among several.

On top of that base, this fork ports nine specific features from Codex Harness and Grok Build across five phases — reimplemented in idiomatic jcode-style Rust, not copy-pasted (jcode is MIT, both donor projects are Apache-2.0).

Full rationale for every decision — including what was rejected and why — lives in [`DESIGN.md`](DESIGN.md).

## What shipped

| Phase | Feature | What it does |
|---|---|---|
| **0 — Foundation** | **Mission Engine** | Budget-aware state machine (token/wall-clock accounting, auto-continuation) + a rule that an agent can never grade its own "done" — a separate verifier must confirm completion. Plus per-turn rewind checkpoints that refuse to guess rather than reconstruct unsafely. |
| **1 — Safety** | **Guardian, sandboxing, execpolicy** | Whole-process macOS Seatbelt sandboxing. An LLM-as-judge ("Guardian") auto-adjudicating sandbox-escalation requests, fails closed. Starlark-based command classification replacing regex allowlists. |
| **2 — Swarm rework** | **Worktree-per-subagent isolation** | Every spawned agent gets its own git worktree; conflicts resolve via plain `git merge` instead of mid-edit negotiation. A real `apply` action merges a worker's commits back into the coordinator's tree. |
| **3 — Ecosystem** | **Workflows + full ACP** | Orchestration-as-script (`workflow` tool: saved, replayable, parameterized multi-agent task graphs). Full Agent Client Protocol support — session lifecycle, session-scoped MCP servers, bidirectional client callbacks. |
| **4 — Memory** | **Two-phase consolidation** | Leased/claimed background extraction jobs per session (with retry backoff), then a locked-write consolidator rendering a global `MEMORY.md`. |

Every phase is complete per the original `DESIGN.md` scope.

**Quality process**: two full-repo AI code scans surfaced 54 findings, every one triaged — fixed, confirmed pre-existing, or deliberately deferred, never rubber-stamped. Real bugs caught before shipping: a cross-session ACP callback forgery, a lock-contention leak, a permanent-data-loss regression in memory consolidation, and others. The recurring lesson: **review the fix, not just the original bug** — several "fixed" issues had a second, subtler bug hiding inside the fix itself, caught only by a follow-up adversarial pass.

## Build docs

The full build history is preserved, in order:

1. [`DESIGN.md`](DESIGN.md) — the original design doc: why jcode was chosen as the base, what was ported from where, and why
2. [`claude-code-build/SESSION_1_MEMORY.md`](claude-code-build/SESSION_1_MEMORY.md) — session 1 narrative
3. [`PROGRESS.md`](PROGRESS.md) — phase-by-phase build log
4. [`claude-code-build/SESSION_2_MEMORY.md`](claude-code-build/SESSION_2_MEMORY.md) — session 2 narrative
5. [`claude-code-build/PROJECT_FINAL_SUMMARY.md`](claude-code-build/PROJECT_FINAL_SUMMARY.md) — the short version of all of the above

## Current status

**Paused, not abandoned.** All five phases are complete, tested, and reusable as-is. The one open item — a persistent `429` on the Antigravity provider — was investigated exhaustively (see `PROJECT_FINAL_SUMMARY.md` for the full seven-hypothesis writeup) and traced to a real, already-acknowledged Google bug affecting Jio-managed Google AI Pro subscriptions, tracked upstream at [Google Issue Tracker #525093265](https://issuetracker.google.com/issues/525093265) — not a jcode-fusion (or jcode) code problem.

Picking this back up: `git clone` this repo and read the build docs above in order.

## Credits

- [`1jehuang/jcode`](https://github.com/1jehuang/jcode) — the base harness this fork builds on. MIT license.
- [`openai/codex`](https://github.com/openai/codex) (Codex Harness) — source of the Mission Engine, Guardian, and sandboxing patterns. Apache-2.0.
- [`xai-org/grok-build`](https://github.com/xai-org/grok-build) — source of the adversarial-verification, worktree-isolation, and ACP patterns. Apache-2.0.

This is an independently maintained fork, not an upstream contribution — jcode's `CONTRIBUTING.md` discourages large external PRs, so this project is planned and resourced as a hard fork from day one.
