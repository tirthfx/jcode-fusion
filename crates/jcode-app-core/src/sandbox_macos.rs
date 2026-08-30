//! Fusion Phase 1: whole-process macOS sandboxing (DESIGN.md §6 item #5).
//!
//! **Deliberately conservative first slice, not Codex-style read-only-root-
//! by-default.** jcode has no existing OS-level sandboxing at all (confirmed
//! by source grep — see PROGRESS.md), and a fully-correct deny-by-default
//! allow-list profile is easy to get wrong in a way that breaks the entire
//! app (can't read its own shared libraries, can't write temp files, can't
//! reach the network for LLM calls). Getting that wrong is worse than not
//! sandboxing at all. So this slice does the safer, still genuinely useful
//! thing: **allow everything by default, explicitly deny writes to a
//! curated list of high-value credential/secret paths** — blast-radius
//! reduction against the worst failure modes (credential theft,
//! credential destruction), not a full lockdown. A stricter, allow-listed
//! profile is a real next step once this is proven safe in practice, not
//! something to attempt in one pass.
//!
//! **Why whole-process, not just wrapping the bash tool's subprocess spawn**:
//! file-edit tools (`WriteTool` etc.) write directly in-process via
//! `tokio::fs::write` — a sandbox that only wraps `bash.rs::build_shell_command`
//! would silently miss every file write/edit. This re-execs the *entire*
//! `jcode-fusion` binary under `sandbox-exec` at startup instead, so no tool
//! (in-process or subprocess) is exempt.
//!
//! **Opt-in, not default-on.** Given the risk of a subtly wrong profile
//! breaking normal operation, this only activates when explicitly requested
//! (`JCODE_FUSION_SANDBOX=1`). Fails open on any error building/applying the
//! sandbox (logs and continues unsandboxed) rather than refusing to start —
//! matching the fail-open convention already used elsewhere in this project
//! (`pre_tool` hook, `mission::supervisor_gate`).

use std::path::PathBuf;

/// Env var that opts into whole-process sandboxing. Unset/anything other
/// than "1" means sandboxing is off (the safe default for this early slice).
const ENABLE_ENV_VAR: &str = "JCODE_FUSION_SANDBOX";

/// Marker env var set on the re-exec'd process so it doesn't try to sandbox
/// itself again (which would otherwise loop `sandbox-exec` invocations).
const SANDBOXED_MARKER_ENV_VAR: &str = "JCODE_FUSION_SANDBOXED";

pub fn is_sandboxing_requested() -> bool {
    std::env::var(ENABLE_ENV_VAR)
        .map(|v| v == "1")
        .unwrap_or(false)
}

pub fn is_already_sandboxed() -> bool {
    std::env::var(SANDBOXED_MARKER_ENV_VAR)
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Curated list of paths (relative to `$HOME`) that must never be written to
/// by a sandboxed jcode-fusion process — credentials, keys, and secrets an
/// agent should be able to *read* (many tools legitimately need to see e.g.
/// `~/.gitconfig`) but should never be able to modify or delete.
///
/// Not exhaustive by design (see module docs) — this is blast-radius
/// reduction against the highest-value targets, not a complete allow-list.
pub fn default_protected_write_subpaths() -> Vec<&'static str> {
    vec![
        ".ssh",
        ".gnupg",
        ".aws",
        ".config/gcloud",
        ".docker",
        ".kube",
        ".netrc",
        ".npmrc",
        ".pypirc",
        ".config/gh",
        ".azure",
    ]
}

/// Build a Seatbelt (`sandbox-exec`) profile string. Pure and testable —
/// takes the already-resolved absolute paths to protect rather than
/// resolving `$HOME` itself, so tests don't depend on the real home
/// directory.
/// sandbox-exec's profile language uses Scheme-style string literals;
/// escape any embedded quotes/backslashes defensively even though real
/// filesystem paths essentially never contain them, rather than assuming
/// they can't.
fn escape_seatbelt_string(raw: &str) -> String {
    raw.replace('\\', "\\\\").replace('"', "\\\"")
}

