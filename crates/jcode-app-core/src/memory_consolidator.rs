//! Fusion Phase 4, phase 2 of DESIGN.md item #9 ("two-phase memory
//! consolidation"): the single-global-lock consolidator that renders the
//! current memory graph into a human-readable `MEMORY.md`.
//!
//! Phase 1 (`memory_consolidation.rs` — leasing, real extraction, ambient-
//! runner wiring) decides *which sessions get their memories extracted at
//! all*. This module is a different, later stage entirely: given whatever
//! memories already exist (extracted by phase 1, or by the pre-existing
//! interactive-CLI-exit path, or hand-entered), render them into one
//! reviewable document.
//!
//! **Deliberately scoped to the rendering + locked-write primitive only,
//! same "smallest coherent first slice" shape every phase in this project
//! has taken.** What this slice does NOT do, not silently pretended
//! solved:
//! - No sub-agent. The design (`PROGRESS.md`/`DESIGN.md`) calls for "a
//!   dedicated no-network, no-approval sub-agent" to actually run this —
//!   spawning one safely (a real agent session with genuinely restricted
//!   tool access, not just a documented convention) is separate,
//!   real infrastructure work, not attempted here. `render_memory_document`
//!   and `write_consolidated_memory_file` are the primitives such a
//!   sub-agent would call, built and tested standing alone first.
//! - No `skills/` generation. Only the `MEMORY.md` half of "MEMORY.md and
//!   skills/" from the original design phrasing.
//! - No triggering mechanism at all (not wired into the ambient runner,
//!   not exposed as a tool action). Nothing calls this automatically yet.
//! - No git-commit step. "Under a git-baselined directory" in the original
//!   design meant the *target* location is expected to already be a git
//!   repo (so the user's own git history is the audit trail for what
//!   changed) — this module writes the file; committing it is left to
//!   whatever future trigger calls this, not attempted here.

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use crate::memory_types::MemoryEntry;

/// Serializes every gather-render-write pass — the "single global lock"
/// half of this phase's own name. Same shape as `rewind_store.rs`'s
/// `REWIND_STORE_LOCK` and `memory_consolidation.rs`'s `LEASE_STORE_LOCK`:
/// an unguarded pass here could race a concurrent consolidation pass on
/// both *what the current memory set even is* and the file write itself,
/// leaving `MEMORY.md` with content from two overlapping renders.
///
/// **In-process only, same verified reasoning as `LEASE_STORE_LOCK`
/// (`memory_consolidation.rs`), not re-derived from scratch here**: every
/// background loop in this codebase runs via `tokio::spawn` inside the
/// single daemon process, not separate OS processes -- confirmed there
/// against `server.rs` directly. A `std::sync::Mutex` is sufficient for how
/// this module is actually invoked (nothing calls it yet, but whatever
/// eventually does will live in that same process, matching every other
/// background trigger in this codebase). The same caveat applies too: a
/// hypothetical future separate CLI invocation running consolidation
/// alongside a live daemon would not be covered by this lock -- not fixed
/// preemptively for a call path nothing uses.
static CONSOLIDATION_LOCK: Mutex<()> = Mutex::new(());

fn lock_consolidation() -> std::sync::MutexGuard<'static, ()> {
    CONSOLIDATION_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Render the given memories into a `MEMORY.md`-shaped document. Pure —
