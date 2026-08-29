//! Fusion Phase 2: worktree-per-subagent swarm isolation (DESIGN.md §6
//! item #2).
//!
//! **What this replaces**: today, a spawned swarm worker that doesn't get an
//! explicit `working_dir` falls back to sharing the parent session's own
//! directory outright (`comm_session.rs::resolve_spawn_working_dir`) —
//! confirmed via source read, not assumed; zero worktree isolation exists in
//! jcode today, no `git2`/`gix` dependency, no `git worktree add` call
//! anywhere. Conflict detection is purely advisory (post-hoc file-touch
//! notifications, no locking) — this module doesn't replace that, it adds a
//! second, structural layer: give concurrent workers their own working
//! trees so most conflicts never happen in the first place, the same
//! pattern both Codex's `worktree` crate and Grok Build's subagent
//! isolation independently converged on.
//!
//! **Opt-in** (`JCODE_FUSION_SWARM_WORKTREES=1`, default off), same
//! convention as `sandbox_macos`. Fails open: any error creating a worktree
//! falls back to the pre-existing shared-directory behavior rather than
//! failing the spawn.
//!
//! **Deliberately scoped to creation only for this first slice — no
//! automatic merge-back, no automatic cleanup.** A worktree this module
//! creates persists (and its branch stays around) until something else
//! removes it via [`remove_worktree`], which nothing calls automatically
//! yet. Real follow-up work, not silently pretended solved: (a) merging a
//! worker's worktree branch back into the coordinator's tree once the
//! worker finishes (Grok Build's own pattern is an explicit "apply" step,
//! not automatic — a reasonable model to follow), and (b) cleaning up
//! abandoned worktrees (crashed workers, cancelled swarms). Both are
//! genuinely more involved than "create a worktree," and getting them wrong
//! risks losing a worker's actual work — not something to rush.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

const ENABLE_ENV_VAR: &str = "JCODE_FUSION_SWARM_WORKTREES";

pub fn is_worktree_isolation_requested() -> bool {
    std::env::var(ENABLE_ENV_VAR)
        .map(|v| v == "1")
        .unwrap_or(false)
}

fn short_hash(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())[..16].to_string()
}

/// Where worktrees for a given repo are stored:
/// `~/.jcode/worktrees/<hash-of-canonical-repo-root>/`. Bucketed by repo so
/// workers spawned from different subdirectories of the same repo still
/// share one storage location.
pub fn worktree_root_for(repo_root: &Path) -> anyhow::Result<PathBuf> {
    let canonical = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    Ok(crate::storage::jcode_dir()?
        .join("worktrees")
        .join(short_hash(&canonical.to_string_lossy())))
}

/// A short, unique label for one worker's worktree/branch, derived from the
/// swarm id plus a real timestamp (needs to be unique per spawn, not
/// reproducible — this is live process code, not a replayable script).
pub fn generate_worker_label(swarm_id: &str) -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}-{:x}", &short_hash(swarm_id)[..8], nanos as u64)
}

/// Resolve the actual repo root for a working directory that might be a
/// subdirectory of a git repo (`git worktree add` needs to run against the
/// repo, and consistent bucketing in [`worktree_root_for`] needs the real
/// root, not whatever subdirectory a worker happened to be in).
pub async fn resolve_repo_root(cwd: &Path) -> anyhow::Result<PathBuf> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(cwd)
        .arg("rev-parse")
        .arg("--show-toplevel")
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "not a git repository (or any parent): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if path.is_empty() {
        anyhow::bail!("git rev-parse --show-toplevel returned an empty path");
    }
    Ok(PathBuf::from(path))
}

/// Create a new git worktree checked out from `repo_root`'s current HEAD on
/// a fresh branch, for one swarm worker. Returns the new worktree's path.
pub async fn create_worktree(repo_root: &Path, worker_label: &str) -> anyhow::Result<PathBuf> {
    let root = worktree_root_for(repo_root)?;
    tokio::fs::create_dir_all(&root).await.ok();
    let branch = format!("jcode-swarm/{worker_label}");
    let path = root.join(worker_label);

    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("worktree")
        .arg("add")
        .arg(&path)
        .arg("-b")
        .arg(&branch)
        .arg("HEAD")
        .output()
        .await?;

    if !output.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(path)
}

