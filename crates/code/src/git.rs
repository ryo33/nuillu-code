use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc as tokio_mpsc;

use crate::Workspace;

const STALE_WORKTREE_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const AGENT_NAME: &str = "Nuillu Code";
const AGENT_EMAIL: &str = "agent@nuillu.invalid";
const MAX_CONFLICT_FILE_BYTES: u64 = 16 * 1024 * 1024;
static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutableIdentity {
    canonical_path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl ExecutableIdentity {
    fn read(path: &Path) -> Result<Self> {
        let canonical_path = fs::canonicalize(path)
            .with_context(|| format!("resolve Git executable {}", path.display()))?;
        let metadata = fs::metadata(&canonical_path)
            .with_context(|| format!("inspect Git executable {}", canonical_path.display()))?;
        if !metadata.is_file() {
            bail!(
                "Git executable is not a regular file: {}",
                canonical_path.display()
            );
        }
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Ok(Self {
            canonical_path,
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            device: metadata.dev(),
            #[cfg(unix)]
            inode: metadata.ino(),
        })
    }
}

#[derive(Clone, Debug)]
struct GitExecutable {
    identity: ExecutableIdentity,
}

impl GitExecutable {
    fn discover() -> Result<Self> {
        let path = find_in_path("git").context("find git in PATH")?;
        Ok(Self {
            identity: ExecutableIdentity::read(&path)?,
        })
    }

    fn reject_inside(&self, root: &Path) -> Result<()> {
        let root = fs::canonicalize(root)
            .with_context(|| format!("resolve repository boundary {}", root.display()))?;
        if self.identity.canonical_path.starts_with(&root) {
            bail!("Git executable must be outside the repository");
        }
        Ok(())
    }

    fn revalidate(&self) -> Result<()> {
        if ExecutableIdentity::read(&self.identity.canonical_path)? != self.identity {
            bail!("Git executable changed after startup");
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum WorkspaceMode {
    #[default]
    ReadOnly,
    Review,
    Write,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewCommit {
    pub id: String,
    pub purpose: String,
    pub diff: String,
    pub changed_paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitUiState {
    pub mode: WorkspaceMode,
    pub branch: String,
    pub commits: Vec<ReviewCommit>,
}

#[derive(Clone, Debug)]
pub enum GitUiEvent {
    State(GitUiState),
    Error(String),
    Sensory(String),
}

#[derive(Clone, Debug)]
pub enum GitControlCommand {
    SetMode(WorkspaceMode),
    Apply(String),
    Discard(String),
    ApplyAll,
}

#[derive(Clone)]
pub struct GitControlHandle {
    sender: tokio_mpsc::UnboundedSender<GitControlCommand>,
}

impl GitControlHandle {
    pub fn send(&self, command: GitControlCommand) -> Result<()> {
        self.sender
            .send(command)
            .map_err(|_| anyhow::anyhow!("code module control channel disconnected"))
    }
}

pub struct GitWorkspaceOpen {
    pub git: GitWorkspace,
    pub workspace: Workspace,
    pub controls: GitControlHandle,
    pub control_receiver: tokio_mpsc::UnboundedReceiver<GitControlCommand>,
    pub ui_events: mpsc::Receiver<GitUiEvent>,
    pub state_dir: PathBuf,
}

#[derive(Clone)]
pub struct GitWorkspace {
    inner: Arc<Mutex<GitState>>,
    events: mpsc::Sender<GitUiEvent>,
}

struct GitState {
    git: GitExecutable,
    repo_root: PathBuf,
    worktree: PathBuf,
    state_dir: PathBuf,
    branch: String,
    session_branch: String,
    lock_path: PathBuf,
    baseline: String,
    observed: String,
    mode: WorkspaceMode,
    commits: Vec<ReviewCommit>,
    aliases: BTreeMap<String, String>,
    lifetime_lock: File,
    cleaned: bool,
}

impl Drop for GitState {
    fn drop(&mut self) {
        if self.cleaned {
            return;
        }
        let _ = self.lifetime_lock.unlock();
        let worktree = self.worktree.to_str().unwrap_or_default();
        let _ = run_git(
            &self.git,
            &self.repo_root,
            ["worktree", "remove", "--force", worktree],
        );
        let _ = run_git(
            &self.git,
            &self.repo_root,
            ["branch", "-D", &self.session_branch],
        );
        let _ = fs::remove_file(&self.lock_path);
    }
}

struct RepoLock(File);

struct CreationGuard {
    git: GitExecutable,
    repo_root: PathBuf,
    worktree: PathBuf,
    branch: String,
    lock_path: PathBuf,
    active: bool,
}

impl Drop for CreationGuard {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        let worktree = self.worktree.to_str().unwrap_or_default();
        let _ = run_git(
            &self.git,
            &self.repo_root,
            ["worktree", "remove", "--force", worktree],
        );
        let _ = run_git(&self.git, &self.repo_root, ["branch", "-D", &self.branch]);
        let _ = fs::remove_file(&self.lock_path);
    }
}

impl Drop for RepoLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatchDisposition {
    Review { review_commit: String },
    WriteApplied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConflictFile {
    pub path: String,
    pub content: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConflictCase {
    pub purpose: String,
    pub files: Vec<ConflictFile>,
}

#[async_trait(?Send)]
pub(crate) trait ConflictResolver {
    async fn resolve(&mut self, conflict: &ConflictCase) -> Result<Vec<ConflictFile>>;
}

impl GitWorkspace {
    pub fn open(cwd: &Path) -> Result<GitWorkspaceOpen> {
        let git = GitExecutable::discover()?;
        let cwd =
            fs::canonicalize(cwd).with_context(|| format!("resolve cwd {}", cwd.display()))?;
        let marker_root = cwd
            .ancestors()
            .find(|ancestor| ancestor.join(".git").exists())
            .context("cwd must be inside a non-bare Git worktree")?;
        git.reject_inside(marker_root)?;
        let root_text = command_text(&git, &cwd, ["rev-parse", "--show-toplevel"])?;
        let repo_root = fs::canonicalize(root_text.trim()).context("resolve repository root")?;
        git.reject_inside(&repo_root)?;
        if command_text(&git, &repo_root, ["rev-parse", "--is-bare-repository"])?.trim() != "false"
        {
            bail!("bare Git repositories are not supported");
        }
        let superproject = command_text(
            &git,
            &repo_root,
            ["rev-parse", "--show-superproject-working-tree"],
        )?;
        if !superproject.trim().is_empty() {
            bail!("starting inside a Git submodule is not supported");
        }
        let branch = command_text(
            &git,
            &repo_root,
            ["symbolic-ref", "--quiet", "--short", "HEAD"],
        )
        .context("cwd must have a checked-out branch")?
        .trim()
        .to_owned();
        validate_repository_state(&git, &repo_root)?;
        reject_external_filters(&git, &repo_root)?;
        validate_state_ignore(&repo_root)?;

        let state_dir = repo_root.join(".nuillu");
        fs::create_dir_all(state_dir.join("worktrees"))
            .context("create worktree state directory")?;
        fs::create_dir_all(state_dir.join("worktree-locks"))
            .context("create worktree lock directory")?;
        cleanup_stale_worktrees(&git, &repo_root, &state_dir)?;

        let id = unique_id();
        let worktree = state_dir.join("worktrees").join(&id);
        let session_branch = format!("nuillu/session/{id}");
        let lock_path = state_dir.join("worktree-locks").join(format!("{id}.lock"));
        let lifetime_lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .with_context(|| format!("open {}", lock_path.display()))?;
        lifetime_lock.lock().context("lock session worktree")?;
        let worktree_text = worktree.to_str().context("worktree path is not UTF-8")?;
        run_git(
            &git,
            &repo_root,
            [
                "worktree",
                "add",
                "--no-checkout",
                "-b",
                &session_branch,
                worktree_text,
                "HEAD",
            ],
        )?;
        let mut creation = CreationGuard {
            git: git.clone(),
            repo_root: repo_root.clone(),
            worktree: worktree.clone(),
            branch: session_branch.clone(),
            lock_path: lock_path.clone(),
            active: true,
        };

        let user_head = command_text(&git, &repo_root, ["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let baseline = snapshot_parent(&git, &repo_root, &state_dir, &user_head)?;
        run_git(&git, &worktree, ["reset", "--hard", &baseline])?;
        let workspace = Workspace::open(&worktree)?;

        let (event_sender, ui_events) = mpsc::channel();
        let state = GitState {
            git,
            repo_root,
            worktree,
            state_dir: state_dir.clone(),
            branch,
            session_branch,
            lock_path,
            baseline: baseline.clone(),
            observed: baseline.clone(),
            mode: WorkspaceMode::ReadOnly,
            commits: Vec::new(),
            aliases: BTreeMap::new(),
            lifetime_lock,
            cleaned: false,
        };
        let git = Self {
            inner: Arc::new(Mutex::new(state)),
            events: event_sender,
        };
        git.emit_state();
        let (control_sender, control_receiver) = tokio_mpsc::unbounded_channel();
        creation.active = false;
        Ok(GitWorkspaceOpen {
            git,
            workspace,
            controls: GitControlHandle {
                sender: control_sender,
            },
            control_receiver,
            ui_events,
            state_dir,
        })
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, GitState>> {
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("Git workspace lock poisoned"))
    }

    pub fn mode(&self) -> Result<WorkspaceMode> {
        Ok(self.lock()?.mode)
    }

    pub fn discard_uncommitted(&self) -> Result<()> {
        let state = self.lock()?;
        run_git(&state.git, &state.worktree, ["reset", "--hard", "HEAD"])
    }

    pub fn sync_from_parent(&self) -> Result<()> {
        let mut state = self.lock()?;
        let _repo_lock = state.repo_lock()?;
        state.validate_branch()?;
        validate_repository_state(&state.git, &state.repo_root)?;
        let head = command_text(&state.git, &state.repo_root, ["rev-parse", "HEAD"])?;
        let next = snapshot_parent(&state.git, &state.repo_root, &state.state_dir, head.trim())?;
        if same_tree(&state.git, &state.repo_root, &next, &state.observed)? {
            return Ok(());
        }
        let external_diff = git_diff(&state.git, &state.repo_root, &state.observed, &next)?;
        state.replay_onto(&next, &self.events)?;
        let previous = std::mem::replace(&mut state.observed, next.clone());
        drop(state);
        self.emit_sensory(&previous, &next, &external_diff);
        self.emit_state();
        Ok(())
    }

    pub(crate) async fn sync_with_resolver<R: ConflictResolver>(
        &self,
        resolver: &mut R,
    ) -> Result<bool> {
        let (old_commits, previous, next, external_diff) = {
            let mut state = self.lock()?;
            let _repo_lock = state.repo_lock()?;
            state.validate_branch()?;
            validate_repository_state(&state.git, &state.repo_root)?;
            let head = command_text(&state.git, &state.repo_root, ["rev-parse", "HEAD"])?;
            let next =
                snapshot_parent(&state.git, &state.repo_root, &state.state_dir, head.trim())?;
            if same_tree(&state.git, &state.repo_root, &next, &state.observed)? {
                return Ok(false);
            }
            let previous = state.observed.clone();
            let external_diff = git_diff(&state.git, &state.repo_root, &previous, &next)?;
            let old_commits = std::mem::take(&mut state.commits);
            run_git(&state.git, &state.worktree, ["reset", "--hard", &next])?;
            state.baseline = next.clone();
            (old_commits, previous, next, external_diff)
        };

        let mut review_changed = false;
        for item in old_commits {
            review_changed |= self.replay_item_with_resolver(item, resolver).await?;
        }
        {
            let mut state = self.lock()?;
            state.observed = next.clone();
            if state.commits.is_empty() {
                state.aliases.clear();
            }
        }
        self.emit_sensory(&previous, &next, &external_diff);
        self.emit_state();
        Ok(review_changed)
    }

    pub(crate) async fn handle_control_with_resolver<R: ConflictResolver>(
        &self,
        command: GitControlCommand,
        resolver: &mut R,
    ) -> Result<()> {
        let review_changed = self.sync_with_resolver(resolver).await?;
        if review_changed
            && matches!(
                command,
                GitControlCommand::ApplyAll | GitControlCommand::SetMode(WorkspaceMode::Write)
            )
        {
            let message =
                "review commits changed during synchronization; review the updated diff before applying"
                    .to_owned();
            let _ = self.events.send(GitUiEvent::Error(message));
            self.emit_state();
            return Ok(());
        }
        let mut state = self.lock()?;
        let _repo_lock = state.repo_lock()?;
        let result = match command {
            GitControlCommand::SetMode(mode) => state.set_mode(mode, &self.events),
            GitControlCommand::Apply(id) => state.apply_one(&id, &self.events),
            GitControlCommand::Discard(id) => state.discard_one(&id, &self.events),
            GitControlCommand::ApplyAll => state.apply_all(&self.events),
        };
        if let Err(error) = &result {
            let _ = self.events.send(GitUiEvent::Error(format!("{error:#}")));
        }
        drop(state);
        self.emit_state();
        result
    }

    async fn replay_item_with_resolver<R: ConflictResolver>(
        &self,
        mut item: ReviewCommit,
        resolver: &mut R,
    ) -> Result<bool> {
        let conflict = {
            let mut state = self.lock()?;
            let _repo_lock = state.repo_lock()?;
            match cherry_pick_as_agent(&state.git, &state.worktree, &item.id) {
                Ok(()) => {
                    let old_id = item.id.clone();
                    item.id = command_text(&state.git, &state.worktree, ["rev-parse", "HEAD"])?
                        .trim()
                        .to_owned();
                    item.diff = git_show(&state.git, &state.worktree, &item.id)?;
                    state.aliases.insert(old_id, item.id.clone());
                    state.commits.push(item);
                    return Ok(false);
                }
                Err(_) => {
                    let conflict = capture_conflict(&state.git, &state.worktree, &item.purpose);
                    let _ = run_git(&state.git, &state.worktree, ["cherry-pick", "--abort"]);
                    conflict
                }
            }
        };

        let conflict = match conflict {
            Ok(conflict) => conflict,
            Err(error) => {
                self.discard_conflicting_item(&item, &error);
                return Ok(true);
            }
        };
        let resolved_files = match resolver.resolve(&conflict).await {
            Ok(files) => files,
            Err(error) => {
                self.discard_conflicting_item(&item, &error);
                return Ok(true);
            }
        };
        let result = {
            let mut state = self.lock()?;
            let _repo_lock = state.repo_lock()?;
            apply_conflict_resolution(&state.git, &state.worktree, &conflict, &resolved_files)
                .and_then(|()| git_commit(&state.git, &state.worktree, &item.purpose))
                .and_then(|()| {
                    item.id = command_text(&state.git, &state.worktree, ["rev-parse", "HEAD"])?
                        .trim()
                        .to_owned();
                    item.diff = git_show(&state.git, &state.worktree, &item.id)?;
                    state.commits.push(item.clone());
                    Ok(())
                })
        };
        if let Err(error) = result {
            let state = self.lock()?;
            let tip = state
                .commits
                .last()
                .map(|commit| commit.id.as_str())
                .unwrap_or(&state.baseline);
            let _ = run_git(&state.git, &state.worktree, ["reset", "--hard", tip]);
            drop(state);
            self.discard_conflicting_item(&item, &error);
        }
        Ok(true)
    }

    fn discard_conflicting_item(&self, item: &ReviewCommit, error: &anyhow::Error) {
        let message = format!(
            "Discarded conflicting review commit {} ({}) because conflict resolution failed: {error:#}\n\n{}",
            item.id, item.purpose, item.diff
        );
        let _ = self.events.send(GitUiEvent::Sensory(message));
    }

    pub fn finish_patch(
        &self,
        purpose: &str,
        rendered_diff: &str,
        changed_paths: Vec<String>,
    ) -> Result<PatchDisposition> {
        let mut state = self.lock()?;
        let _repo_lock = state.repo_lock()?;
        state.validate_branch()?;
        if state.mode == WorkspaceMode::ReadOnly {
            run_git(&state.git, &state.worktree, ["reset", "--hard", "HEAD"])?;
            bail!("workspace is read-only; select Review or Write before patching");
        }
        run_git(&state.git, &state.worktree, ["add", "-A", "--", "."])?;
        git_commit(&state.git, &state.worktree, purpose)?;
        let commit = command_text(&state.git, &state.worktree, ["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let diff = git_show(&state.git, &state.worktree, &commit)
            .unwrap_or_else(|_| rendered_diff.to_owned());
        if state.mode == WorkspaceMode::Write {
            if let Err(error) = state.apply_patch_to_parent(&diff) {
                run_git(
                    &state.git,
                    &state.worktree,
                    ["reset", "--hard", &state.baseline],
                )?;
                return Err(error)
                    .context("automatic Write apply failed; temporary commit discarded");
            }
            let finalize = (|| -> Result<String> {
                let head = command_text(&state.git, &state.repo_root, ["rev-parse", "HEAD"])?;
                let snapshot =
                    snapshot_parent(&state.git, &state.repo_root, &state.state_dir, head.trim())?;
                run_git(&state.git, &state.worktree, ["reset", "--hard", &snapshot])?;
                Ok(snapshot)
            })();
            let snapshot = match finalize {
                Ok(snapshot) => snapshot,
                Err(error) => {
                    state
                        .reverse_patch_on_parent(&diff)
                        .context("Write finalization failed and parent rollback also failed")?;
                    run_git(
                        &state.git,
                        &state.worktree,
                        ["reset", "--hard", &state.baseline],
                    )?;
                    return Err(error).context("finalize automatic Write apply");
                }
            };
            state.baseline = snapshot.clone();
            state.observed = snapshot;
            drop(state);
            self.emit_state();
            return Ok(PatchDisposition::WriteApplied);
        }

        state.commits.push(ReviewCommit {
            id: commit.clone(),
            purpose: purpose.to_owned(),
            diff,
            changed_paths,
        });
        state.collapse_dependent_commits()?;
        let review_commit = state
            .commits
            .last()
            .map(|item| item.id.clone())
            .unwrap_or(commit);
        drop(state);
        self.emit_state();
        Ok(PatchDisposition::Review { review_commit })
    }

    pub fn handle_control(&self, command: GitControlCommand) -> Result<()> {
        self.sync_from_parent()?;
        let mut state = self.lock()?;
        let _repo_lock = state.repo_lock()?;
        let result = match command {
            GitControlCommand::SetMode(mode) => state.set_mode(mode, &self.events),
            GitControlCommand::Apply(id) => state.apply_one(&id, &self.events),
            GitControlCommand::Discard(id) => state.discard_one(&id, &self.events),
            GitControlCommand::ApplyAll => state.apply_all(&self.events),
        };
        if let Err(error) = &result {
            let _ = self.events.send(GitUiEvent::Error(format!("{error:#}")));
        }
        drop(state);
        self.emit_state();
        result
    }

    pub fn cleanup(&self) -> Result<()> {
        let mut state = self.lock()?;
        if state.cleaned {
            return Ok(());
        }
        state.cleaned = true;
        let worktree = state.worktree.clone();
        let branch = state.session_branch.clone();
        let lock_path = state.lock_path.clone();
        let git = state.git.clone();
        let root = state.repo_root.clone();
        let _ = state.lifetime_lock.unlock();
        drop(state);
        let worktree_text = worktree.to_str().unwrap_or_default();
        let _ = run_git(
            &git,
            &root,
            ["worktree", "remove", "--force", worktree_text],
        );
        let _ = run_git(&git, &root, ["branch", "-D", &branch]);
        let _ = fs::remove_file(lock_path);
        Ok(())
    }

    fn emit_state(&self) {
        if let Ok(state) = self.lock() {
            let _ = self.events.send(GitUiEvent::State(state.ui_state()));
        }
    }

    fn emit_sensory(&self, old: &str, new: &str, diff: &str) {
        if diff.is_empty() {
            return;
        }
        let branch = self
            .lock()
            .map(|state| state.branch.clone())
            .unwrap_or_default();
        let content = format!(
            "Observed external workspace changes on branch {branch}.\nold snapshot: {old}\nnew snapshot: {new}\n\n{diff}"
        );
        let _ = self.events.send(GitUiEvent::Sensory(content));
    }
}

fn unique_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:032x}-{:08x}-{counter:016x}", std::process::id())
}

impl GitState {
    fn repo_lock(&self) -> Result<RepoLock> {
        let path = self.state_dir.join("repository.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        file.lock()
            .with_context(|| format!("lock {}", path.display()))?;
        Ok(RepoLock(file))
    }

    fn validate_branch(&self) -> Result<()> {
        let current = command_text(
            &self.git,
            &self.repo_root,
            ["symbolic-ref", "--quiet", "--short", "HEAD"],
        )
        .context("parent worktree no longer has a checked-out branch")?;
        if current.trim() != self.branch {
            bail!(
                "parent branch changed from {} to {}; restore it or restart Nuillu Code",
                self.branch,
                current.trim()
            );
        }
        Ok(())
    }

    fn ui_state(&self) -> GitUiState {
        GitUiState {
            mode: self.mode,
            branch: self.branch.clone(),
            commits: self.commits.clone(),
        }
    }

    fn replay_onto(&mut self, baseline: &str, events: &mpsc::Sender<GitUiEvent>) -> Result<()> {
        let old = std::mem::take(&mut self.commits);
        run_git(&self.git, &self.worktree, ["reset", "--hard", baseline])?;
        self.baseline = baseline.to_owned();
        for mut item in old {
            match run_git(
                &self.git,
                &self.worktree,
                ["cherry-pick", "--no-gpg-sign", &item.id],
            ) {
                Ok(()) => {
                    let old_id = item.id.clone();
                    item.id = command_text(&self.git, &self.worktree, ["rev-parse", "HEAD"])?
                        .trim()
                        .to_owned();
                    item.diff = git_show(&self.git, &self.worktree, &item.id)?;
                    self.aliases.insert(old_id, item.id.clone());
                    self.commits.push(item);
                }
                Err(error) => {
                    let _ = run_git(&self.git, &self.worktree, ["cherry-pick", "--abort"]);
                    let tip = self
                        .commits
                        .last()
                        .map(|commit| commit.id.as_str())
                        .unwrap_or(baseline);
                    run_git(&self.git, &self.worktree, ["reset", "--hard", tip])?;
                    let message = format!(
                        "Discarded conflicting review commit {} ({}) because replay failed: {error:#}\n\n{}",
                        item.id, item.purpose, item.diff
                    );
                    let _ = events.send(GitUiEvent::Sensory(message));
                }
            }
        }
        if self.commits.is_empty() {
            self.aliases.clear();
        }
        Ok(())
    }

    fn collapse_dependent_commits(&mut self) -> Result<()> {
        if self.commits.len() < 2 {
            return Ok(());
        }
        let newest = self.commits.last().expect("checked length").clone();
        if patch_applies_to_tree(
            &self.git,
            &self.repo_root,
            &self.state_dir,
            &self.baseline,
            &newest.diff,
        )? {
            return Ok(());
        }

        let dependency = self.commits[..self.commits.len() - 1]
            .iter()
            .enumerate()
            .find_map(|(index, candidate)| {
                patch_applies_after_patch(
                    &self.git,
                    &self.repo_root,
                    &self.state_dir,
                    &self.baseline,
                    &candidate.diff,
                    &newest.diff,
                )
                .ok()
                .filter(|applies| *applies)
                .map(|_| index)
            });
        if let Some(dependency) = dependency {
            return self.merge_dependency_component(dependency);
        }

        // A patch depending on multiple prior components is uncommon and cannot be separated
        // safely by a single-component replay test. Conservatively combine the queue.
        let purposes = self
            .commits
            .iter()
            .map(|item| item.purpose.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        let paths = self
            .commits
            .iter()
            .flat_map(|item| item.changed_paths.iter().cloned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        run_git(
            &self.git,
            &self.worktree,
            ["reset", "--soft", &self.baseline],
        )?;
        git_commit(&self.git, &self.worktree, &purposes)?;
        let id = command_text(&self.git, &self.worktree, ["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let diff = git_show(&self.git, &self.worktree, &id)?;
        self.commits = vec![ReviewCommit {
            id,
            purpose: purposes,
            diff,
            changed_paths: paths,
        }];
        Ok(())
    }

    fn merge_dependency_component(&mut self, dependency: usize) -> Result<()> {
        let original = self.commits.clone();
        let original_tip = original
            .last()
            .expect("dependency queue is not empty")
            .id
            .clone();
        let newest = original
            .last()
            .expect("dependency queue is not empty")
            .clone();
        let result = (|| -> Result<Vec<ReviewCommit>> {
            run_git(
                &self.git,
                &self.worktree,
                ["reset", "--hard", &self.baseline],
            )?;
            let mut rebuilt = Vec::new();
            for (index, item) in original[..original.len() - 1].iter().enumerate() {
                apply_patch_to_worktree(&self.git, &self.worktree, &item.diff)?;
                let (purpose, paths) = if index == dependency {
                    apply_patch_to_worktree(&self.git, &self.worktree, &newest.diff)?;
                    let paths = item
                        .changed_paths
                        .iter()
                        .chain(newest.changed_paths.iter())
                        .cloned()
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect();
                    (format!("{}; {}", item.purpose, newest.purpose), paths)
                } else {
                    (item.purpose.clone(), item.changed_paths.clone())
                };
                run_git(&self.git, &self.worktree, ["add", "-A", "--", "."])?;
                git_commit(&self.git, &self.worktree, &purpose)?;
                let id = command_text(&self.git, &self.worktree, ["rev-parse", "HEAD"])?
                    .trim()
                    .to_owned();
                let diff = git_show(&self.git, &self.worktree, &id)?;
                if index != dependency {
                    self.aliases.insert(item.id.clone(), id.clone());
                }
                rebuilt.push(ReviewCommit {
                    id,
                    purpose,
                    diff,
                    changed_paths: paths,
                });
            }
            Ok(rebuilt)
        })();
        match result {
            Ok(rebuilt) => {
                self.commits = rebuilt;
                Ok(())
            }
            Err(error) => {
                let _ = run_git(
                    &self.git,
                    &self.worktree,
                    ["reset", "--hard", &original_tip],
                );
                self.commits = original;
                Err(error).context("rebuild dependent review component")
            }
        }
    }

    fn apply_patch_to_parent(&self, diff: &str) -> Result<()> {
        run_git_with_input(
            &self.git,
            &self.repo_root,
            ["apply", "--check", "--binary", "-"],
            diff.as_bytes(),
        )?;
        run_git_with_input(
            &self.git,
            &self.repo_root,
            ["apply", "--binary", "-"],
            diff.as_bytes(),
        )
    }

    fn reverse_patch_on_parent(&self, diff: &str) -> Result<()> {
        run_git_with_input(
            &self.git,
            &self.repo_root,
            ["apply", "--check", "--reverse", "--binary", "-"],
            diff.as_bytes(),
        )?;
        run_git_with_input(
            &self.git,
            &self.repo_root,
            ["apply", "--reverse", "--binary", "-"],
            diff.as_bytes(),
        )
    }

    fn apply_one(&mut self, id: &str, events: &mpsc::Sender<GitUiEvent>) -> Result<()> {
        let id = self.resolve_alias(id);
        let index = self
            .commits
            .iter()
            .position(|item| item.id == id)
            .context("review commit not found")?;
        let item = self.commits[index].clone();
        let original = self.commits.clone();
        let baseline = self.baseline.clone();
        self.apply_patch_to_parent(&item.diff)?;
        self.commits.remove(index);
        if let Err(error) = self.adopt_parent_and_replay(events) {
            self.reverse_patch_on_parent(&item.diff)
                .context("individual Apply failed and parent rollback also failed")?;
            self.commits = original;
            self.replay_onto(&baseline, events)?;
            return Err(error).context("finalize individual Apply");
        }
        Ok(())
    }

    fn discard_one(&mut self, id: &str, events: &mpsc::Sender<GitUiEvent>) -> Result<()> {
        let id = self.resolve_alias(id);
        let index = self
            .commits
            .iter()
            .position(|item| item.id == id)
            .context("review commit not found")?;
        self.commits.remove(index);
        let baseline = self.baseline.clone();
        self.replay_onto(&baseline, events)
    }

    fn resolve_alias(&self, id: &str) -> String {
        let mut current = id.to_owned();
        let mut seen = BTreeSet::new();
        while seen.insert(current.clone()) {
            let Some(next) = self.aliases.get(&current) else {
                break;
            };
            current = next.clone();
        }
        current
    }

    fn apply_all(&mut self, events: &mpsc::Sender<GitUiEvent>) -> Result<()> {
        if self.commits.is_empty() {
            return Ok(());
        }
        let tip = self.commits.last().expect("not empty").id.clone();
        let diff = git_diff(&self.git, &self.worktree, &self.baseline, &tip)?;
        let original = self.commits.clone();
        let baseline = self.baseline.clone();
        self.apply_patch_to_parent(&diff)?;
        self.commits.clear();
        if let Err(error) = self.adopt_parent_and_replay(events) {
            self.reverse_patch_on_parent(&diff)
                .context("Apply-all failed and parent rollback also failed")?;
            self.commits = original;
            self.replay_onto(&baseline, events)?;
            return Err(error).context("finalize Apply-all");
        }
        Ok(())
    }

    fn adopt_parent_and_replay(&mut self, events: &mpsc::Sender<GitUiEvent>) -> Result<()> {
        let head = command_text(&self.git, &self.repo_root, ["rev-parse", "HEAD"])?;
        let snapshot = snapshot_parent(&self.git, &self.repo_root, &self.state_dir, head.trim())?;
        self.observed = snapshot.clone();
        self.replay_onto(&snapshot, events)
    }

    fn set_mode(&mut self, mode: WorkspaceMode, events: &mpsc::Sender<GitUiEvent>) -> Result<()> {
        if self.mode == mode {
            return Ok(());
        }
        if self.mode == WorkspaceMode::Review && mode == WorkspaceMode::Write {
            self.apply_all(events)?;
        }
        self.mode = mode;
        Ok(())
    }
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

fn base_command(git: &GitExecutable, cwd: &Path) -> Result<Command> {
    git.revalidate()?;
    let mut command = Command::new(&git.identity.canonical_path);
    command
        .current_dir(cwd)
        .env_clear()
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgSign=false",
            "-c",
            "core.fsmonitor=false",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    Ok(command)
}

fn run_git<'a>(
    git: &GitExecutable,
    cwd: &Path,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let mut command = base_command(git, cwd)?;
    command.args(args);
    checked_output(command).map(|_| ())
}

fn command_text<'a>(
    git: &GitExecutable,
    cwd: &Path,
    args: impl IntoIterator<Item = &'a str>,
) -> Result<String> {
    let mut command = base_command(git, cwd)?;
    command.args(args);
    let output = checked_output(command)?;
    String::from_utf8(output.stdout).context("Git output was not UTF-8")
}

fn run_git_with_input<'a>(
    git: &GitExecutable,
    cwd: &Path,
    args: impl IntoIterator<Item = &'a str>,
    input: &[u8],
) -> Result<()> {
    let mut command = base_command(git, cwd)?;
    command.args(args).stdin(Stdio::piped());
    let mut child = command.spawn().context("start git")?;
    child
        .stdin
        .take()
        .context("open git stdin")?
        .write_all(input)?;
    check_status(child.wait_with_output().context("wait for git")?).map(|_| ())
}

fn checked_output(mut command: Command) -> Result<Output> {
    check_status(command.output().context("start git")?)
}

fn check_status(output: Output) -> Result<Output> {
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    bail!("git failed with {}: {}", output.status, stderr.trim())
}

fn validate_repository_state(git: &GitExecutable, root: &Path) -> Result<()> {
    let common = command_text(git, root, ["rev-parse", "--git-common-dir"])?;
    let common = root.join(common.trim());
    let worktree_git = command_text(git, root, ["rev-parse", "--git-dir"])?;
    let worktree_git = root.join(worktree_git.trim());
    for directory in [&common, &worktree_git] {
        for marker in [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "REBASE_HEAD",
        ] {
            if directory.join(marker).exists() {
                bail!("repository has an in-progress Git operation ({marker})");
            }
        }
        if directory.join("rebase-merge").exists() || directory.join("rebase-apply").exists() {
            bail!("repository has an in-progress rebase");
        }
    }
    if !command_text(git, root, ["ls-files", "-u"])?.is_empty() {
        bail!("repository index contains unmerged entries");
    }
    Ok(())
}

fn reject_external_filters(git: &GitExecutable, root: &Path) -> Result<()> {
    if command_text(git, root, ["config", "--local", "--get", "diff.external"])
        .is_ok_and(|value| !value.trim().is_empty())
    {
        bail!("repository-local external diff is not supported");
    }
    if command_text(
        git,
        root,
        [
            "config",
            "--local",
            "--get-regexp",
            "^diff\\..*\\.textconv$",
        ],
    )
    .is_ok_and(|value| !value.trim().is_empty())
    {
        bail!("repository-local textconv is not supported");
    }
    let configured = command_text(
        git,
        root,
        ["config", "--local", "--get-regexp", "^filter\\."],
    )
    .unwrap_or_default();
    if !configured.trim().is_empty() {
        bail!("custom Git clean/smudge filters are not supported");
    }
    if command_text(
        git,
        root,
        ["config", "--local", "--get", "core.attributesfile"],
    )
    .is_ok_and(|value| !value.trim().is_empty())
    {
        bail!("repository-local core.attributesFile is not supported");
    }
    let mut directories = vec![root.to_path_buf()];
    while let Some(directory) = directories.pop() {
        for entry in fs::read_dir(&directory)
            .with_context(|| format!("inspect attributes under {}", directory.display()))?
        {
            let entry = entry?;
            let file_type = entry.file_type()?;
            let name = entry.file_name();
            if file_type.is_dir() {
                if name != ".git" && name != ".nuillu" {
                    directories.push(entry.path());
                }
            } else if name == ".gitattributes" {
                let content = fs::read_to_string(entry.path()).unwrap_or_default();
                if content.lines().any(|line| line.contains("filter=")) {
                    bail!(
                        "custom Git filter attribute is not supported: {}",
                        entry.path().display()
                    );
                }
            }
        }
    }
    let common = command_text(git, root, ["rev-parse", "--git-common-dir"])?;
    let info_attributes = root.join(common.trim()).join("info").join("attributes");
    if fs::read_to_string(&info_attributes)
        .unwrap_or_default()
        .lines()
        .any(|line| line.contains("filter="))
    {
        bail!("Git info/attributes contains a custom filter");
    }
    Ok(())
}

fn validate_state_ignore(root: &Path) -> Result<()> {
    let path = root.join(".gitignore");
    let content =
        fs::read_to_string(&path).with_context(|| format!("read required {}", path.display()))?;
    let accepted = [".nuillu", ".nuillu/", "/.nuillu", "/.nuillu/"];
    if !content
        .lines()
        .map(str::trim)
        .any(|line| accepted.contains(&line))
    {
        bail!("repository .gitignore must contain a root .nuillu ignore rule");
    }
    Ok(())
}

fn snapshot_parent(
    git: &GitExecutable,
    root: &Path,
    state_dir: &Path,
    head: &str,
) -> Result<String> {
    let index = state_dir.join(format!("snapshot-{}.index", unique_id()));
    let mut command = base_command(git, root)?;
    command
        .env("GIT_INDEX_FILE", &index)
        .args(["read-tree", head]);
    checked_output(command)?;
    let mut command = base_command(git, root)?;
    command
        .env("GIT_INDEX_FILE", &index)
        .args(["add", "-A", "--", "."]);
    if let Err(error) = checked_output(command) {
        let _ = fs::remove_file(&index);
        return Err(error).context("snapshot parent working tree");
    }
    let mut command = base_command(git, root)?;
    command.env("GIT_INDEX_FILE", &index).arg("write-tree");
    let tree = String::from_utf8(checked_output(command)?.stdout)?;
    let mut command = base_command(git, root)?;
    command
        .env("GIT_AUTHOR_NAME", AGENT_NAME)
        .env("GIT_AUTHOR_EMAIL", AGENT_EMAIL)
        .env("GIT_COMMITTER_NAME", AGENT_NAME)
        .env("GIT_COMMITTER_EMAIL", AGENT_EMAIL)
        .args([
            "commit-tree",
            tree.trim(),
            "-p",
            head,
            "-m",
            "Nuillu parent snapshot",
        ]);
    let result = checked_output(command);
    let _ = fs::remove_file(&index);
    Ok(String::from_utf8(result?.stdout)?.trim().to_owned())
}

fn git_commit(git: &GitExecutable, cwd: &Path, message: &str) -> Result<()> {
    let mut command = base_command(git, cwd)?;
    command
        .env("GIT_AUTHOR_NAME", AGENT_NAME)
        .env("GIT_AUTHOR_EMAIL", AGENT_EMAIL)
        .env("GIT_COMMITTER_NAME", AGENT_NAME)
        .env("GIT_COMMITTER_EMAIL", AGENT_EMAIL)
        .args(["commit", "--no-verify", "--no-gpg-sign", "-m", message]);
    checked_output(command).map(|_| ())
}

fn cherry_pick_as_agent(git: &GitExecutable, cwd: &Path, commit: &str) -> Result<()> {
    let mut command = base_command(git, cwd)?;
    command
        .env("GIT_AUTHOR_NAME", AGENT_NAME)
        .env("GIT_AUTHOR_EMAIL", AGENT_EMAIL)
        .env("GIT_COMMITTER_NAME", AGENT_NAME)
        .env("GIT_COMMITTER_EMAIL", AGENT_EMAIL)
        .args(["cherry-pick", "--no-gpg-sign", commit]);
    checked_output(command).map(|_| ())
}

fn capture_conflict(git: &GitExecutable, worktree: &Path, purpose: &str) -> Result<ConflictCase> {
    let paths = command_text(
        git,
        worktree,
        [
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            "--diff-filter=U",
        ],
    )?;
    let mut files = Vec::new();
    for path in paths.lines().filter(|path| !path.is_empty()) {
        validate_conflict_path(path)?;
        let target = worktree.join(path);
        if fs::symlink_metadata(&target).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            bail!("conflict resolver cannot read a symbolic link: {path}");
        }
        if fs::metadata(&target).is_ok_and(|metadata| metadata.len() > MAX_CONFLICT_FILE_BYTES) {
            bail!("conflict file exceeds 16 MiB: {path}");
        }
        let content = match fs::read(&target) {
            Ok(bytes) => Some(
                String::from_utf8(bytes)
                    .with_context(|| format!("conflict file is not UTF-8: {path}"))?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).with_context(|| format!("read conflict file {path}")),
        };
        files.push(ConflictFile {
            path: path.to_owned(),
            content,
        });
    }
    if files.is_empty() {
        bail!("cherry-pick failed without reporting conflicted files");
    }
    Ok(ConflictCase {
        purpose: purpose.to_owned(),
        files,
    })
}

fn apply_conflict_resolution(
    git: &GitExecutable,
    worktree: &Path,
    conflict: &ConflictCase,
    resolved: &[ConflictFile],
) -> Result<()> {
    let expected = conflict
        .files
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    let actual = resolved
        .iter()
        .map(|file| file.path.as_str())
        .collect::<BTreeSet<_>>();
    if expected != actual || actual.len() != resolved.len() {
        bail!("conflict resolver must return every conflicted path exactly once");
    }
    for file in resolved {
        validate_conflict_path(&file.path)?;
        let target = worktree.join(&file.path);
        if target
            .symlink_metadata()
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            bail!(
                "conflict resolver cannot write a symbolic link: {}",
                file.path
            );
        }
        match &file.content {
            Some(content) => {
                if contains_conflict_markers(content) {
                    bail!("conflict markers remain in {}", file.path);
                }
                fs::write(&target, content)
                    .with_context(|| format!("write resolved conflict {}", file.path))?;
            }
            None => match fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("delete resolved conflict {}", file.path));
                }
            },
        }
    }
    let mut args = vec!["add", "-A", "--"];
    args.extend(resolved.iter().map(|file| file.path.as_str()));
    run_git(git, worktree, args)?;
    run_git(
        git,
        worktree,
        [
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--cached",
            "--check",
        ],
    )
}

fn validate_conflict_path(path: &str) -> Result<()> {
    if path.is_empty()
        || Path::new(path)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        bail!("invalid conflicted path: {path}");
    }
    Ok(())
}

fn contains_conflict_markers(content: &str) -> bool {
    content.lines().any(|line| {
        line.starts_with("<<<<<<< ") || line == "=======" || line.starts_with(">>>>>>> ")
    })
}

fn git_diff(git: &GitExecutable, cwd: &Path, old: &str, new: &str) -> Result<String> {
    command_text(
        git,
        cwd,
        [
            "diff",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            old,
            new,
        ],
    )
}

fn git_show(git: &GitExecutable, cwd: &Path, commit: &str) -> Result<String> {
    command_text(
        git,
        cwd,
        [
            "show",
            "--format=",
            "--binary",
            "--no-ext-diff",
            "--no-textconv",
            commit,
        ],
    )
}

fn same_tree(git: &GitExecutable, cwd: &Path, left: &str, right: &str) -> Result<bool> {
    let left = command_text(git, cwd, ["rev-parse", &format!("{left}^{{tree}}")])?;
    let right = command_text(git, cwd, ["rev-parse", &format!("{right}^{{tree}}")])?;
    Ok(left.trim() == right.trim())
}

fn patch_applies_to_tree(
    git: &GitExecutable,
    root: &Path,
    state_dir: &Path,
    tree: &str,
    patch: &str,
) -> Result<bool> {
    let index = state_dir.join(format!("dependency-{}.index", unique_id()));
    let mut command = base_command(git, root)?;
    command
        .env("GIT_INDEX_FILE", &index)
        .args(["read-tree", tree]);
    checked_output(command)?;
    let mut command = base_command(git, root)?;
    command
        .env("GIT_INDEX_FILE", &index)
        .args(["apply", "--cached", "--check", "--binary", "-"])
        .stdin(Stdio::piped());
    let mut child = command.spawn().context("start git dependency check")?;
    child
        .stdin
        .take()
        .context("open git stdin")?
        .write_all(patch.as_bytes())?;
    let output = child.wait_with_output()?;
    let _ = fs::remove_file(index);
    Ok(output.status.success())
}

fn patch_applies_after_patch(
    git: &GitExecutable,
    root: &Path,
    state_dir: &Path,
    tree: &str,
    first: &str,
    second: &str,
) -> Result<bool> {
    let index = state_dir.join(format!("dependency-pair-{}.index", unique_id()));
    let result = (|| -> Result<bool> {
        let mut command = base_command(git, root)?;
        command
            .env("GIT_INDEX_FILE", &index)
            .args(["read-tree", tree]);
        checked_output(command)?;
        let mut command = base_command(git, root)?;
        command
            .env("GIT_INDEX_FILE", &index)
            .args(["apply", "--cached", "--binary", "-"])
            .stdin(Stdio::piped());
        let mut child = command.spawn().context("start first dependency apply")?;
        child
            .stdin
            .take()
            .context("open first dependency stdin")?
            .write_all(first.as_bytes())?;
        if !child.wait_with_output()?.status.success() {
            return Ok(false);
        }
        let mut command = base_command(git, root)?;
        command
            .env("GIT_INDEX_FILE", &index)
            .args(["apply", "--cached", "--check", "--binary", "-"])
            .stdin(Stdio::piped());
        let mut child = command.spawn().context("start second dependency check")?;
        child
            .stdin
            .take()
            .context("open second dependency stdin")?
            .write_all(second.as_bytes())?;
        Ok(child.wait_with_output()?.status.success())
    })();
    let _ = fs::remove_file(index);
    result
}

fn apply_patch_to_worktree(git: &GitExecutable, worktree: &Path, patch: &str) -> Result<()> {
    run_git_with_input(
        git,
        worktree,
        ["apply", "--check", "--binary", "-"],
        patch.as_bytes(),
    )?;
    run_git_with_input(git, worktree, ["apply", "--binary", "-"], patch.as_bytes())
}

fn cleanup_stale_worktrees(git: &GitExecutable, root: &Path, state_dir: &Path) -> Result<()> {
    let repository_lock_path = state_dir.join("repository.lock");
    let repository_lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&repository_lock_path)?;
    repository_lock
        .lock()
        .with_context(|| format!("lock {}", repository_lock_path.display()))?;
    let _repository_lock = RepoLock(repository_lock);
    let locks = state_dir.join("worktree-locks");
    let now = SystemTime::now();
    for entry in fs::read_dir(&locks).context("list worktree locks")? {
        let entry = entry?;
        let modified = entry.metadata()?.modified().unwrap_or(now);
        if now.duration_since(modified).unwrap_or_default() < STALE_WORKTREE_AGE {
            continue;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(entry.path())?;
        if file.try_lock().is_err() {
            continue;
        }
        let Some(id) = entry
            .path()
            .file_stem()
            .and_then(|name| name.to_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let worktree = state_dir.join("worktrees").join(&id);
        let branch = format!("nuillu/session/{id}");
        let worktree_text = worktree.to_str().unwrap_or_default();
        let _ = run_git(git, root, ["worktree", "remove", "--force", worktree_text]);
        let _ = run_git(git, root, ["branch", "-D", &branch]);
        let _ = fs::remove_file(entry.path());
    }
    let _ = run_git(git, root, ["worktree", "prune"]);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestRepository {
        root: PathBuf,
        git: GitExecutable,
    }

    impl TestRepository {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("nuillu-code-git-{name}-{}", unique_id()));
            fs::create_dir_all(&root).unwrap();
            let git = GitExecutable::discover().unwrap();
            run_git(&git, &root, ["init", "-b", "main"]).unwrap();
            fs::write(root.join(".gitignore"), ".nuillu/\n").unwrap();
            fs::write(root.join("tracked.txt"), "base\n").unwrap();
            run_git(&git, &root, ["add", ".gitignore", "tracked.txt"]).unwrap();
            git_commit(&git, &root, "initial").unwrap();
            Self { root, git }
        }
    }

    impl Drop for TestRepository {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    struct FixedResolver;

    #[async_trait(?Send)]
    impl ConflictResolver for FixedResolver {
        async fn resolve(&mut self, conflict: &ConflictCase) -> Result<Vec<ConflictFile>> {
            Ok(conflict
                .files
                .iter()
                .map(|file| ConflictFile {
                    path: file.path.clone(),
                    content: Some("resolved\n".to_owned()),
                })
                .collect())
        }
    }

    #[test]
    fn generated_session_ids_are_distinct_and_ref_safe() {
        let first = unique_id();
        let second = unique_id();
        assert_ne!(first, second);
        assert!(
            first
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
        );
    }

    #[test]
    fn repository_local_git_executable_is_rejected_and_identity_is_revalidated() {
        let repo = TestRepository::new("git-identity");
        let local_git = repo.root.join("git");
        fs::write(&local_git, "first").unwrap();
        let executable = GitExecutable {
            identity: ExecutableIdentity::read(&local_git).unwrap(),
        };
        assert!(executable.reject_inside(&repo.root).is_err());
        assert!(executable.revalidate().is_ok());
        fs::write(&local_git, "changed identity").unwrap();
        assert!(executable.revalidate().is_err());
    }

    #[test]
    fn snapshot_includes_untracked_without_touching_parent_index() {
        let repo = TestRepository::new("snapshot");
        fs::write(repo.root.join("untracked.txt"), "outside\n").unwrap();
        let open = GitWorkspace::open(&repo.root).unwrap();
        assert_eq!(
            fs::read_to_string(open.workspace.root().join("untracked.txt")).unwrap(),
            "outside\n"
        );
        assert!(
            command_text(&repo.git, &repo.root, ["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        assert!(
            open.ui_events
                .try_iter()
                .all(|event| !matches!(event, GitUiEvent::Sensory(_))),
            "the startup snapshot is the observation baseline"
        );
        open.git.cleanup().unwrap();
    }

    #[test]
    fn write_preserves_preexisting_staged_and_unstaged_state() {
        let repo = TestRepository::new("index-preservation");
        fs::write(repo.root.join("tracked.txt"), "staged\n").unwrap();
        run_git(&repo.git, &repo.root, ["add", "tracked.txt"]).unwrap();
        fs::write(repo.root.join("tracked.txt"), "unstaged\n").unwrap();
        let cached_before = command_text(&repo.git, &repo.root, ["diff", "--cached"]).unwrap();
        let unstaged_before = command_text(&repo.git, &repo.root, ["diff"]).unwrap();

        let open = GitWorkspace::open(&repo.root).unwrap();
        open.git
            .handle_control(GitControlCommand::SetMode(WorkspaceMode::Write))
            .unwrap();
        fs::write(open.workspace.root().join("agent.txt"), "agent\n").unwrap();
        open.git
            .finish_patch("agent file", "", vec!["agent.txt".to_owned()])
            .unwrap();

        assert_eq!(
            command_text(&repo.git, &repo.root, ["diff", "--cached"]).unwrap(),
            cached_before
        );
        assert!(
            command_text(&repo.git, &repo.root, ["diff"])
                .unwrap()
                .starts_with(&unstaged_before)
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("agent.txt")).unwrap(),
            "agent\n"
        );
        open.git.cleanup().unwrap();
    }

    #[test]
    fn review_waits_and_write_auto_applies_without_staging() {
        let repo = TestRepository::new("modes");
        let open = GitWorkspace::open(&repo.root).unwrap();
        open.git
            .handle_control(GitControlCommand::SetMode(WorkspaceMode::Review))
            .unwrap();
        fs::write(open.workspace.root().join("tracked.txt"), "review\n").unwrap();
        let review = open
            .git
            .finish_patch("review change", "", vec!["tracked.txt".to_owned()])
            .unwrap();
        let PatchDisposition::Review { review_commit } = review else {
            panic!("Review mode must retain a commit")
        };
        assert_eq!(
            fs::read_to_string(repo.root.join("tracked.txt")).unwrap(),
            "base\n"
        );
        open.git
            .handle_control(GitControlCommand::Apply(review_commit))
            .unwrap();
        assert_eq!(
            fs::read_to_string(repo.root.join("tracked.txt")).unwrap(),
            "review\n"
        );
        assert!(
            command_text(&repo.git, &repo.root, ["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );

        open.git
            .handle_control(GitControlCommand::SetMode(WorkspaceMode::Write))
            .unwrap();
        fs::write(open.workspace.root().join("tracked.txt"), "write\n").unwrap();
        assert_eq!(
            open.git
                .finish_patch("write change", "", vec!["tracked.txt".to_owned()])
                .unwrap(),
            PatchDisposition::WriteApplied
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("tracked.txt")).unwrap(),
            "write\n"
        );
        assert!(
            command_text(&repo.git, &repo.root, ["diff", "--cached", "--name-only"])
                .unwrap()
                .is_empty()
        );
        open.git.cleanup().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn conflicting_review_commit_is_resolved_and_recommitted() {
        let repo = TestRepository::new("conflict");
        let open = GitWorkspace::open(&repo.root).unwrap();
        open.git
            .handle_control(GitControlCommand::SetMode(WorkspaceMode::Review))
            .unwrap();
        fs::write(open.workspace.root().join("tracked.txt"), "agent\n").unwrap();
        open.git
            .finish_patch("agent change", "", vec!["tracked.txt".to_owned()])
            .unwrap();
        fs::write(repo.root.join("tracked.txt"), "user\n").unwrap();

        open.git
            .sync_with_resolver(&mut FixedResolver)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(open.workspace.root().join("tracked.txt")).unwrap(),
            "resolved\n"
        );
        assert_eq!(open.git.lock().unwrap().commits.len(), 1);
        assert_eq!(
            fs::read_to_string(repo.root.join("tracked.txt")).unwrap(),
            "user\n"
        );
        open.git.cleanup().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn apply_all_stops_after_conflict_resolution_for_review() {
        let repo = TestRepository::new("conflict-review");
        let open = GitWorkspace::open(&repo.root).unwrap();
        open.git
            .handle_control(GitControlCommand::SetMode(WorkspaceMode::Review))
            .unwrap();
        fs::write(open.workspace.root().join("tracked.txt"), "agent\n").unwrap();
        open.git
            .finish_patch("agent change", "", vec!["tracked.txt".to_owned()])
            .unwrap();
        fs::write(repo.root.join("tracked.txt"), "user\n").unwrap();

        open.git
            .handle_control_with_resolver(GitControlCommand::ApplyAll, &mut FixedResolver)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(repo.root.join("tracked.txt")).unwrap(),
            "user\n"
        );
        assert_eq!(open.git.lock().unwrap().commits.len(), 1);

        open.git
            .handle_control_with_resolver(GitControlCommand::ApplyAll, &mut FixedResolver)
            .await
            .unwrap();
        assert_eq!(
            fs::read_to_string(repo.root.join("tracked.txt")).unwrap(),
            "resolved\n"
        );
        assert!(open.git.lock().unwrap().commits.is_empty());
        open.git.cleanup().unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn own_apply_is_silent_but_later_external_change_is_sensory() {
        let repo = TestRepository::new("sensory-origin");
        let open = GitWorkspace::open(&repo.root).unwrap();
        while open.ui_events.try_recv().is_ok() {}
        open.git
            .handle_control(GitControlCommand::SetMode(WorkspaceMode::Write))
            .unwrap();
        fs::write(open.workspace.root().join("tracked.txt"), "agent\n").unwrap();
        open.git
            .finish_patch("agent change", "", vec!["tracked.txt".to_owned()])
            .unwrap();
        assert!(
            open.ui_events
                .try_iter()
                .all(|event| !matches!(event, GitUiEvent::Sensory(_)))
        );

        fs::write(repo.root.join("tracked.txt"), "external\n").unwrap();
        open.git
            .sync_with_resolver(&mut FixedResolver)
            .await
            .unwrap();
        let sensory = open
            .ui_events
            .try_iter()
            .find_map(|event| match event {
                GitUiEvent::Sensory(content) => Some(content),
                _ => None,
            })
            .expect("external change must produce sensory input");
        assert!(sensory.contains("+external"));
        open.git
            .sync_with_resolver(&mut FixedResolver)
            .await
            .unwrap();
        assert!(
            open.ui_events
                .try_iter()
                .all(|event| !matches!(event, GitUiEvent::Sensory(_)))
        );
        open.git.cleanup().unwrap();
    }

    #[test]
    fn dependent_patch_merges_only_its_git_replay_component() {
        let repo = TestRepository::new("dependency-component");
        let open = GitWorkspace::open(&repo.root).unwrap();
        open.git
            .handle_control(GitControlCommand::SetMode(WorkspaceMode::Review))
            .unwrap();

        fs::write(open.workspace.root().join("tracked.txt"), "first\n").unwrap();
        open.git
            .finish_patch("first", "", vec!["tracked.txt".to_owned()])
            .unwrap();
        fs::write(open.workspace.root().join("independent.txt"), "other\n").unwrap();
        open.git
            .finish_patch("independent", "", vec!["independent.txt".to_owned()])
            .unwrap();
        assert_eq!(open.git.lock().unwrap().commits.len(), 2);

        fs::write(open.workspace.root().join("tracked.txt"), "second\n").unwrap();
        open.git
            .finish_patch("second", "", vec!["tracked.txt".to_owned()])
            .unwrap();
        let commits = open.git.lock().unwrap().commits.clone();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].purpose, "first; second");
        assert_eq!(commits[1].purpose, "independent");
        open.git
            .handle_control(GitControlCommand::Apply(commits[1].id.clone()))
            .unwrap();
        assert_eq!(
            fs::read_to_string(repo.root.join("independent.txt")).unwrap(),
            "other\n"
        );
        assert_eq!(
            fs::read_to_string(repo.root.join("tracked.txt")).unwrap(),
            "base\n"
        );
        let remaining = open.git.lock().unwrap().commits.clone();
        assert_eq!(remaining.len(), 1);
        open.git
            .handle_control(GitControlCommand::Discard(remaining[0].id.clone()))
            .unwrap();
        assert!(open.git.lock().unwrap().commits.is_empty());
        open.git.cleanup().unwrap();
    }

    #[test]
    fn parent_branch_change_blocks_controls() {
        let repo = TestRepository::new("branch-change");
        let open = GitWorkspace::open(&repo.root).unwrap();
        run_git(&repo.git, &repo.root, ["switch", "-c", "other"]).unwrap();
        let error = open
            .git
            .handle_control(GitControlCommand::SetMode(WorkspaceMode::Review))
            .unwrap_err();
        assert!(format!("{error:#}").contains("parent branch changed"));
        run_git(&repo.git, &repo.root, ["switch", "main"]).unwrap();
        open.git.cleanup().unwrap();
    }

    #[test]
    fn repository_filter_configuration_is_rejected() {
        let repo = TestRepository::new("filter");
        run_git(
            &repo.git,
            &repo.root,
            ["config", "filter.unsafe.clean", "some-process"],
        )
        .unwrap();
        let error = match GitWorkspace::open(&repo.root) {
            Ok(_) => panic!("custom filter must be rejected"),
            Err(error) => error,
        };
        assert!(format!("{error:#}").contains("clean/smudge filters"));
    }
}
