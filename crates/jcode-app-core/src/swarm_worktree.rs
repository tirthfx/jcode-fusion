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

/// True if `path` lives under this module's own worktree storage root
/// (`~/.jcode/worktrees/`) — the way the cleanup sweep (below) tells "this
/// member's working_dir is a worktree we created" from "this is an ordinary
/// shared directory we must never touch." A simple path-prefix check rather
/// than a new field on `SwarmMember`, deliberately: adding a field would
/// touch that struct's definition and persistence format, a much larger
/// change than warranted for this.
pub fn is_managed_worktree_path(path: &Path) -> bool {
    let Ok(jcode_dir) = crate::storage::jcode_dir() else {
        return false;
    };
    let managed_root = jcode_dir.join("worktrees");
    // Both sides must resolve for real -- no falling back to an
    // uncanonicalized path on either one. `Path::starts_with` compares
    // components lexically, not the resolved filesystem path; canonicalize's
    // usual "just use the raw path" fallback on a missing/unreadable path
    // would let a crafted, nonexistent `.../worktrees/x/../../secrets`-style
    // string pass a component-wise prefix check without its `..` segments
    // ever actually being resolved. A worktree this module created always
    // exists on disk, so requiring both sides to canonicalize costs nothing
    // real and closes that edge case.
    let Ok(canonical_managed_root) = std::fs::canonicalize(&managed_root) else {
        return false;
    };
    let Ok(canonical_path) = std::fs::canonicalize(path) else {
        return false;
    };
    canonical_path.starts_with(&canonical_managed_root)
}

/// The outcome of attempting to merge a worker's worktree branch back into
/// the coordinator's tree. Deliberately only two variants: a merge either
/// lands cleanly or it doesn't. A conflicted attempt is always aborted
/// before this returns (see [`merge_worktree_branch`]) -- there is no
/// "merged with conflicts left for you to resolve" state, since that would
/// mean handing back a repo in a half-merged condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeOutcome {
    /// The branch merged cleanly. `commit_sha` is the new merge commit.
    Merged { commit_sha: String },
    /// The merge produced conflicts and was aborted -- the repo is back to
    /// exactly the state it was in before the merge was attempted.
    /// `files` lists the conflicting paths, for the caller to act on
    /// (e.g. ask the worker to resolve and recommit, or route to a human).
    Conflict { files: Vec<String> },
}

/// Derive the `jcode-swarm/<label>` branch name [`create_worktree`] created
/// for a given worktree, from the worktree's own directory name -- mirrors
/// `path = root.join(worker_label)` in [`create_worktree`] exactly, so this
/// is a pure inverse of that, not a separate convention to keep in sync by
/// hand.
pub fn branch_name_for_worktree(worktree_path: &Path) -> Option<String> {
    let label = worktree_path.file_name()?.to_str()?;
    Some(format!("jcode-swarm/{label}"))
}

/// True if the worktree has no uncommitted changes (staged, unstaged, or
/// untracked). Merging a dirty worktree would silently leave that work
/// behind -- git only ever merges what's committed -- so callers must
/// check this *before* attempting a merge, not discover it after the fact.
pub async fn worktree_is_clean(worktree_path: &Path) -> anyhow::Result<bool> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("status")
        .arg("--porcelain")
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "git status failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout.is_empty())
}

/// List the conflicting paths from a merge currently in progress (`git
/// status --porcelain` marks unmerged entries with a `U` in either column,
/// plus the `AA`/`DD` both-added/both-deleted cases). Used only to build a
/// [`MergeOutcome::Conflict`] report before the merge is aborted.
async fn list_conflicted_paths(repo_root: &Path) -> anyhow::Result<Vec<String>> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("status")
        .arg("--porcelain")
        .output()
        .await?;
    let text = String::from_utf8_lossy(&output.stdout);
    let mut files: Vec<String> = text
        .lines()
        .filter(|line| {
            let status = line.get(0..2).unwrap_or("");
            matches!(status, "UU" | "AA" | "DD" | "AU" | "UA" | "DU" | "UD")
        })
        .filter_map(|line| {
            // `git status --porcelain` wraps a path containing spaces or
            // other special characters in double quotes -- strip them so a
            // conflict report doesn't show a filename with literal quote
            // characters baked in. Cosmetic only: this list is for a human
            // (or an agent) reading the tool's response text, not something
            // re-parsed by git.
            line.get(3..).map(|s| s.trim_matches('"').to_string())
        })
        .collect();
    files.sort();
    files.dedup();
    Ok(files)
}