pub fn build_seatbelt_profile(protected_write_paths: &[PathBuf]) -> String {
    let mut profile = String::from("(version 1)\n(allow default)\n");
    if !protected_write_paths.is_empty() {
        profile.push_str("(deny file-write*\n");
        for path in protected_write_paths {
            let escaped = escape_seatbelt_string(&path.to_string_lossy());
            profile.push_str(&format!("  (subpath \"{escaped}\")\n"));
        }
        profile.push_str(")\n");

        // Gemini review, 2026-08-30: a `subpath` rule on e.g. `~/.config/gh`
        // only matches operations whose *target* path is under `.config/gh`
        // -- it does nothing to stop renaming the *parent* (`~/.config`)
        // out from under it, since `(allow default)` otherwise permits that
        // rename freely and the renamed path (`~/.config`) isn't itself a
        // subpath of `.config/gh`. Close that by also denying
        // rename/unlink of each protected path's own immediate parent
        // directory, as a *literal* (not `subpath`) rule -- this does not
        // block ordinary writes to sibling files inside that parent, only
        // renaming/removing the parent entry itself.
        let mut parents: Vec<String> = protected_write_paths
            .iter()
            .filter_map(|p| p.parent())
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        parents.sort();
        parents.dedup();
        for parent in &parents {
            let escaped = escape_seatbelt_string(parent);
            profile.push_str(&format!(
                "(deny file-write-rename (literal \"{escaped}\"))\n\
                 (deny file-write-unlink (literal \"{escaped}\"))\n"
            ));
        }
    }
    profile
}

/// Resolve [`default_protected_write_subpaths`] against the real home
/// directory. Returns an empty list (not an error) if the home directory
/// can't be resolved — callers should treat that as "nothing to protect
/// against right now" rather than a hard failure, consistent with this
/// module's fail-open posture.
///
/// **Canonicalizes `$HOME` before joining subpaths.** Verified by hand
/// during development: macOS's Seatbelt matches `subpath` rules against the
/// *resolved* filesystem path, not whatever string you hand it — e.g.
/// `/var/folders/...` (what `mktemp`/`TMPDIR` hand out) is actually a
/// symlink to `/private/var/folders/...`, and a `deny` rule written against
/// the symlinked form silently matches nothing at all. `$HOME` on macOS is
/// essentially never itself a symlink in practice, but canonicalizing it
/// costs nothing and removes the entire class of bug rather than relying on
/// that being true. Falls back to the raw home dir (not an empty list) if
/// canonicalization fails, so a genuinely unusual `$HOME` still gets
/// *some* protection rather than none.
pub fn resolve_default_protected_paths() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    resolve_protected_paths_from(&home)
}

/// The actual canonicalize-then-join logic, split out from
/// [`resolve_default_protected_paths`] so it's testable against a
/// controlled (including deliberately symlinked) directory instead of the
/// real, unpredictable `$HOME`.
fn resolve_protected_paths_from(home: &std::path::Path) -> Vec<PathBuf> {
    let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    let mut paths = Vec::new();
    for sub in default_protected_write_subpaths() {
        let joined = home.join(sub);
        // Gemini review, 2026-08-30 (first pass): only `$HOME` itself was
        // being canonicalized -- if a protected *leaf* (e.g. `~/.ssh`,
        // `~/.aws`) is itself a symlink, the deny rule was written against
        // the un-resolved leaf path while Seatbelt matches the resolved
        // filesystem path, leaving the real target directory completely
        // unprotected. Fixed by also canonicalizing each leaf.
        //
        // Second pass caught a real gap in that fix: protecting *only* the
        // canonical target (replacing `joined` instead of adding to it)
        // left the symlink *entry itself* unprotected -- `unlink()`
        // operates on the symlink's own path, not its resolved target, so
        // an attacker could `rm ~/.ssh` (the symlink) and `mkdir ~/.ssh`
        // fresh, writing into a brand-new, entirely unprotected directory
        // at the same literal path. Both the literal leaf path and its
        // canonical resolution are protected now, not one or the other.
        let canonical = std::fs::canonicalize(&joined).ok();
        paths.push(joined.clone());
        if let Some(canonical) = canonical
            && canonical != joined
        {
            paths.push(canonical);
        }
    }
    paths
}

/// If sandboxing is requested and this process isn't already running inside
/// one, re-exec the current binary (same args) wrapped in `sandbox-exec`.
/// On success this never returns (the process image is replaced). On any
/// failure, logs a warning and returns normally — **fails open**, the
/// caller continues running unsandboxed rather than refusing to start.
#[cfg(target_os = "macos")]
pub fn maybe_reexec_under_sandbox() {
    if is_already_sandboxed() || !is_sandboxing_requested() {
        return;
    }

    if let Err(err) = try_reexec_under_sandbox() {
        crate::logging::warn(&format!(
            "[sandbox] failed to apply whole-process sandbox, continuing unsandboxed: {err}"
        ));
    }
}