/// takes `generated_at` as a parameter rather than calling `Utc::now()`
/// internally, so this is deterministically testable (same convention
/// `session/maintenance.rs::prune_old_session_backups_in` already uses,
/// parameterizing "now" for its own caller to supply).
///
/// Only `active` memories are included — a memory that's been superseded
/// (`superseded_by: Some(_)`) or explicitly deactivated has already been
/// judged not-current by whatever marked it that way; including it in a
/// human-facing summary document would misrepresent what's actually still
/// believed true. Grouped by category, each group sorted by descending
/// confidence (most-trusted first) then alphabetically by content for a
/// stable order when confidence ties (real, not incidental: a stable order
/// means re-running this on an unchanged memory set produces byte-identical
/// output, so a git diff on `MEMORY.md` only ever shows genuine content
/// changes, never reordering noise).
pub fn render_memory_document(memories: &[MemoryEntry], generated_at: DateTime<Utc>) -> String {
    let mut doc = String::new();
    doc.push_str("# Memory\n\n");
    doc.push_str(&format!(
        "_Consolidated automatically by jcode-fusion on {} — regenerated each \
         consolidation pass, edits here will be overwritten._\n\n",
        generated_at.format("%Y-%m-%d %H:%M UTC")
    ));

    // Gemini review, 2026-08-30: filter straight into the grouping map
    // instead of collecting an intermediate `active: Vec<&MemoryEntry>`
    // first -- one pass, one fewer allocation.
    let mut by_category: std::collections::BTreeMap<String, Vec<&MemoryEntry>> =
        std::collections::BTreeMap::new();
    for entry in memories
        .iter()
        .filter(|entry| entry.active && entry.superseded_by.is_none())
    {
        by_category
            .entry(entry.category.to_string())
            .or_default()
            .push(entry);
    }

    if by_category.is_empty() {
        doc.push_str("_Nothing consolidated yet._\n");
        return doc;
    }

    for (category, mut entries) in by_category {
        entries.sort_by(|a, b| {
            // Gemini review, 2026-08-30: two real determinism gaps fixed
            // here, both verified against the actual types before
            // patching, not assumed.
            // (1) `f32::partial_cmp` returns `None` for a NaN confidence,
            //     previously mapped to `Equal` -- `total_cmp` (stable since
            //     1.62) gives a real total order that includes NaN instead
            //     of silently treating it as tied with everything.
            // (2) The tie-break on `content` alone left two entries with
            //     identical confidence *and* identical content ordered only
            //     by whatever order `memories` happened to arrive in
            //     (`sort_by` is stable) -- not a property of the data, so
            //     not actually deterministic despite the doc comment above
            //     claiming it. `id` is unique and stable regardless of
            //     input order, so tie-breaking on it last closes the gap
            //     for real.
            b.confidence
                .total_cmp(&a.confidence)
                .then_with(|| a.content.cmp(&b.content))
                .then_with(|| a.id.cmp(&b.id))
        });

        doc.push_str(&format!("## {}\n\n", category_heading(&category)));
        for entry in entries {
            // Flatten embedded newlines (both conventions -- CRLF left an
            // orphaned \r behind if only \n was handled, a real gap a
            // Gemini review caught) so one entry can never accidentally
            // break out of its own markdown list item.
            let content = entry.content.replace(['\r', '\n'], " ");
            doc.push_str(&format!("- {content}\n"));
        }
        doc.push('\n');
    }

    doc
}

/// Title-case (and, for the four known built-in categories, correctly
/// pluralize) a category string for the section heading. `MemoryCategory`'s
/// built-in variants are all lowercase single words via their own `Display`
/// impl; `Custom(String)` could be anything a caller chose.
///
/// Gemini review, 2026-08-30, two real fixes:
/// (1) A naive "+s" turned "entity" into "Entitys" -- wrong for the one
///     built-in category that doesn't pluralize that way. The four known
///     categories get their real plural form explicitly; only a genuinely
///     unknown `Custom` value falls back to the naive "+s" (an honest
///     limitation for arbitrary custom text, not something a general
///     English pluralizer belongs in this module to solve).
/// (2) `Custom(String::new())` (a degenerate but constructible case)
///     previously produced a blank `## \n\n` heading. Falls back to
///     "Uncategorized" instead of rendering an empty, structurally-odd
///     section header.
fn category_heading(category: &str) -> String {
    match category {
        "fact" => return "Facts".to_string(),
        "preference" => return "Preferences".to_string(),
        "entity" => return "Entities".to_string(),
        "correction" => return "Corrections".to_string(),
        "" => return "Uncategorized".to_string(),
        _ => {}
    }
    let mut chars = category.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str() + "s",
        None => "Uncategorized".to_string(),
    }
}

