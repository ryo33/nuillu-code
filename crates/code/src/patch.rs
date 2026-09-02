use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::git::ConflictResolver;
use crate::workspace::transaction_file_name;
use crate::{
    GitWorkspace, PatchDisposition, Workspace, WorkspaceMode, WorkspaceToolOutput, sha256_hex,
};

const MAX_OPERATIONS: usize = 32;
const PATCH_DIFF_BYTE_LIMIT: usize = 256 * 1024;
static NEXT_TRANSACTION_ID: AtomicU64 = AtomicU64::new(1);
type PatchToolOutput = WorkspaceToolOutput<PatchOutput>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Replacement {
    pub old: String,
    pub new: String,
    pub expected_occurrences: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum PatchOperation {
    Create {
        path: String,
        content: String,
    },
    Update {
        path: String,
        preimage_sha256: String,
        replacements: Vec<Replacement>,
    },
    Delete {
        path: String,
        preimage_sha256: String,
    },
    Rename {
        from: String,
        to: String,
        preimage_sha256: String,
    },
}

/// Propose or atomically apply a structured transaction of create, update, delete, and rename operations. Updates use exact replacements and every existing source requires the SHA-256 returned by read.
#[lutum::tool_input(name = "patch", output = PatchToolOutput)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchInput {
    pub operations: Vec<PatchOperation>,
    pub purpose: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PatchOutput {
    pub applied: bool,
    pub rejected: bool,
    pub changed_paths: Vec<String>,
    pub message: String,
    pub mode: String,
    #[serde(default)]
    pub review_commit: Option<String>,
    pub auto_applied: bool,
}

#[derive(Clone)]
pub(crate) struct PatchExecutor {
    workspace: Workspace,
    git: GitWorkspace,
}

impl PatchExecutor {
    pub fn new_git(workspace: Workspace, git: GitWorkspace) -> Self {
        Self { workspace, git }
    }

    pub async fn execute_with_resolver<R: ConflictResolver>(
        &self,
        input: &PatchInput,
        resolver: &mut R,
    ) -> Result<PatchOutput> {
        self.git
            .sync_with_resolver(resolver)
            .await
            .context("sync parent before patch")?;
        if self.git.mode()? == WorkspaceMode::ReadOnly {
            bail!("workspace is read-only; select Review or Write before patching");
        }
        let proposal = prepare_patch(&self.workspace, input).await?;
        let changed_paths = apply_patch(&self.workspace, &proposal).await?;
        let disposition =
            self.git
                .finish_patch(&proposal.purpose, &proposal.diff, changed_paths.clone());
        let disposition = match disposition {
            Ok(disposition) => disposition,
            Err(error) => {
                let _ = self.git.discard_uncommitted();
                return Err(error);
            }
        };
        let (mode, review_commit, auto_applied, message) = match disposition {
            PatchDisposition::Review { review_commit } => (
                "review".to_owned(),
                Some(review_commit),
                false,
                "patch committed for review".to_owned(),
            ),
            PatchDisposition::WriteApplied => (
                "write".to_owned(),
                None,
                true,
                "patch automatically applied to the parent working tree".to_owned(),
            ),
        };
        Ok(PatchOutput {
            applied: true,
            rejected: false,
            changed_paths,
            message,
            mode,
            review_commit,
            auto_applied,
        })
    }
}

async fn prepare_patch(workspace: &Workspace, input: &PatchInput) -> Result<PreparedPatch> {
    if input.operations.is_empty() || input.operations.len() > MAX_OPERATIONS {
        bail!("patch must contain between 1 and {MAX_OPERATIONS} operations");
    }
    if input.purpose.trim().is_empty() {
        bail!("patch purpose must not be empty");
    }
    let mut touched = BTreeSet::new();
    let mut operations = Vec::with_capacity(input.operations.len());
    for operation in &input.operations {
        match operation {
            PatchOperation::Create { path, content } => {
                claim_path(&mut touched, path)?;
                workspace.validate_new_path(path)?;
                operations.push(PreparedOperation::Create {
                    path: path.clone(),
                    after: text_bytes(content)?,
                });
            }
            PatchOperation::Update {
                path,
                preimage_sha256,
                replacements,
            } => {
                claim_path(&mut touched, path)?;
                validate_hash(preimage_sha256)?;
                let before = workspace.read_visible_bytes(path).await?;
                verify_hash(path, &before, preimage_sha256)?;
                let after = apply_replacements(path, &before, replacements)?;
                operations.push(PreparedOperation::Update {
                    path: path.clone(),
                    before,
                    after,
                });
            }
            PatchOperation::Delete {
                path,
                preimage_sha256,
            } => {
                claim_path(&mut touched, path)?;
                validate_hash(preimage_sha256)?;
                let before = workspace.read_visible_bytes(path).await?;
                verify_hash(path, &before, preimage_sha256)?;
                operations.push(PreparedOperation::Delete {
                    path: path.clone(),
                    before,
                });
            }
            PatchOperation::Rename {
                from,
                to,
                preimage_sha256,
            } => {
                claim_path(&mut touched, from)?;
                claim_path(&mut touched, to)?;
                validate_hash(preimage_sha256)?;
                let before = workspace.read_visible_bytes(from).await?;
                verify_hash(from, &before, preimage_sha256)?;
                workspace.validate_new_path(to)?;
                operations.push(PreparedOperation::Rename {
                    from: from.clone(),
                    to: to.clone(),
                    before,
                });
            }
        }
    }
    Ok(PreparedPatch {
        purpose: input.purpose.trim().to_owned(),
        diff: render_diff(&operations),
        operations,
    })
}

async fn apply_patch(workspace: &Workspace, patch: &PreparedPatch) -> Result<Vec<String>> {
    let snapshots = snapshots(&patch.operations);
    let created_directories = created_directories(workspace.root(), &patch.operations);
    let result = apply_patch_inner(workspace, patch).await;
    if let Err(error) = result {
        rollback(workspace.root(), &snapshots).context("patch failed and rollback also failed")?;
        remove_created_directories(&created_directories)
            .context("patch failed and directory rollback also failed")?;
        return Err(error);
    }
    result
}

async fn apply_patch_inner(workspace: &Workspace, patch: &PreparedPatch) -> Result<Vec<String>> {
    let mut changed = BTreeSet::new();
    for operation in &patch.operations {
        match operation {
            PreparedOperation::Create { path, after } => {
                atomic_create(&workspace.resolve_relative(path)?, after)?;
                changed.insert(path.clone());
            }
            PreparedOperation::Update { path, after, .. } => {
                atomic_replace(&workspace.resolve_relative(path)?, after)?;
                changed.insert(path.clone());
            }
            PreparedOperation::Delete { path, .. } => {
                let target = workspace.validate_no_symlink_prefix(path, false)?;
                fs::remove_file(&target).with_context(|| format!("delete {}", target.display()))?;
                changed.insert(path.clone());
            }
            PreparedOperation::Rename { from, to, .. } => {
                let source = workspace.validate_no_symlink_prefix(from, false)?;
                let destination = workspace.validate_new_path(to)?;
                create_parent(&destination)?;
                fs::hard_link(&source, &destination).with_context(|| {
                    format!(
                        "link {} to {} without replacing",
                        source.display(),
                        destination.display()
                    )
                })?;
                fs::remove_file(&source)
                    .with_context(|| format!("remove renamed source {}", source.display()))?;
                changed.insert(from.clone());
                changed.insert(to.clone());
            }
        }
    }
    for operation in &patch.operations {
        let resulting_path = match operation {
            PreparedOperation::Create { path, .. } | PreparedOperation::Update { path, .. } => {
                Some(path)
            }
            PreparedOperation::Rename { to, .. } => Some(to),
            PreparedOperation::Delete { .. } => None,
        };
        if let Some(path) = resulting_path
            && !workspace.is_visible_existing(path).await?
        {
            bail!("resulting path is ignored by ripgrep: {path}");
        }
    }
    Ok(changed.into_iter().collect())
}

#[derive(Clone, Debug)]
enum PreparedOperation {
    Create {
        path: String,
        after: Vec<u8>,
    },
    Update {
        path: String,
        before: Vec<u8>,
        after: Vec<u8>,
    },
    Delete {
        path: String,
        before: Vec<u8>,
    },
    Rename {
        from: String,
        to: String,
        before: Vec<u8>,
    },
}

#[derive(Clone, Debug)]
struct PreparedPatch {
    purpose: String,
    operations: Vec<PreparedOperation>,
    diff: String,
}

fn claim_path(paths: &mut BTreeSet<String>, path: &str) -> Result<()> {
    if !paths.insert(path.to_ascii_lowercase()) {
        bail!("a path may occur only once per transaction: {path}");
    }
    Ok(())
}

fn validate_hash(hash: &str) -> Result<()> {
    if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("preimage_sha256 must contain exactly 64 hexadecimal characters");
    }
    Ok(())
}