/// Remove a worktree created by [`create_worktree`] (and, separately, its
/// branch — `git worktree remove` alone leaves the branch behind by design,
/// since the branch may still hold work worth keeping/merging). **Not
/// called automatically by anything yet** — see module docs.
pub async fn remove_worktree(repo_root: &Path, worktree_path: &Path) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("worktree")
        .arg("remove")
        .arg(worktree_path)
        .arg("--force")
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// The actual spawn-time entry point: resolve the repo root from a working
/// directory, then create a worktree for one worker. Combines
/// [`resolve_repo_root`] + [`generate_worker_label`] + [`create_worktree`]
/// into the one call the spawn path needs.
pub async fn create_worktree_for_spawn(
    working_dir: &Path,
    swarm_id: &str,
) -> anyhow::Result<PathBuf> {
    let repo_root = resolve_repo_root(working_dir).await?;
    let label = generate_worker_label(swarm_id);
    create_worktree(&repo_root, &label).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `git init` + a commit in a tempdir -- these tests exercise the
    /// actual `git` binary, not a mock, since worktree creation is exactly
    /// the kind of thing that's easy to get subtly wrong against a fake.
    async fn init_test_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .expect("git command")
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
        run(&["add", "."]);
        run(&["commit", "-q", "-m", "initial"]);
        dir
    }

    #[test]
    fn sandboxing_style_opt_in_is_off_by_default() {
        let _guard = crate::storage::lock_test_env();
        unsafe {
            std::env::remove_var(ENABLE_ENV_VAR);
        }
        assert!(!is_worktree_isolation_requested());
    }

    #[test]
    fn worker_labels_are_unique_across_calls() {
        let a = generate_worker_label("swarm-1");
        let b = generate_worker_label("swarm-1");
        assert_ne!(a, b, "two labels generated back-to-back must not collide");
    }

    #[test]
    fn worktree_root_buckets_by_canonical_repo_path() {
        let _guard = crate::storage::lock_test_env();
        let temp = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", temp.path());
        let repo = tempfile::tempdir().expect("tempdir");

        let root_a = worktree_root_for(repo.path()).expect("root a");
        let root_b = worktree_root_for(repo.path()).expect("root b");
        assert_eq!(root_a, root_b, "same repo path must bucket identically");
    }

    #[tokio::test]
    async fn resolve_repo_root_finds_the_toplevel_from_a_subdirectory() {
        let repo = init_test_repo().await;
        let subdir = repo.path().join("src").join("nested");
        std::fs::create_dir_all(&subdir).expect("mkdir");

        let resolved = resolve_repo_root(&subdir).await.expect("resolve");
        let expected = std::fs::canonicalize(repo.path()).expect("canonicalize");
        assert_eq!(resolved, expected);
    }

    #[tokio::test]
    async fn resolve_repo_root_fails_cleanly_outside_a_repo() {
        let not_a_repo = tempfile::tempdir().expect("tempdir");
        let result = resolve_repo_root(not_a_repo.path()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn create_worktree_produces_a_real_working_checkout() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-a")
            .await
            .expect("create worktree");

        assert!(worktree_path.exists());
        assert!(
            worktree_path.join("README.md").exists(),
            "the worktree should have a real checkout of HEAD"
        );
        let readme = std::fs::read_to_string(worktree_path.join("README.md")).expect("read");
        assert_eq!(readme, "hello\n");

        // The worktree is genuinely independent: a change there must not
        // touch the original checkout.
        std::fs::write(worktree_path.join("only_in_worktree.txt"), "isolated\n")
            .expect("write in worktree");
        assert!(!repo.path().join("only_in_worktree.txt").exists());
    }

    #[tokio::test]
    async fn two_spawns_get_two_independent_worktrees() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let a = create_worktree_for_spawn(repo.path(), "swarm-x")
            .await
            .expect("spawn a");
        let b = create_worktree_for_spawn(repo.path(), "swarm-x")
            .await
            .expect("spawn b");

        assert_ne!(a, b);
        assert!(a.exists());
        assert!(b.exists());
    }

    #[tokio::test]
    async fn create_worktree_for_spawn_fails_cleanly_outside_a_repo() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let not_a_repo = tempfile::tempdir().expect("tempdir");

        let result = create_worktree_for_spawn(not_a_repo.path(), "swarm-y").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn remove_worktree_actually_removes_it() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-cleanup")
            .await
            .expect("create");
        assert!(worktree_path.exists());

        remove_worktree(repo.path(), &worktree_path)
            .await
            .expect("remove");
        assert!(!worktree_path.exists());
    }
}