/// Merge a worker's worktree branch into `repo_root`'s currently checked
/// out branch. **Refuses (does not attempt) if the worktree has
/// uncommitted changes** -- see [`worktree_is_clean`]; that check must run
/// before this is called, and this function re-checks it itself rather
/// than trusting the caller, since silently merging past a dirty worktree
/// is exactly the kind of mistake this module exists to prevent.
///
/// Always merges with `--no-ff`: a merge commit is created even when a
/// fast-forward would be possible, so the resulting history always
/// explicitly records that a swarm worker's branch was applied here,
/// rather than looking indistinguishable from the coordinator's own
/// commits.
///
/// On conflict, the merge is unconditionally aborted (`git merge --abort`)
/// before returning -- the coordinator's tree is never left mid-merge.
/// Verified by this module's own tests, not just assumed: a conflicting
/// merge attempt is followed by a real `git status` check confirming no
/// `MERGE_HEAD` / unmerged state remains.
pub async fn merge_worktree_branch(
    repo_root: &Path,
    worktree_path: &Path,
) -> anyhow::Result<MergeOutcome> {
    if !worktree_is_clean(worktree_path).await? {
        anyhow::bail!(
            "worktree at {} has uncommitted changes -- commit or discard them before merging",
            worktree_path.display()
        );
    }

    let branch = branch_name_for_worktree(worktree_path).ok_or_else(|| {
        anyhow::anyhow!(
            "could not derive a branch name from worktree path {}",
            worktree_path.display()
        )
    })?;

    let verify = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("rev-parse")
        .arg("--verify")
        .arg(format!("refs/heads/{branch}"))
        .output()
        .await?;
    if !verify.status.success() {
        anyhow::bail!("branch '{branch}' does not exist in {}", repo_root.display());
    }

    let merge = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("merge")
        .arg("--no-ff")
        .arg("-m")
        .arg(format!("Merge swarm worker branch '{branch}' (jcode-fusion merge-back)"))
        .arg(&branch)
        .output()
        .await?;

    if merge.status.success() {
        let sha_output = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .arg("rev-parse")
            .arg("HEAD")
            .output()
            .await?;
        let commit_sha = String::from_utf8_lossy(&sha_output.stdout).trim().to_string();
        return Ok(MergeOutcome::Merged { commit_sha });
    }

    // Conflict (or some other merge failure). First check whether git
    // actually entered a merge state at all: a pre-flight refusal (e.g. the
    // coordinator's own tree has uncommitted changes that would be
    // overwritten) never creates MERGE_HEAD, and in that case `git merge
    // --abort` would itself fail with "There is no merge to abort" -- a
    // false alarm, not a real "repo left mid-merge" problem. Only run (and
    // require) a real abort when a merge was genuinely in progress.
    let merge_head = repo_root.join(".git").join("MERGE_HEAD");
    let merge_was_in_progress = tokio::fs::try_exists(&merge_head).await.unwrap_or(false);
    let files = list_conflicted_paths(repo_root).await.unwrap_or_default();

    if merge_was_in_progress {
        let abort = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo_root)
            .arg("merge")
            .arg("--abort")
            .output()
            .await;
        match abort {
            Err(e) => {
                anyhow::bail!(
                    "merge of '{branch}' failed and `git merge --abort` itself could not run \
                     ({e}) -- repo at {} may be left mid-merge, needs manual attention",
                    repo_root.display()
                );
            }
            // A spawn that succeeds but returns a failing exit code (e.g. an
            // index lock held by another process) is just as dangerous as a
            // spawn error here -- either way the repo may still be mid-merge,
            // and silently returning `Conflict` as if it had been cleanly
            // reverted would be a false "safe" report.
            Ok(output) if !output.status.success() => {
                anyhow::bail!(
                    "merge of '{branch}' failed and `git merge --abort` itself failed ({}) -- \
                     repo at {} is likely still mid-merge, needs manual attention",
                    String::from_utf8_lossy(&output.stderr).trim(),
                    repo_root.display()
                );
            }
            Ok(_) => {}
        }
    }

    if files.is_empty() {
        // Merge failed for a reason that wasn't a content conflict (e.g. the
        // coordinator's own tree had uncommitted changes in the way, or a
        // local pre-merge hook rejected it). Surface the real stderr rather
        // than reporting a misleading empty conflict list.
        anyhow::bail!(
            "git merge failed{}: {}",
            if merge_was_in_progress { " and was aborted" } else { "" },
            String::from_utf8_lossy(&merge.stderr).trim()
        );
    }

    Ok(MergeOutcome::Conflict { files })
}