fn verify_hash(path: &str, bytes: &[u8], expected: &str) -> Result<()> {
    if sha256_hex(bytes) != expected.to_ascii_lowercase() {
        bail!("preimage hash mismatch for {path}; read the file again");
    }
    Ok(())
}

fn text_bytes(content: &str) -> Result<Vec<u8>> {
    if content.as_bytes().contains(&0) {
        bail!("NUL bytes are forbidden");
    }
    Ok(content.as_bytes().to_vec())
}

fn apply_replacements(path: &str, before: &[u8], replacements: &[Replacement]) -> Result<Vec<u8>> {
    if replacements.is_empty() {
        bail!("update requires at least one replacement");
    }
    let mut text = std::str::from_utf8(before)
        .context("update target is not UTF-8")?
        .to_owned();
    for replacement in replacements {
        if replacement.old.is_empty() || replacement.expected_occurrences == 0 {
            bail!("replacement old text and expected_occurrences must be non-zero");
        }
        let occurrences = text.matches(&replacement.old).count();
        if occurrences != replacement.expected_occurrences {
            bail!(
                "replacement in {path} expected {} occurrences but found {occurrences}",
                replacement.expected_occurrences
            );
        }
        text = text.replace(&replacement.old, &replacement.new);
    }
    text_bytes(&text)
}

