use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncReadExt as _;

pub const SEARCH_MATCH_LIMIT: usize = 20;
pub const FILE_LIMIT: usize = 100;
pub const READ_LINE_LIMIT: usize = 40;
pub const READ_BYTE_LIMIT: usize = 32 * 1024;
pub const SEARCH_FRAGMENT_BYTE_LIMIT: usize = 1024;
const RG_OUTPUT_BYTE_LIMIT: usize = 256 * 1024;
const VISIBILITY_OUTPUT_BYTE_LIMIT: usize = 2 * 1024 * 1024;
const MAX_READ_FILE_BYTES: u64 = 16 * 1024 * 1024;
const RG_TIMEOUT: Duration = Duration::from_secs(10);

/// Nuillu's state directory, which must stay invisible to every coding tool.
pub(crate) const STATE_DIR_NAME: &str = ".nuillu";
/// Affixes reserved for in-progress patch transaction files. A workspace path
/// carrying them is rejected, and ripgrep never reports one.
pub(crate) const TRANSACTION_FILE_PREFIX: &str = ".nuillu-code-";
pub(crate) const TRANSACTION_FILE_SUFFIX: &str = ".tmp";
/// `rg --version` prints its own name first. This is a behavioural sanity probe
/// at startup, not an identity guarantee; identity comes from
/// [`ExecutableIdentity`].
const RG_VERSION_BANNER: &[u8] = b"ripgrep ";

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExecutableIdentity {
    canonical_path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl ExecutableIdentity {
    fn read(path: &Path) -> Result<Self> {
        let canonical_path = fs::canonicalize(path)
            .with_context(|| format!("resolve ripgrep executable {}", path.display()))?;
        let metadata = fs::metadata(&canonical_path)
            .with_context(|| format!("inspect ripgrep executable {}", canonical_path.display()))?;
        if !metadata.is_file() {
            bail!(
                "ripgrep executable is not a regular file: {}",
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
pub struct RgExecutable {
    identity: ExecutableIdentity,
}

impl RgExecutable {
    pub fn discover(root: &Path) -> Result<Self> {
        let path = find_in_path("rg").context("find `rg` in PATH")?;
        let identity = ExecutableIdentity::read(&path)?;
        if identity.canonical_path.starts_with(root) {
            bail!("ripgrep executable must be outside cwd");
        }
        let executable = Self { identity };
        executable.revalidate()?;
        let output = std::process::Command::new(&executable.identity.canonical_path)
            .arg("--version")
            .env_clear()
            .stdin(Stdio::null())
            .output()
            .context("validate ripgrep executable")?;
        if !output.status.success() || !output.stdout.starts_with(RG_VERSION_BANNER) {
            bail!("resolved `rg` did not report a ripgrep version");
        }
        Ok(executable)
    }

    fn revalidate(&self) -> Result<()> {
        let current = ExecutableIdentity::read(&self.identity.canonical_path)?;
        if current != self.identity {
            bail!("ripgrep executable changed after startup");
        }
        Ok(())
    }
}

async fn read_limited(
    reader: impl tokio::io::AsyncRead + Unpin,
    limit: usize,
) -> Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .await
        .context("read child-process output")?;
    let truncated = bytes.len() > limit;
    bytes.truncate(limit);
    Ok((bytes, truncated))
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|directory| directory.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[derive(Clone, Debug)]
pub struct Workspace {
    root: Arc<PathBuf>,
    rg: Arc<RgExecutable>,
}

impl Workspace {
    pub fn open(cwd: &Path) -> Result<Self> {
        let root =
            fs::canonicalize(cwd).with_context(|| format!("resolve cwd {}", cwd.display()))?;
        if !root.is_dir() {
            bail!("cwd is not a directory: {}", root.display());
        }
        let rg = RgExecutable::discover(&root)?;
        Ok(Self {
            root: Arc::new(root),
            rg: Arc::new(rg),
        })
    }

    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    pub fn state_dir(&self) -> PathBuf {
        self.root.join(STATE_DIR_NAME)
    }

    pub async fn verify_state_dir_is_ignored(&self) -> Result<()> {
        let path = self.root.join(".gitignore");
        let content = fs::read_to_string(&path)
            .with_context(|| format!("read required {}", path.display()))?;
        if !content.lines().any(declares_state_dir_ignore) {
            bail!("cwd/.gitignore must contain a root .nuillu ignore rule");
        }
        let mut args = base_rg_args();
        args.push(OsString::from("--files"));
        args.push(OsString::from("--null"));
        let output = self.run_rg(args, VISIBILITY_OUTPUT_BYTE_LIMIT).await?;
        if !output.success || output.truncated {
            bail!("could not verify the complete workspace ignore boundary");
        }
        if split_nul_paths(&output.stdout)?
            .iter()
            .any(|visible| path_contains(STATE_DIR_NAME, visible))
        {
            bail!(".nuillu is re-included by an ignore source");
        }
        Ok(())
    }

    /// Normalises a caller-supplied scope at the boundary: both `None` and the
    /// wire form `"."` mean "the whole workspace", so nothing downstream has to
    /// recognise `"."` as a sentinel.
    fn resolve_scope(&self, path: Option<&str>) -> Result<Option<String>> {
        let Some(path) = path.filter(|path| *path != ".") else {
            return Ok(None);
        };
        validate_relative_path(path)?;
        self.validate_no_symlink_prefix(path, false)?;
        Ok(Some(path.to_owned()))
    }

    pub fn resolve_relative(&self, path: &str) -> Result<PathBuf> {
        validate_relative_path(path)?;
        Ok(self.root.join(path))
    }

    pub fn validate_no_symlink_prefix(&self, path: &str, allow_missing: bool) -> Result<PathBuf> {
        let resolved = self.resolve_relative(path)?;
        let relative = Path::new(path);
        let mut current = self.root.as_path().to_path_buf();
        for component in relative.components() {
            let Component::Normal(segment) = component else {
                bail!("path must contain only normal relative components");
            };
            current.push(segment);
            match fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    bail!("symbolic links are forbidden: {}", current.display());
                }
                Ok(_) => {}
                Err(error) if allow_missing && error.kind() == std::io::ErrorKind::NotFound => {
                    break;
                }
                Err(error) => {
                    return Err(error).with_context(|| format!("inspect {}", current.display()));
                }
            }
        }
        Ok(resolved)
    }

    pub async fn visible_files(
        &self,
        glob: Option<&str>,
        path: Option<&str>,
        limit: usize,
    ) -> Result<FileList> {
        if let Some(glob) = glob {
            validate_glob(glob)?;
        }
        let scope = self.resolve_scope(path)?;
        let mut args = base_rg_args();
        args.push(OsString::from("--files"));
        args.push(OsString::from("--null"));
        let output = self.run_rg(args, RG_OUTPUT_BYTE_LIMIT).await?;
        if !output.success {
            bail!("ripgrep file listing failed: {}", output.stderr_text());
        }
        let file_bytes = if output.truncated {
            complete_delimited_prefix(&output.stdout, b'\0')
        } else {
            output.stdout.as_slice()
        };
        let mut paths = split_nul_paths(file_bytes)?;
        paths.retain(|candidate| {
            scope
                .as_deref()
                .is_none_or(|scope| path_contains(scope, candidate))
                && glob.is_none_or(|pattern| glob_matches(pattern, candidate))
        });
        paths.sort();
        let truncated = output.truncated || paths.len() > limit;
        paths.truncate(limit);
        Ok(FileList { paths, truncated })
    }

    pub async fn is_visible_existing(&self, path: &str) -> Result<bool> {
        self.validate_no_symlink_prefix(path, false)?;
        let mut args = base_rg_args();
        args.push(OsString::from("--files"));
        args.push(OsString::from("--null"));
        let output = self.run_rg(args, VISIBILITY_OUTPUT_BYTE_LIMIT).await?;
        if !output.success {
            bail!("ripgrep visibility check failed: {}", output.stderr_text());
        }
        if output.truncated {
            bail!("workspace file listing exceeded the safety limit");
        }
        Ok(split_nul_paths(&output.stdout)?
            .iter()
            .any(|visible| visible == path))
    }

    pub async fn search(&self, input: &SearchInput) -> Result<SearchOutput> {
        if input.pattern.is_empty() {
            bail!("search pattern must not be empty");
        }
        let scope = self.resolve_scope(input.path.as_deref())?;
        if let Some(glob) = input.glob.as_deref() {
            validate_glob(glob)?;
        }
        let mut args = base_rg_args();
        args.push(OsString::from("--json"));
        args.push(OsString::from("--"));
        args.push(OsString::from(&input.pattern));
        args.push(OsString::from("."));
        let output = self.run_rg(args, RG_OUTPUT_BYTE_LIMIT).await?;
        // ripgrep uses 1 when no matches were found.
        if !output.success && !output.stderr.is_empty() {
            bail!("ripgrep search failed: {}", output.stderr_text());
        }
        let mut matches = Vec::new();
        let mut truncated = output.truncated;
        let json_bytes = if output.truncated {
            complete_delimited_prefix(&output.stdout, b'\n')
        } else {
            output.stdout.as_slice()
        };
        let text = std::str::from_utf8(json_bytes).context("ripgrep JSON is not UTF-8")?;
        for line in text.lines() {
            let value: Value = serde_json::from_str(line).context("decode ripgrep JSON event")?;
            if value.get("type").and_then(Value::as_str) != Some("match") {
                continue;
            }
            let data = value.get("data").context("ripgrep match has no data")?;
            let raw_path = data
                .pointer("/path/text")
                .and_then(Value::as_str)
                .context("ripgrep returned a non-UTF-8 path")?;
            let candidate = raw_path.strip_prefix("./").unwrap_or(raw_path);
            validate_relative_path(candidate)?;
            if scope
                .as_deref()
                .is_some_and(|scope| !path_contains(scope, candidate))
                || input
                    .glob
                    .as_deref()
                    .is_some_and(|glob| !glob_matches(glob, candidate))
            {
                continue;
            }
            if matches.len() == SEARCH_MATCH_LIMIT {
                truncated = true;
                break;
            }
            let line_number = data.get("line_number").and_then(Value::as_u64).unwrap_or(0);
            let column = data
                .get("submatches")
                .and_then(Value::as_array)
                .and_then(|matches| matches.first())
                .and_then(|submatch| submatch.get("start"))
                .and_then(Value::as_u64)
                .map_or(0, |start| start + 1);
            let matched_line = data
                .pointer("/lines/text")
                .and_then(Value::as_str)
                .context("ripgrep returned non-UTF-8 match text")?
                .trim_end_matches(['\r', '\n']);
            matches.push(SearchMatch {
                path: candidate.to_owned(),
                line: line_number,
                column,
                text: truncate_utf8(matched_line, SEARCH_FRAGMENT_BYTE_LIMIT),
            });
        }
        Ok(SearchOutput { matches, truncated })
    }

    pub async fn read(&self, input: &ReadInput) -> Result<ReadOutput> {
        if !self.is_visible_existing(&input.path).await? {
            bail!("path is ignored, outside the workspace, or not a regular visible file");
        }
        let path = self.validate_no_symlink_prefix(&input.path, false)?;
        let metadata =
            fs::metadata(&path).with_context(|| format!("inspect {}", path.display()))?;
        if !metadata.is_file() {
            bail!("read target is not a regular file");
        }
        if metadata.len() > MAX_READ_FILE_BYTES {
            bail!("read target exceeds the 16 MiB safety limit");
        }
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        if bytes.contains(&0) {
            bail!("binary files are not readable");
        }
        let text = std::str::from_utf8(&bytes).context("file is not UTF-8 text")?;
        let start = input.start_line.unwrap_or(1);
        if start == 0 {
            bail!("start_line is one-based");
        }
        let requested_end = input
            .end_line
            .unwrap_or(start.saturating_add(READ_LINE_LIMIT - 1));
        if requested_end < start {
            bail!("end_line must not precede start_line");
        }
        let end = requested_end.min(start.saturating_add(READ_LINE_LIMIT - 1));
        let mut retained = Vec::new();
        let mut retained_bytes = 0usize;
        let mut byte_truncated = false;
        // Line numbers are carried by `start_line`, never baked into the text.
        for line in text.lines().skip(start - 1).take(end - start + 1) {
            if retained_bytes.saturating_add(line.len()) > READ_BYTE_LIMIT {
                byte_truncated = true;
                break;
            }
            retained_bytes += line.len();
            retained.push(line.to_owned());
        }
        Ok(ReadOutput {
            path: input.path.clone(),
            sha256: sha256_hex(&bytes),
            start_line: start,
            lines: retained,
            truncated: byte_truncated || requested_end > end,
        })
    }

    pub async fn read_visible_bytes(&self, path: &str) -> Result<Vec<u8>> {
        if !self.is_visible_existing(path).await? {
            bail!("path is ignored, outside the workspace, or not a regular visible file");
        }
        let resolved = self.validate_no_symlink_prefix(path, false)?;
        let metadata =
            fs::metadata(&resolved).with_context(|| format!("inspect {}", resolved.display()))?;
        if !metadata.is_file() {
            bail!("target is not a regular file");
        }
        if metadata.len() > MAX_READ_FILE_BYTES {
            bail!("target exceeds the 16 MiB safety limit");
        }
        let bytes = fs::read(&resolved).with_context(|| format!("read {}", resolved.display()))?;
        if bytes.contains(&0) || std::str::from_utf8(&bytes).is_err() {
            bail!("binary or non-UTF-8 files are forbidden");
        }
        Ok(bytes)
    }

    pub fn validate_new_path(&self, path: &str) -> Result<PathBuf> {
        let resolved = self.validate_no_symlink_prefix(path, true)?;
        if resolved.exists() {
            bail!("destination already exists: {path}");
        }
        Ok(resolved)
    }

    pub async fn run_rg(&self, args: Vec<OsString>, byte_limit: usize) -> Result<RgOutput> {
        self.rg.run_in(&self.root, args, byte_limit).await
    }
}

impl RgExecutable {
    async fn run_in(
        &self,
        cwd: &Path,
        args: impl IntoIterator<Item = OsString>,
        byte_limit: usize,
    ) -> Result<RgOutput> {
        self.revalidate()?;
        let mut command = tokio::process::Command::new(&self.identity.canonical_path);
        command
            .current_dir(cwd)
            .env_clear()
            .kill_on_drop(true)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args(args);
        let mut child = command.spawn().context("start fixed ripgrep executable")?;
        let stdout = child.stdout.take().context("capture ripgrep stdout")?;
        let stderr = child.stderr.take().context("capture ripgrep stderr")?;
        let collect = async move {
            let (stdout, stderr, status) = tokio::join!(
                read_limited(stdout, byte_limit),
                read_limited(stderr, 16 * 1024),
                child.wait()
            );
            let (stdout, stdout_truncated) = stdout?;
            let (stderr, stderr_truncated) = stderr?;
            Ok::<_, anyhow::Error>(RgOutput {
                success: status.context("wait for ripgrep")?.success(),
                stdout,
                stderr,
                truncated: stdout_truncated || stderr_truncated,
            })
        };
        tokio::time::timeout(RG_TIMEOUT, collect)
            .await
            .context("ripgrep exceeded ten second timeout")?
    }
}

fn base_rg_args() -> Vec<OsString> {
    vec![
        OsString::from("--no-config"),
        OsString::from("--no-require-git"),
        OsString::from("--hidden"),
        OsString::from("--path-separator"),
        OsString::from("/"),
        OsString::from("--glob"),
        OsString::from("!.git/**"),
        OsString::from("--glob"),
        OsString::from(format!(
            "!**/{TRANSACTION_FILE_PREFIX}*{TRANSACTION_FILE_SUFFIX}"
        )),
    ]
}

/// The name of an in-progress transaction file, which no workspace path may use.
pub(crate) fn transaction_file_name(transaction_id: u64) -> String {
    format!("{TRANSACTION_FILE_PREFIX}{transaction_id}{TRANSACTION_FILE_SUFFIX}")
}

fn is_transaction_file_name(segment: &str) -> bool {
    segment.starts_with(TRANSACTION_FILE_PREFIX) && segment.ends_with(TRANSACTION_FILE_SUFFIX)
}

/// Whether one `.gitignore` line ignores the state directory at the root.
fn declares_state_dir_ignore(line: &str) -> bool {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') || line.starts_with('!') {
        return false;
    }
    let line = line.strip_prefix('/').unwrap_or(line);
    let line = line.strip_suffix("/**").unwrap_or(line);
    let line = line.strip_suffix('/').unwrap_or(line);
    line == STATE_DIR_NAME
}

fn validate_relative_path(path: &str) -> Result<()> {
    if path.is_empty() {
        bail!("path must not be empty");
    }
    let mut saw_component = false;
    let mut normalized = Vec::new();
    for component in Path::new(path).components() {
        let Component::Normal(segment) = component else {
            bail!("absolute paths, `.` and `..` are forbidden");
        };
        saw_component = true;
        let segment = segment
            .to_str()
            .context("workspace paths must be valid UTF-8")?;
        if segment == ".git" {
            bail!(".git is always forbidden");
        }
        if is_transaction_file_name(segment) {
            bail!("reserved transaction paths are forbidden");
        }
        normalized.push(segment);
    }
    if !saw_component {
        bail!("path must contain a normal component");
    }
    if normalized.join("/") != path {
        bail!("path must use one canonical `/` separator between components");
    }
    Ok(())
}

fn validate_glob(glob: &str) -> Result<()> {
    if glob.is_empty() || glob.starts_with('!') || glob.starts_with('/') || glob.contains("..") {
        bail!("glob must be a non-negated workspace-relative pattern");
    }
    Ok(())
}

/// Whether `candidate` is `scope` itself or sits below it.
fn path_contains(scope: &str, candidate: &str) -> bool {
    let scope = scope.trim_end_matches('/');
    candidate == scope
        || candidate
            .strip_prefix(scope)
            .is_some_and(|suffix| suffix.starts_with('/'))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GlobToken {
    /// Any sequence, including `/`.
    AnyPath,
    /// Any sequence within one path segment.
    AnySegment,
    /// Exactly one character other than `/`.
    AnyChar,
    Literal(u8),
}

fn tokenize_glob(pattern: &[u8]) -> Vec<GlobToken> {
    let mut tokens = Vec::with_capacity(pattern.len());
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            b'*' if pattern.get(index + 1) == Some(&b'*') => {
                tokens.push(GlobToken::AnyPath);
                index += 2;
            }
            b'*' => {
                tokens.push(GlobToken::AnySegment);
                index += 1;
            }
            b'?' => {
                tokens.push(GlobToken::AnyChar);
                index += 1;
            }
            byte => {
                tokens.push(GlobToken::Literal(byte));
                index += 1;
            }
        }
    }
    tokens
}

/// A glob reaches this from the model, so matching runs in `O(pattern * path)`
/// rather than backtracking. Nothing else bounds it: this runs after ripgrep
/// has returned, so neither [`RG_TIMEOUT`] nor the output limits apply.
fn glob_matches(pattern: &str, candidate: &str) -> bool {
    let candidate = if pattern.contains('/') {
        candidate
    } else {
        candidate.rsplit('/').next().unwrap_or(candidate)
    };
    let tokens = tokenize_glob(pattern.as_bytes());
    let candidate = candidate.as_bytes();
    // `row[j]` is whether the tokens from `index` on match `candidate[j..]`.
    // The empty pattern matches only the empty remainder.
    let mut row = (0..=candidate.len())
        .map(|j| j == candidate.len())
        .collect::<Vec<_>>();
    for token in tokens.iter().rev() {
        let mut next = vec![false; candidate.len() + 1];
        for j in (0..=candidate.len()).rev() {
            let more = j < candidate.len();
            next[j] = match *token {
                GlobToken::AnyPath => row[j] || (more && next[j + 1]),
                GlobToken::AnySegment => row[j] || (more && candidate[j] != b'/' && next[j + 1]),
                GlobToken::AnyChar => more && candidate[j] != b'/' && row[j + 1],
                GlobToken::Literal(byte) => more && candidate[j] == byte && row[j + 1],
            };
        }
        row = next;
    }
    row[0]
}

fn split_nul_paths(bytes: &[u8]) -> Result<Vec<String>> {
    bytes
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            let path = std::str::from_utf8(path).context("ripgrep returned a non-UTF-8 path")?;
            validate_relative_path(path)?;
            Ok(path.to_owned())
        })
        .collect()
}