/// Cleanup entry point: remove a worktree using only its own path, no
/// separately-tracked repo root needed. **Verified this actually works**
/// (not assumed): `git worktree remove` operates on repo-wide state shared
/// via the main `.git` directory, so invoking it with `-C <worktree>`
/// pointed at the worktree itself is sufficient — confirmed by hand with a
/// real `git worktree add` + `git -C <worktree> worktree remove <worktree>`
/// before writing this. `SwarmMember` only stores `working_dir` (the
/// worktree path), not the original repo root, so this self-contained form
/// is what the cleanup sweep actually needs.
pub async fn remove_worktree_self_contained(worktree_path: &Path) -> anyhow::Result<()> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(worktree_path)
        .arg("worktree")
        .arg("remove")
        .arg(worktree_path)
        .arg("--force")
        .output()
        .await?;
    if !output.status.success() {
        anyhow::bail!(
            "git worktree remove (self-contained) failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
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
    async fn is_managed_worktree_path_recognizes_our_own_worktrees() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-managed-check")
            .await
            .expect("create");
        assert!(is_managed_worktree_path(&worktree_path));
    }

    #[tokio::test]
    async fn is_managed_worktree_path_rejects_ordinary_directories() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let ordinary_dir = tempfile::tempdir().expect("tempdir");

        assert!(
            !is_managed_worktree_path(ordinary_dir.path()),
            "an arbitrary directory (e.g. a real shared working_dir) must never be \
             mistaken for a worktree this module created"
        );
    }

    #[tokio::test]
    async fn remove_worktree_self_contained_needs_only_the_worktree_path() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-self-remove")
            .await
            .expect("create");
        assert!(worktree_path.exists());

        // No repo_root passed anywhere -- this is the whole point of the
        // "self-contained" variant, matching what the cleanup sweep will
        // actually have available (SwarmMember only stores working_dir).
        remove_worktree_self_contained(&worktree_path)
            .await
            .expect("self-contained remove");
        assert!(!worktree_path.exists());
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

    /// Run a git command in `dir`, panicking with stderr on failure --
    /// shared by the merge-back tests below, which (unlike the tests
    /// above) need to run *more* than one git command per test to set up
    /// real divergent history.
    fn git_ok(dir: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[tokio::test]
    async fn branch_name_for_worktree_matches_what_create_worktree_actually_made() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-branch-name")
            .await
            .expect("create");
        let branch = branch_name_for_worktree(&worktree_path).expect("derive branch name");
        assert_eq!(branch, "jcode-swarm/worker-branch-name");

        // Not just string-shaped -- the branch this names must actually exist.
        let verify = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .arg("rev-parse")
            .arg("--verify")
            .arg(format!("refs/heads/{branch}"))
            .output()
            .expect("git rev-parse");
        assert!(verify.status.success());
    }

    #[tokio::test]
    async fn worktree_is_clean_true_for_an_untouched_worktree() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-clean")
            .await
            .expect("create");
        assert!(worktree_is_clean(&worktree_path).await.expect("check"));
    }

    #[tokio::test]
    async fn worktree_is_clean_false_after_an_uncommitted_edit() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-dirty")
            .await
            .expect("create");
        std::fs::write(worktree_path.join("scratch.txt"), "not committed\n").expect("write");
        assert!(!worktree_is_clean(&worktree_path).await.expect("check"));
    }

    #[tokio::test]
    async fn merge_worktree_branch_refuses_when_the_worktree_is_dirty() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-dirty-merge")
            .await
            .expect("create");
        std::fs::write(worktree_path.join("scratch.txt"), "not committed\n").expect("write");

        let result = merge_worktree_branch(repo.path(), &worktree_path).await;
        assert!(result.is_err(), "must refuse to merge a dirty worktree");

        // And must not have touched the coordinator's tree at all.
        assert!(!repo.path().join("scratch.txt").exists());
    }

    #[tokio::test]
    async fn merge_worktree_branch_merges_a_clean_commit() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-good-merge")
            .await
            .expect("create");
        std::fs::write(worktree_path.join("feature.txt"), "worker's work\n").expect("write");
        git_ok(&worktree_path, &["add", "."]);
        git_ok(&worktree_path, &["commit", "-q", "-m", "worker: add feature.txt"]);

        let outcome = merge_worktree_branch(repo.path(), &worktree_path)
            .await
            .expect("merge should succeed");
        match outcome {
            MergeOutcome::Merged { commit_sha } => assert!(!commit_sha.is_empty()),
            other => panic!("expected Merged, got {other:?}"),
        }

        // The coordinator's own working tree must now actually have it.
        assert!(repo.path().join("feature.txt").exists());
        let contents = std::fs::read_to_string(repo.path().join("feature.txt")).expect("read");
        assert_eq!(contents, "worker's work\n");
    }

    #[tokio::test]
    async fn merge_worktree_branch_conflict_leaves_the_repo_clean() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-conflict")
            .await
            .expect("create");

        // Diverge: the coordinator's own tree changes the same line...
        std::fs::write(repo.path().join("README.md"), "coordinator's version\n")
            .expect("write coordinator side");
        git_ok(repo.path(), &["add", "."]);
        git_ok(repo.path(), &["commit", "-q", "-m", "coordinator: edit README"]);

        // ...and so does the worker, on its own branch.
        std::fs::write(worktree_path.join("README.md"), "worker's version\n")
            .expect("write worker side");
        git_ok(&worktree_path, &["add", "."]);
        git_ok(&worktree_path, &["commit", "-q", "-m", "worker: edit README"]);

        let outcome = merge_worktree_branch(repo.path(), &worktree_path)
            .await
            .expect("merge call itself should not error on a conflict");
        let files = match outcome {
            MergeOutcome::Conflict { files } => files,
            other => panic!("expected Conflict, got {other:?}"),
        };
        assert_eq!(files, vec!["README.md".to_string()]);

        // The repo must be left exactly as if no merge had been attempted:
        // no leftover MERGE_HEAD, no dirty/unmerged state, and the
        // coordinator's own pre-merge content is still there untouched.
        assert!(!repo.path().join(".git").join("MERGE_HEAD").exists());
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(repo.path())
            .arg("status")
            .arg("--porcelain")
            .output()
            .expect("git status");
        assert!(
            status.stdout.is_empty(),
            "repo must be clean after an aborted merge, got: {}",
            String::from_utf8_lossy(&status.stdout)
        );
        let readme = std::fs::read_to_string(repo.path().join("README.md")).expect("read");
        assert_eq!(readme, "coordinator's version\n");
    }

    /// Regression test for a real bug an external review caught: when the
    /// *coordinator's own tree* (not the worktree) has an uncommitted change
    /// git would need to overwrite, `git merge` refuses before it even
    /// starts -- no `MERGE_HEAD` is ever created. The old code unconditionally
    /// ran `git merge --abort` in this situation too, which would itself
    /// fail ("There is no merge to abort") in a way that was easy to
    /// conflate with "abort of a real conflict failed." This confirms the
    /// call bails with the real git error, without ever pretending a merge
    /// was attempted-and-aborted.
    #[tokio::test]
    async fn merge_worktree_branch_reports_a_dirty_coordinator_tree_cleanly() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        let worktree_path = create_worktree(repo.path(), "worker-vs-dirty-coordinator")
            .await
            .expect("create");
        std::fs::write(worktree_path.join("README.md"), "worker's version\n")
            .expect("write worker side");
        git_ok(&worktree_path, &["add", "."]);
        git_ok(&worktree_path, &["commit", "-q", "-m", "worker: edit README"]);

        // The coordinator's own tree has an *uncommitted* change to the same
        // file -- deliberately not committed, so this is a pre-flight
        // refusal, not a content conflict.
        std::fs::write(repo.path().join("README.md"), "coordinator's uncommitted edit\n")
            .expect("write coordinator side, uncommitted");

        let result = merge_worktree_branch(repo.path(), &worktree_path).await;
        assert!(
            result.is_err(),
            "must not report Merged or Conflict for a pre-flight refusal"
        );
        let message = result.unwrap_err().to_string();
        assert!(
            !message.contains("and was aborted"),
            "no merge was ever in progress, so the message must not claim one was aborted: {message}"
        );

        // No trace of a merge attempt should be left behind.
        assert!(!repo.path().join(".git").join("MERGE_HEAD").exists());
        let uncommitted = std::fs::read_to_string(repo.path().join("README.md")).expect("read");
        assert_eq!(uncommitted, "coordinator's uncommitted edit\n");
    }

    #[tokio::test]
    async fn merge_worktree_branch_fails_cleanly_when_the_branch_does_not_exist() {
        let _guard = crate::storage::lock_test_env();
        let jcode_home = tempfile::tempdir().expect("tempdir");
        crate::env::set_var("JCODE_HOME", jcode_home.path());
        let repo = init_test_repo().await;

        // A directory that merely *looks* like a worktree path (right
        // basename shape) but was never actually created via
        // create_worktree -- its derived branch genuinely doesn't exist.
        let fake_worktree = repo.path().join("not-a-real-worktree");
        std::fs::create_dir_all(&fake_worktree).expect("mkdir");

        let result = merge_worktree_branch(repo.path(), &fake_worktree).await;
        assert!(result.is_err());
    }
}