fn render_diff(operations: &[PreparedOperation]) -> String {
    let mut rendered = String::new();
    for operation in operations {
        match operation {
            PreparedOperation::Create { path, after } => {
                rendered.push_str(&format!("--- /dev/null\n+++ {path}\n"));
                render_prefixed(&mut rendered, after, '+');
            }
            PreparedOperation::Update {
                path,
                before,
                after,
            } => {
                rendered.push_str(&format!("--- {path}\n+++ {path}\n"));
                render_changed_lines(&mut rendered, before, after);
            }
            PreparedOperation::Delete { path, before } => {
                rendered.push_str(&format!("--- {path}\n+++ /dev/null\n"));
                render_prefixed(&mut rendered, before, '-');
            }
            PreparedOperation::Rename { from, to, .. } => {
                rendered.push_str(&format!("rename from {from}\nrename to {to}\n"));
            }
        }
    }
    if rendered.len() <= PATCH_DIFF_BYTE_LIMIT {
        return rendered;
    }
    let mut end = PATCH_DIFF_BYTE_LIMIT;
    while !rendered.is_char_boundary(end) {
        end -= 1;
    }
    rendered.truncate(end);
    rendered.push_str("\n... diff display truncated ...\n");
    rendered
}

fn render_prefixed(rendered: &mut String, bytes: &[u8], prefix: char) {
    for line in String::from_utf8_lossy(bytes).lines() {
        rendered.push(prefix);
        rendered.push_str(line);
        rendered.push('\n');
    }
}

/// Lines of unchanged context kept on each side of an update.
const DIFF_CONTEXT_LINES: usize = 3;

/// Renders only the changed region of an update. Emitting both files in full
/// would push the actual change past [`PATCH_DIFF_BYTE_LIMIT`], leaving the
/// user approving a patch whose change is not on screen.
fn render_changed_lines(rendered: &mut String, before: &[u8], after: &[u8]) {
    let before_text = String::from_utf8_lossy(before);
    let after_text = String::from_utf8_lossy(after);
    let before_lines = before_text.lines().collect::<Vec<_>>();
    let after_lines = after_text.lines().collect::<Vec<_>>();

    let common = before_lines.len().min(after_lines.len());
    let prefix = (0..common)
        .take_while(|&index| before_lines[index] == after_lines[index])
        .count();
    let suffix = (0..common - prefix)
        .take_while(|&index| {
            before_lines[before_lines.len() - 1 - index]
                == after_lines[after_lines.len() - 1 - index]
        })
        .count();

    let before_end = before_lines.len() - suffix;
    let after_end = after_lines.len() - suffix;
    let context_start = prefix.saturating_sub(DIFF_CONTEXT_LINES);
    let context_end = (before_end + DIFF_CONTEXT_LINES).min(before_lines.len());

    rendered.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        context_start + 1,
        context_end - context_start,
        context_start + 1,
        (context_end - before_end) + (after_end - context_start),
    ));
    for line in &before_lines[context_start..prefix] {
        rendered.push_str(&format!(" {line}\n"));
    }
    for line in &before_lines[prefix..before_end] {
        rendered.push_str(&format!("-{line}\n"));
    }
    for line in &after_lines[prefix..after_end] {
        rendered.push_str(&format!("+{line}\n"));
    }
    for line in &before_lines[before_end..context_end] {
        rendered.push_str(&format!(" {line}\n"));
    }
}