#[cfg(target_os = "macos")]
fn try_reexec_under_sandbox() -> anyhow::Result<()> {
    use std::io::Write;

    let profile = build_seatbelt_profile(&resolve_default_protected_paths());
    let profile_path = std::env::temp_dir().join(format!("jcode-fusion-sandbox-{}.sb", std::process::id()));
    {
        let mut file = std::fs::File::create(&profile_path)?;
        file.write_all(profile.as_bytes())?;
    }

    let current_exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new("sandbox-exec");
    cmd.arg("-f")
        .arg(&profile_path)
        .arg("--")
        .arg(&current_exe)
        .args(std::env::args().skip(1))
        .env(SANDBOXED_MARKER_ENV_VAR, "1");

    crate::logging::info(&format!(
        "[sandbox] re-executing under sandbox-exec with profile {}",
        profile_path.display()
    ));

    // Never returns on success (process image replaced). On failure,
    // returns the io::Error so the caller can log-and-continue.
    let err = crate::platform::replace_process(&mut cmd);
    Err(err.into())
}

#[cfg(not(target_os = "macos"))]
pub fn maybe_reexec_under_sandbox() {
    // Linux/Windows sandboxing is later Fusion work (DESIGN.md §8 Phase 1
    // note: macOS first since the dev machine is macOS, Linux validated
    // later via Docker/CI). No-op elsewhere, not a silent gap: the request
    // flag is simply not actionable yet outside macOS.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_with_no_protected_paths_is_just_allow_default() {
        let profile = build_seatbelt_profile(&[]);
        assert_eq!(profile, "(version 1)\n(allow default)\n");
    }

    #[test]
    fn profile_denies_writes_to_each_protected_path() {
        let paths = vec![
            PathBuf::from("/Users/test/.ssh"),
            PathBuf::from("/Users/test/.aws"),
        ];
        let profile = build_seatbelt_profile(&paths);
        assert!(profile.contains("(allow default)"));
        assert!(profile.contains("(deny file-write*"));
        assert!(profile.contains(r#"(subpath "/Users/test/.ssh")"#));
        assert!(profile.contains(r#"(subpath "/Users/test/.aws")"#));
    }

    #[test]
    fn profile_escapes_embedded_quotes_defensively() {
        let paths = vec![PathBuf::from(r#"/Users/te"st/.ssh"#)];
        let profile = build_seatbelt_profile(&paths);
        assert!(profile.contains(r#"\"st"#), "expected escaped quote in: {profile}");
    }

    /// Regression test for a real bug caught during manual verification:
    /// Seatbelt matches `subpath` rules against the *canonical* filesystem
    /// path, not whatever string is handed to it. A profile built from a
    /// symlinked path (e.g. `/var/...`, which is actually a symlink to
    /// `/private/var/...` on macOS) silently protects nothing at all —
    /// confirmed by hand with a real `sandbox-exec` run before this fix
    /// existed. This test proves `resolve_protected_paths_from` actually
    /// resolves the symlink rather than trusting the input path verbatim.
    #[test]
    fn resolved_paths_follow_symlinks_to_the_real_path() {
        let temp = tempfile::tempdir().expect("tempdir");
        let real_home = temp.path().join("real_home");
        std::fs::create_dir_all(&real_home).expect("mkdir real_home");
        let symlinked_home = temp.path().join("home_symlink");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_home, &symlinked_home).expect("symlink");

        let resolved = resolve_protected_paths_from(&symlinked_home);
        let expected_real_path = std::fs::canonicalize(&real_home).expect("canonicalize real");
        assert!(!resolved.is_empty());
        for path in &resolved {
            assert!(
                path.starts_with(&expected_real_path),
                "expected {} to be under the canonical real path {}, not the symlinked input \
                 {} -- this is exactly the bug that made the deny rule silently match nothing",
                path.display(),
                expected_real_path.display(),
                symlinked_home.display()
            );
        }
    }

    /// Gemini review, 2026-08-30: a symlinked *leaf* (not just a symlinked
    /// `$HOME`) used to leave the real target directory unprotected, since
    /// only `$HOME` was canonicalized before joining subpaths.
    #[test]
    fn resolved_paths_follow_symlinks_on_the_leaf_itself_not_just_home() {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).expect("mkdir home");
        let real_ssh_target = temp.path().join("real_ssh_elsewhere");
        std::fs::create_dir_all(&real_ssh_target).expect("mkdir real target");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_ssh_target, home.join(".ssh")).expect("symlink leaf");

        let resolved = resolve_protected_paths_from(&home);
        let expected_real_path =
            std::fs::canonicalize(&real_ssh_target).expect("canonicalize real target");
        assert!(
            resolved.contains(&expected_real_path),
            "expected the resolved paths ({resolved:?}) to include the real target of the \
             symlinked ~/.ssh ({expected_real_path:?}), not just the symlink's own path"
        );
        // Second-pass fix, Gemini review 2026-08-30: protecting *only* the
        // canonical target left the symlink entry itself removable
        // (`unlink` operates on the symlink's own path, not its resolved
        // target) -- an attacker could delete the symlink and recreate
        // `~/.ssh` fresh at the same literal, unprotected path. Both the
        // literal leaf path and its resolved target must be present. `home`
        // itself is canonicalized before joining (same as `$HOME` always
        // was), so the expected literal path is joined against the
        // canonical home, not the possibly-symlinked input `home`.
        let canonical_home = std::fs::canonicalize(&home).expect("canonicalize home");
        assert!(
            resolved.contains(&canonical_home.join(".ssh")),
            "expected the resolved paths ({resolved:?}) to ALSO include the literal symlink \
             path itself ({:?}) -- otherwise the symlink entry could be deleted and replaced \
             with a fresh, unprotected directory at the same path",
            canonical_home.join(".ssh")
        );
    }

    /// Gemini review, 2026-08-30: a `subpath` rule alone doesn't stop
    /// renaming the *parent* of a protected path out from under it.
    #[test]
    fn profile_also_denies_renaming_or_unlinking_each_protected_paths_parent() {
        let paths = vec![
            PathBuf::from("/Users/test/.config/gh"),
            PathBuf::from("/Users/test/.config/gcloud"),
            PathBuf::from("/Users/test/.ssh"),
        ];
        let profile = build_seatbelt_profile(&paths);
        assert!(
            profile.contains(r#"(deny file-write-rename (literal "/Users/test/.config"))"#),
            "expected a rename-deny on the shared parent .config, got: {profile}"
        );
        assert!(
            profile.contains(r#"(deny file-write-unlink (literal "/Users/test/.config"))"#),
            "expected an unlink-deny on the shared parent .config, got: {profile}"
        );
        assert!(
            profile.contains(r#"(deny file-write-rename (literal "/Users/test"))"#),
            "expected a rename-deny on .ssh's parent, got: {profile}"
        );
        // The shared parent (.config) must appear only once even though
        // two protected paths live under it.
        assert_eq!(
            profile.matches(r#"(deny file-write-rename (literal "/Users/test/.config"))"#).count(),
            1,
            "a shared parent must be deduplicated, not repeated once per child"
        );
    }

    #[test]
    fn default_protected_subpaths_cover_the_high_value_targets() {
        let subpaths = default_protected_write_subpaths();
        for expected in [".ssh", ".gnupg", ".aws", ".docker", ".kube"] {
            assert!(
                subpaths.contains(&expected),
                "expected {expected} in default protected list"
            );
        }
    }

    #[test]
    fn sandboxing_is_off_by_default() {
        let _guard = crate::storage::lock_test_env();
        unsafe {
            std::env::remove_var(ENABLE_ENV_VAR);
        }
        assert!(!is_sandboxing_requested());
    }

    #[test]
    fn sandboxing_requires_exactly_the_string_one() {
        let _guard = crate::storage::lock_test_env();
        unsafe {
            std::env::set_var(ENABLE_ENV_VAR, "true");
        }
        assert!(
            !is_sandboxing_requested(),
            "only the literal value \"1\" should enable sandboxing"
        );
        unsafe {
            std::env::set_var(ENABLE_ENV_VAR, "1");
        }
        assert!(is_sandboxing_requested());
        unsafe {
            std::env::remove_var(ENABLE_ENV_VAR);
        }
    }

    #[test]
    fn already_sandboxed_marker_is_read_correctly() {
        let _guard = crate::storage::lock_test_env();
        unsafe {
            std::env::remove_var(SANDBOXED_MARKER_ENV_VAR);
        }
        assert!(!is_already_sandboxed());
        unsafe {
            std::env::set_var(SANDBOXED_MARKER_ENV_VAR, "1");
        }
        assert!(is_already_sandboxed());
        unsafe {
            std::env::remove_var(SANDBOXED_MARKER_ENV_VAR);
        }
    }
}