/// Gather the current memory set from `manager` and write the rendered
/// document to `target_path`, holding [`CONSOLIDATION_LOCK`] across
/// *both* the gather and the write.
///
/// **Real gap fixed here, caught by an agy (Gemini 3.1 Pro) review before
/// this had any caller to break**: the original signature took an
/// already-gathered `memories: &[MemoryEntry]`, meaning whatever gathered
/// that list (e.g. `manager.list_all()`) necessarily ran *outside* this
/// function, and therefore outside the lock entirely. Two concurrent
/// consolidation passes could each gather a snapshot, then both enter this
/// function serialized by the lock -- but the lock only ever protected the
/// second one from clobbering the first one's *write*, not from the two
/// passes racing on what the "current" memory set even was to begin with.
/// Taking `&MemoryManager` and calling `list_all()` here, inside the lock,
/// means the read-current-state-then-render-then-write sequence is what's
/// actually serialized, not just its last step.
///
/// Uses `jcode_storage`'s atomic write (temp file + rename, the same
/// primitive every other persisted-state module in this project already
/// uses) rather than a plain `std::fs::write`, so a crash mid-write can
/// never leave `MEMORY.md` truncated or corrupt.
pub fn write_consolidated_memory_file(
    target_path: &Path,
    manager: &crate::memory::MemoryManager,
    generated_at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let _guard = lock_consolidation();
    let memories = manager.list_all()?;
    let document = render_memory_document(&memories, generated_at);
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    crate::storage::write_bytes(target_path, document.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory_types::{MemoryCategory, TrustLevel};

    fn entry(category: MemoryCategory, content: &str, confidence: f32, active: bool) -> MemoryEntry {
        let now = Utc::now();
        MemoryEntry {
            id: format!("mem-{content}"),
            category,
            content: content.to_string(),
            tags: Vec::new(),
            search_text: String::new(),
            created_at: now,
            updated_at: now,
            access_count: 0,
            source: None,
            trust: TrustLevel::Medium,
            strength: 1,
            active,
            superseded_by: None,
            reinforcements: Vec::new(),
            embedding: None,
            embedding_model: None,
            confidence,
        }
    }

    fn fixed_time() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-08-30T12:00:00Z")
            .expect("valid fixed timestamp")
            .with_timezone(&Utc)
    }

    #[test]
    fn renders_a_placeholder_when_there_is_nothing_active() {
        let doc = render_memory_document(&[], fixed_time());
        assert!(doc.contains("Nothing consolidated yet"));
    }

    #[test]
    fn excludes_inactive_and_superseded_memories() {
        let mut superseded = entry(MemoryCategory::Fact, "old fact", 0.9, true);
        superseded.superseded_by = Some("mem-newer".to_string());
        let inactive = entry(MemoryCategory::Fact, "retracted fact", 0.9, false);
        let live = entry(MemoryCategory::Fact, "current fact", 0.9, true);

        let doc = render_memory_document(&[superseded, inactive, live], fixed_time());
        assert!(doc.contains("current fact"));
        assert!(!doc.contains("old fact"));
        assert!(!doc.contains("retracted fact"));
    }

    #[test]
    fn groups_by_category_with_pluralized_headings() {
        let doc = render_memory_document(
            &[
                entry(MemoryCategory::Fact, "a fact", 0.5, true),
                entry(MemoryCategory::Preference, "a preference", 0.5, true),
            ],
            fixed_time(),
        );
        assert!(doc.contains("## Facts"));
        assert!(doc.contains("## Preferences"));
    }

    #[test]
    fn sorts_by_descending_confidence_within_a_category() {
        let doc = render_memory_document(
            &[
                entry(MemoryCategory::Fact, "low confidence", 0.1, true),
                entry(MemoryCategory::Fact, "high confidence", 0.9, true),
            ],
            fixed_time(),
        );
        let high_pos = doc.find("high confidence").expect("present");
        let low_pos = doc.find("low confidence").expect("present");
        assert!(
            high_pos < low_pos,
            "higher-confidence entries must render first"
        );
    }

    #[test]
    fn ties_break_alphabetically_for_a_stable_deterministic_order() {
        let doc_a = render_memory_document(
            &[
                entry(MemoryCategory::Fact, "zebra fact", 0.5, true),
                entry(MemoryCategory::Fact, "apple fact", 0.5, true),
            ],
            fixed_time(),
        );
        let doc_b = render_memory_document(
            &[
                entry(MemoryCategory::Fact, "apple fact", 0.5, true),
                entry(MemoryCategory::Fact, "zebra fact", 0.5, true),
            ],
            fixed_time(),
        );
        assert_eq!(
            doc_a, doc_b,
            "input order must not affect output -- a rerun on an unchanged \
             memory set must produce byte-identical output"
        );
        assert!(doc_a.find("apple fact") < doc_a.find("zebra fact"));
    }

    #[test]
    fn ties_break_on_id_when_confidence_and_content_both_match() {
        // Regression for a real gap an agy review caught: two entries with
        // identical confidence *and* identical content (a legitimate case
        // -- a near-duplicate memory that wasn't deduped) previously fell
        // back to whichever order they arrived in `memories` (`sort_by` is
        // stable), which is a property of the *caller*, not the data --
        // not actually deterministic despite this function's own claim.
        let mut first = entry(MemoryCategory::Fact, "duplicate content", 0.5, true);
        first.id = "mem-b-later-id".to_string();
        let mut second = entry(MemoryCategory::Fact, "duplicate content", 0.5, true);
        second.id = "mem-a-earlier-id".to_string();

        // Same two entries, opposite input order -- output must be
        // identical regardless, driven by `id` now that confidence and
        // content can no longer distinguish them.
        let doc_forward = render_memory_document(&[first.clone(), second.clone()], fixed_time());
        let doc_reversed = render_memory_document(&[second, first], fixed_time());
        assert_eq!(doc_forward, doc_reversed);
    }

    #[test]
    fn flattens_embedded_newlines_so_content_cannot_break_the_markdown_list() {
        let doc = render_memory_document(
            &[entry(
                MemoryCategory::Fact,
                "line one\nline two",
                0.5,
                true,
            )],
            fixed_time(),
        );
        assert!(doc.contains("- line one line two"));
    }

    #[test]
    fn write_consolidated_memory_file_round_trips_to_disk() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("nested").join("MEMORY.md");

        let manager = crate::memory::MemoryManager::new_test();
        manager
            .remember_project(crate::memory_types::MemoryEntry::new(
                MemoryCategory::Fact,
                "a real fact",
            ))
            .expect("seed memory");

        write_consolidated_memory_file(&target, &manager, fixed_time()).expect("write");

        let written = std::fs::read_to_string(&target).expect("read back");
        assert!(written.contains("a real fact"));
        assert!(written.contains("2026-08-30 12:00 UTC"));
    }

    #[test]
    fn write_consolidated_memory_file_overwrites_a_previous_render() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("MEMORY.md");

        let manager = crate::memory::MemoryManager::new_test();
        manager
            .remember_project(crate::memory_types::MemoryEntry::new(
                MemoryCategory::Fact,
                "first pass fact",
            ))
            .expect("seed first pass");
        write_consolidated_memory_file(&target, &manager, fixed_time()).expect("first write");

        // A genuinely fresh memory state for the "second pass" -- clearing
        // test storage rather than accumulating onto the first manager, so
        // this actually exercises "the file reflects whatever the memory
        // store looks like *now*", not "the file grows forever".
        manager.clear_test_storage().expect("clear for second pass");
        manager
            .remember_project(crate::memory_types::MemoryEntry::new(
                MemoryCategory::Fact,
                "second pass fact",
            ))
            .expect("seed second pass");
        write_consolidated_memory_file(&target, &manager, fixed_time()).expect("second write");

        let written = std::fs::read_to_string(&target).expect("read back");
        assert!(written.contains("second pass fact"));
        assert!(
            !written.contains("first pass fact"),
            "a fresh render must replace the prior content, not append to it"
        );
    }
}