fn snapshots(operations: &[PreparedOperation]) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut snapshots = BTreeMap::new();
    for operation in operations {
        match operation {
            PreparedOperation::Create { path, .. } => {
                snapshots.insert(path.clone(), None);
            }
            PreparedOperation::Update { path, before, .. }
            | PreparedOperation::Delete { path, before } => {
                snapshots.insert(path.clone(), Some(before.clone()));
            }
            PreparedOperation::Rename { from, to, before } => {
                snapshots.insert(from.clone(), Some(before.clone()));
                snapshots.insert(to.clone(), None);
            }
        }
    }
    snapshots
}

fn created_directories(root: &Path, operations: &[PreparedOperation]) -> Vec<PathBuf> {
    let mut directories = BTreeSet::new();
    for operation in operations {
        let path = match operation {
            PreparedOperation::Create { path, .. } => Some(path),
            PreparedOperation::Rename { to, .. } => Some(to),
            PreparedOperation::Update { .. } | PreparedOperation::Delete { .. } => None,
        };
        let Some(path) = path else {
            continue;
        };
        let mut parent = root.join(path).parent().map(Path::to_path_buf);
        while let Some(directory) = parent {
            if directory == root || directory.exists() {
                break;
            }
            directories.insert(directory.clone());
            parent = directory.parent().map(Path::to_path_buf);
        }
    }
    let mut directories = directories.into_iter().collect::<Vec<_>>();
    directories.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    directories
}

fn remove_created_directories(directories: &[PathBuf]) -> Result<()> {
    for directory in directories {
        match fs::remove_dir(directory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("remove rolled-back directory {}", directory.display())
                });
            }
        }
    }
    Ok(())
}

fn rollback(root: &Path, snapshots: &BTreeMap<String, Option<Vec<u8>>>) -> Result<()> {
    for (path, before) in snapshots.iter().rev() {
        let target = root.join(path);
        if target.exists() {
            let metadata = fs::symlink_metadata(&target)?;
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                bail!("rollback target became unsafe: {}", target.display());
            }
            fs::remove_file(&target)?;
        }
        if let Some(before) = before {
            atomic_replace(&target, before)?;
        }
    }
    Ok(())
}

fn create_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("file path has no parent")?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))
}