fn complete_delimited_prefix(bytes: &[u8], delimiter: u8) -> &[u8] {
    bytes
        .iter()
        .rposition(|byte| *byte == delimiter)
        .map_or(&[], |end| &bytes[..=end])
}

pub(crate) fn truncate_utf8(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

#[derive(Debug)]
pub struct RgOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
}

impl RgOutput {
    fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).trim().to_owned()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileList {
    pub paths: Vec<String>,
    pub truncated: bool,
}

type SearchToolOutput = WorkspaceToolOutput<SearchOutput>;

/// Search visible workspace text with a ripgrep regular expression. Returns at most 20 matches and at most 1 KiB per match.
#[lutum::tool_input(name = "search", output = SearchToolOutput)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchInput {
    pub pattern: String,
    #[serde(default)]
    pub glob: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
}

/// One ripgrep match, kept structured rather than flattened into `grep` text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchMatch {
    pub path: String,
    pub line: u64,
    pub column: u64,
    /// The matched line, truncated to [`SEARCH_FRAGMENT_BYTE_LIMIT`].
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SearchOutput {
    pub matches: Vec<SearchMatch>,
    pub truncated: bool,
}

type ReadToolOutput = WorkspaceToolOutput<ReadOutput>;

/// Read at most 40 lines and 32 KiB from one visible UTF-8 text file. Lines are returned verbatim, numbered from `start_line`. Returns a SHA-256 preimage hash required by patch.
#[lutum::tool_input(name = "read", output = ReadToolOutput)]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadInput {
    pub path: String,
    #[serde(default)]
    pub start_line: Option<usize>,
    #[serde(default)]
    pub end_line: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReadOutput {
    pub path: String,
    pub sha256: String,
    /// One-based number of `lines[0]`; later lines follow consecutively.
    pub start_line: usize,
    /// Verbatim file lines, without a line-number prefix.
    pub lines: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct WorkspaceToolOutput<T> {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl<T> WorkspaceToolOutput<T> {
    pub(crate) fn success(result: T) -> Self {
        Self {
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub(crate) fn failure(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            result: None,
            error: Some(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempWorkspace;

    /// A workspace holding one hidden file, one ignored file, one ignored
    /// directory, and one 46-line visible file. Every entry contains `needle`.
    fn ignore_fixture(name: &str) -> TempWorkspace {
        let temp = TempWorkspace::new(name);
        temp.write(".gitignore", ".nuillu/\nignored.txt\nignored-dir/\n");
        temp.write("ignored.txt", "needle\n");
        temp.write("ignored-dir/secret.txt", "needle\n");
        temp.write(".github/workflow.yml", "needle: hidden\n");
        let lines = (1..=45)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        temp.write("visible.txt", &format!("needle\n{lines}\n"));
        temp
    }

    #[test]
    fn path_validation_rejects_escape_and_git() {
        assert!(validate_relative_path("src/lib.rs").is_ok());
        assert!(validate_relative_path("../secret").is_err());
        assert!(validate_relative_path("/etc/passwd").is_err());
        assert!(validate_relative_path(".git/config").is_err());
        assert!(validate_relative_path("src//lib.rs").is_err());
        assert!(validate_relative_path(".nuillu-code-1.tmp").is_err());
    }

    #[test]
    fn glob_matching_respects_segment_boundaries() {
        // A pattern without `/` matches the file name only.
        assert!(glob_matches("*.rs", "src/deep/lib.rs"));
        assert!(!glob_matches("*.rs", "src/lib.rs.bak"));
        // `*` stops at a separator, `**` crosses it.
        assert!(glob_matches("src/*.rs", "src/lib.rs"));
        assert!(!glob_matches("src/*.rs", "src/deep/lib.rs"));
        assert!(glob_matches("src/**/*.rs", "src/deep/lib.rs"));
        assert!(glob_matches("src/**", "src/deep/lib.rs"));
        // `?` is one character, never a separator.
        assert!(glob_matches("src/li?.rs", "src/lib.rs"));
        assert!(!glob_matches("src?lib.rs", "src/lib.rs"));
        assert!(!glob_matches("", "src/lib.rs"));
    }

    /// A backtracking matcher needs exponential time here. Nothing bounds this
    /// call, so it must stay polynomial.
    #[test]
    fn glob_matching_does_not_blow_up_on_repeated_wildcards() {
        let candidate = format!("a/{}", "a".repeat(64));
        let start = std::time::Instant::now();
        assert!(!glob_matches("a/**a**a**a**a**a**a**a**a**b", &candidate));
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn utf8_truncation_preserves_boundaries() {
        assert_eq!(truncate_utf8("aあいう", 4), "aあ…");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn state_ignore_requires_explicit_root_rule() {
        let temp = TempWorkspace::new("state-ignore");
        temp.write(".gitignore", "target/\n");
        let workspace = temp.open();
        assert!(
            workspace.verify_state_dir_is_ignored().await.is_err(),
            "a .gitignore without a root .nuillu rule must be rejected"
        );

        temp.write(".gitignore", ".nuillu/\n");
        assert!(workspace.verify_state_dir_is_ignored().await.is_ok());

        temp.write(".nuillu/model-set.eure", "local");
        temp.write(".ignore", "!.nuillu/\n!.nuillu/**\n");
        assert!(
            workspace.verify_state_dir_is_ignored().await.is_err(),
            "another ignore source must not be able to re-include .nuillu"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn visible_files_lists_hidden_files_but_not_ignored_ones() {
        let temp = ignore_fixture("visible-files");
        let files = temp
            .open()
            .visible_files(None, None, FILE_LIMIT)
            .await
            .unwrap();
        assert!(files.paths.contains(&".github/workflow.yml".to_owned()));
        assert!(files.paths.contains(&"visible.txt".to_owned()));
        assert!(!files.paths.contains(&"ignored.txt".to_owned()));
        assert!(!files.paths.contains(&"ignored-dir/secret.txt".to_owned()));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn visible_files_filters_by_glob_and_path() {
        let temp = ignore_fixture("visible-files-filter");
        let workspace = temp.open();
        assert_eq!(
            workspace
                .visible_files(Some("*.yml"), None, FILE_LIMIT)
                .await
                .unwrap()
                .paths,
            vec![".github/workflow.yml"]
        );
        assert!(
            workspace
                .visible_files(Some("ignored.txt"), None, FILE_LIMIT)
                .await
                .unwrap()
                .paths
                .is_empty(),
            "a glob must not re-expose an ignored file"
        );
        assert!(
            workspace
                .visible_files(None, Some("ignored-dir"), FILE_LIMIT)
                .await
                .unwrap()
                .paths
                .is_empty(),
            "a path scope must not re-expose an ignored directory"
        );
        assert_eq!(
            workspace
                .visible_files(None, Some("."), FILE_LIMIT)
                .await
                .unwrap()
                .paths,
            workspace
                .visible_files(None, None, FILE_LIMIT)
                .await
                .unwrap()
                .paths,
            "an explicit `.` scope means the whole workspace, like no scope"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_reaches_hidden_files_but_skips_ignored_ones() {
        let temp = ignore_fixture("search");
        let search = temp
            .open()
            .search(&SearchInput {
                pattern: "needle".to_owned(),
                glob: None,
                path: None,
            })
            .await
            .unwrap();
        let mut paths = search
            .matches
            .iter()
            .map(|hit| hit.path.as_str())
            .collect::<Vec<_>>();
        paths.sort_unstable();
        assert_eq!(paths, vec![".github/workflow.yml", "visible.txt"]);
        assert!(search.matches.iter().all(|hit| hit.line == 1));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn search_glob_cannot_select_an_ignored_file() {
        let temp = ignore_fixture("search-glob");
        assert!(
            temp.open()
                .search(&SearchInput {
                    pattern: "needle".to_owned(),
                    glob: Some("ignored.txt".to_owned()),
                    path: None,
                })
                .await
                .unwrap()
                .matches
                .is_empty()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_stops_at_the_line_limit_and_reports_truncation() {
        let temp = ignore_fixture("read-limit");
        let read = temp
            .open()
            .read(&ReadInput {
                path: "visible.txt".to_owned(),
                start_line: Some(1),
                end_line: Some(45),
            })
            .await
            .unwrap();
        assert_eq!(read.lines.len(), READ_LINE_LIMIT);
        assert!(read.truncated);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn read_rejects_ignored_files() {
        let temp = ignore_fixture("read-ignored");
        assert!(
            temp.open()
                .read(&ReadInput {
                    path: "ignored.txt".to_owned(),
                    start_line: None,
                    end_line: None,
                })
                .await
                .is_err()
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn read_rejects_a_symlink_to_a_visible_file() {
        let temp = ignore_fixture("read-symlink");
        std::os::unix::fs::symlink(
            temp.root().join("visible.txt"),
            temp.root().join("linked.txt"),
        )
        .unwrap();
        assert!(
            temp.open()
                .read(&ReadInput {
                    path: "linked.txt".to_owned(),
                    start_line: None,
                    end_line: None,
                })
                .await
                .is_err()
        );
    }
}