fn write_transaction_file(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    create_parent(path)?;
    let parent = path.parent().context("file path has no parent")?;
    let transaction_id = NEXT_TRANSACTION_ID.fetch_add(1, Ordering::Relaxed);
    let temp = parent.join(transaction_file_name(transaction_id));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .with_context(|| format!("create transaction file {}", temp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok::<_, anyhow::Error>(temp.clone())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn atomic_create(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = write_transaction_file(path, bytes)?;
    let result = fs::hard_link(&temp, path)
        .with_context(|| format!("create {} without replacing", path.display()));
    let _ = fs::remove_file(&temp);
    result
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<()> {
    let temp = write_transaction_file(path, bytes)?;
    let result = fs::rename(&temp, path).with_context(|| format!("replace {}", path.display()));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempWorkspace;

    #[test]
    fn replacements_require_exact_occurrence_count() {
        let replacement = Replacement {
            old: "one".to_owned(),
            new: "two".to_owned(),
            expected_occurrences: 1,
        };
        assert_eq!(
            apply_replacements("x", b"one\n", std::slice::from_ref(&replacement)).unwrap(),
            b"two\n"
        );
        assert!(apply_replacements("x", b"one one", &[replacement]).is_err());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn duplicate_transaction_paths_are_rejected() {
        let temp = TempWorkspace::new("patch-duplicate");
        temp.write(".gitignore", ".nuillu/\n");
        temp.write("a.txt", "old\n");
        let workspace = temp.open();
        let hash = sha256_hex(b"old\n");
        let input = PatchInput {
            purpose: "touch one path twice".to_owned(),
            operations: vec![
                PatchOperation::Update {
                    path: "a.txt".to_owned(),
                    preimage_sha256: hash.clone(),
                    replacements: vec![Replacement {
                        old: "old".to_owned(),
                        new: "changed".to_owned(),
                        expected_occurrences: 1,
                    }],
                },
                PatchOperation::Delete {
                    path: "a.txt".to_owned(),
                    preimage_sha256: hash,
                },
            ],
        };
        assert!(prepare_patch(&workspace, &input).await.is_err());
        assert_eq!(temp.read("a.txt"), b"old\n");
    }

    /// A whole-file dump would push the change past `PATCH_DIFF_BYTE_LIMIT`,
    /// so the user would approve a patch they cannot see.
    #[test]
    fn render_diff_shows_only_the_changed_region_of_a_large_file() {
        let mut before = (1..=4000)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>();
        let mut after = before.clone();
        // The only change sits at the very end of the file.
        after[3999] = "line 4000 CHANGED".to_owned();
        before.push(String::new());
        after.push(String::new());

        let diff = render_diff(&[PreparedOperation::Update {
            path: "big.txt".to_owned(),
            before: before.join("\n").into_bytes(),
            after: after.join("\n").into_bytes(),
        }]);

        assert!(diff.starts_with("--- big.txt\n+++ big.txt\n"));
        assert!(
            !diff.contains("... diff display truncated ..."),
            "a one-line change must not overflow the display limit"
        );
        assert!(diff.contains("-line 4000\n"));
        assert!(diff.contains("+line 4000 CHANGED\n"));
        assert!(diff.contains(" line 3997\n"), "context must be shown");
        assert!(
            !diff.contains("line 100\n"),
            "unchanged regions must not be dumped"
        );
    }

    #[test]
    fn render_diff_handles_pure_insertion_and_deletion() {
        let inserted = render_diff(&[PreparedOperation::Update {
            path: "a.txt".to_owned(),
            before: b"one\ntwo\n".to_vec(),
            after: b"one\nmiddle\ntwo\n".to_vec(),
        }]);
        assert!(inserted.contains("+middle\n"));
        assert!(!inserted.contains("-one\n"));

        let removed = render_diff(&[PreparedOperation::Update {
            path: "a.txt".to_owned(),
            before: b"one\nmiddle\ntwo\n".to_vec(),
            after: b"one\ntwo\n".to_vec(),
        }]);
        assert!(removed.contains("-middle\n"));
        assert!(!removed.contains("+one\n"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_stale_preimage_is_rejected() {
        let temp = TempWorkspace::new("patch-preimage");
        temp.write(".gitignore", ".nuillu/\n");
        temp.write("a.txt", "old\n");
        let workspace = temp.open();
        let input = PatchInput {
            purpose: "update from a stale read".to_owned(),
            operations: vec![PatchOperation::Update {
                path: "a.txt".to_owned(),
                preimage_sha256: sha256_hex(b"what the model last read\n"),
                replacements: vec![Replacement {
                    old: "old".to_owned(),
                    new: "new".to_owned(),
                    expected_occurrences: 1,
                }],
            }],
        };
        assert!(prepare_patch(&workspace, &input).await.is_err());
        assert_eq!(temp.read("a.txt"), b"old\n");
        assert!(
            validate_hash("not-a-sha256").is_err(),
            "a malformed hash is rejected before the file is read"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_failed_operation_rolls_the_whole_transaction_back() {
        let temp = TempWorkspace::new("patch-rollback");
        temp.write(".gitignore", ".nuillu/\ngenerated/\n");
        temp.write("a.txt", "old\n");
        let workspace = temp.open();
        // The second operation lands on an ignored path, which fails after the
        // first operation has already been written.
        let input = PatchInput {
            purpose: "exercise rollback".to_owned(),
            operations: vec![
                PatchOperation::Update {
                    path: "a.txt".to_owned(),
                    preimage_sha256: sha256_hex(b"old\n"),
                    replacements: vec![Replacement {
                        old: "old".to_owned(),
                        new: "changed".to_owned(),
                        expected_occurrences: 1,
                    }],
                },
                PatchOperation::Create {
                    path: "generated/ignored.txt".to_owned(),
                    content: "must roll back\n".to_owned(),
                },
            ],
        };
        let patch = prepare_patch(&workspace, &input).await.unwrap();
        assert!(apply_patch(&workspace, &patch).await.is_err());
        assert_eq!(temp.read("a.txt"), b"old\n");
        assert!(
            !temp.root().join("generated").exists(),
            "rollback must also remove the directory it created"
        );
    }
}
