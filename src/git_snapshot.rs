use std::{
    collections::HashSet,
    io::{BufRead, BufReader, BufWriter, Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio},
    sync::Mutex,
    thread,
    time::Duration,
};

#[cfg(test)]
use std::fs;

use crc32fast::Hasher;

use crate::{
    config::BuildConfig,
    error::{AppError, Result},
};

const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const SEARCH_INDEX_FINGERPRINT_VERSION: &str = "index-engine-postings-v2";

pub(crate) struct GitBlobReader {
    repo_root: std::path::PathBuf,
    state: Mutex<GitBlobReaderState>,
}

struct GitBlobReaderState {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl std::fmt::Debug for GitBlobReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitBlobReader")
            .field("repo_root", &self.repo_root)
            .finish_non_exhaustive()
    }
}

pub(crate) fn resolve_git_head(
    repo_root: &Path,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Option<String>> {
    let output = run_git_command(repo_root, ["rev-parse", "--verify", "HEAD"], should_cancel)?;
    if !output.status.success() {
        return Ok(None);
    }

    let head_commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if head_commit.is_empty() {
        return Ok(None);
    }
    Ok(Some(head_commit))
}

pub(crate) struct GitHeadPathEntry {
    pub mode: String,
    pub logical_path: String,
    pub object_id: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitCommitDiff {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

pub(crate) fn commit_diff_paths(
    repo_root: &Path,
    from_commit: &str,
    to_commit: &str,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<GitCommitDiff> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo_root).args([
        "diff",
        "--name-status",
        "-z",
        "--no-renames",
        from_commit,
        to_commit,
        "--",
        ".",
    ]);
    let output = run_command_with_cancellation(&mut command, should_cancel)?;
    if !output.status.success() {
        return Err(AppError::InvalidIndex(format!(
            "failed to diff git commits {from_commit}..{to_commit} for {}",
            repo_root.display()
        )));
    }

    let mut fields = output.stdout.split(|byte| *byte == 0);
    let mut diff = GitCommitDiff {
        added: Vec::new(),
        modified: Vec::new(),
        deleted: Vec::new(),
    };
    while let Some(status) = fields.next().filter(|field| !field.is_empty()) {
        let path = fields
            .next()
            .filter(|field| !field.is_empty())
            .ok_or_else(|| {
                AppError::InvalidIndex(format!(
                    "git diff returned a status without a path for {}",
                    repo_root.display()
                ))
            })?;
        let path = String::from_utf8_lossy(path).into_owned();
        match status.first().copied() {
            Some(b'A') => diff.added.push(path),
            Some(b'D') => diff.deleted.push(path),
            Some(b'M' | b'T') => diff.modified.push(path),
            Some(other) => {
                return Err(AppError::InvalidIndex(format!(
                    "unsupported git diff status {} for {path}",
                    char::from(other)
                )));
            }
            None => {}
        }
    }
    Ok(diff)
}

pub(crate) fn head_tracked_entries(
    repo_root: &Path,
    head_commit: &str,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Vec<GitHeadPathEntry>> {
    let output = run_git_command(
        repo_root,
        ["ls-tree", "-r", "-l", "-z", head_commit, "--", "."],
        should_cancel,
    )?;
    if !output.status.success() {
        return Err(AppError::InvalidIndex(format!(
            "failed to enumerate git tree entries for {}",
            repo_root.display()
        )));
    }

    let mut entries = Vec::new();
    for record in output
        .stdout
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab_index) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let meta = &record[..tab_index];
        let path = &record[tab_index + 1..];
        let meta = String::from_utf8_lossy(meta);
        let mut fields = meta.split_whitespace();
        let mode = fields.next();
        let object_type = fields.next();
        let object_id = fields.next();
        let size = fields.next();
        if object_type != Some("blob") {
            continue;
        }
        let Some(object_id) = object_id.filter(|value| !value.is_empty()) else {
            continue;
        };
        let Some(size) = size.and_then(|value| value.parse::<u64>().ok()) else {
            continue;
        };
        entries.push(GitHeadPathEntry {
            mode: mode.unwrap_or_default().to_string(),
            logical_path: String::from_utf8_lossy(path).into_owned(),
            object_id: object_id.to_string(),
            size,
        });
    }
    Ok(entries)
}

pub(crate) fn dirty_tracked_paths(
    repo_root: &Path,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<HashSet<String>> {
    let output = run_git_command(
        repo_root,
        [
            "status",
            "--porcelain=v1",
            "--untracked-files=no",
            "--",
            ".",
        ],
        should_cancel,
    )?;
    if !output.status.success() {
        return Err(AppError::InvalidIndex(format!(
            "failed to inspect tracked git worktree state for {}",
            repo_root.display()
        )));
    }

    let mut paths = HashSet::new();
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let Some(path) = line.get(3..) else {
            continue;
        };
        let logical_path = path.split(" -> ").next().unwrap_or(path).to_string();
        paths.insert(logical_path);
    }
    Ok(paths)
}

pub(crate) fn read_git_blob(
    repo_root: &Path,
    head_commit: &str,
    logical_path: &str,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Vec<u8>> {
    let spec = format!("{head_commit}:{logical_path}");
    let output = run_git_command(repo_root, ["cat-file", "blob", &spec], should_cancel)?;
    if !output.status.success() {
        return Err(AppError::InvalidIndex(format!(
            "failed to read git blob {spec} from {}",
            repo_root.display()
        )));
    }
    Ok(output.stdout)
}

impl GitBlobReader {
    pub(crate) fn new(repo_root: &Path) -> Result<Self> {
        Ok(Self {
            repo_root: repo_root.to_path_buf(),
            state: Mutex::new(spawn_git_blob_reader_state(repo_root)?),
        })
    }

    pub(crate) fn read_blob(&self, head_commit: &str, logical_path: &str) -> Result<Vec<u8>> {
        let spec = format!("{head_commit}:{logical_path}");
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.stdin.write_all(spec.as_bytes())?;
        state.stdin.write_all(b"\n")?;
        state.stdin.flush()?;

        let mut header = String::new();
        let bytes_read = state.stdout.read_line(&mut header)?;
        if bytes_read == 0 {
            return Err(AppError::InvalidIndex(format!(
                "git cat-file batch closed unexpectedly for {}",
                self.repo_root.display()
            )));
        }
        let header = header.trim_end_matches('\n');
        if header.ends_with(" missing") {
            return Ok(Vec::new());
        }
        let mut fields = header.split_whitespace();
        let _object_id = fields.next();
        let object_type = fields.next();
        let size = fields.next();
        if object_type != Some("blob") {
            return Err(AppError::InvalidIndex(format!(
                "unexpected git cat-file response for {spec}: {header}"
            )));
        }
        let size = size
            .and_then(|value| value.parse::<usize>().ok())
            .ok_or_else(|| {
                AppError::InvalidIndex(format!(
                    "failed to parse git cat-file size for {spec}: {header}"
                ))
            })?;
        let mut bytes = vec![0u8; size];
        state.stdout.read_exact(&mut bytes)?;
        let mut trailing = [0u8; 1];
        state.stdout.read_exact(&mut trailing)?;
        if trailing[0] != b'\n' {
            return Err(AppError::InvalidIndex(format!(
                "git cat-file batch framing error for {spec}"
            )));
        }
        Ok(bytes)
    }
}

impl Drop for GitBlobReader {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            let _ = state.stdin.flush();
            let _ = state.child.kill();
            let _ = state.child.wait();
        }
    }
}

pub(crate) fn build_config_fingerprint(config: &BuildConfig) -> String {
    let mut hasher = Hasher::new();
    hasher.update(SEARCH_INDEX_FINGERPRINT_VERSION.as_bytes());
    hasher.update(config.tokenizer.as_str().as_bytes());
    hasher.update(config.corpus_mode.as_str().as_bytes());
    hasher.update(&[u8::from(config.include_hidden)]);
    hasher.update(&config.max_file_size.to_le_bytes());
    hasher.update(&(config.min_sparse_len as u64).to_le_bytes());
    hasher.update(&(config.max_sparse_len as u64).to_le_bytes());
    format!("{:08x}", hasher.finalize())
}

pub(crate) fn canonical_repo_root(repo_root: &Path) -> String {
    std::fs::canonicalize(repo_root)
        .unwrap_or_else(|_| repo_root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn run_git_command<const N: usize>(
    repo_root: &Path,
    args: [&str; N],
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Output> {
    let mut command = Command::new("git");
    command.arg("-C").arg(repo_root).args(args);
    run_command_with_cancellation(&mut command, should_cancel)
}

fn spawn_git_blob_reader_state(repo_root: &Path) -> Result<GitBlobReaderState> {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(repo_root)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| AppError::InvalidIndex("failed to capture git blob reader stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::InvalidIndex("failed to capture git blob reader stdout".into()))?;
    Ok(GitBlobReaderState {
        child,
        stdin: BufWriter::new(stdin),
        stdout: BufReader::new(stdout),
    })
}

fn run_command_with_cancellation(
    command: &mut Command,
    should_cancel: &mut impl FnMut() -> bool,
) -> Result<Output> {
    check_cancel(should_cancel)?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| AppError::InvalidIndex("failed to capture command stdout".into()))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| AppError::InvalidIndex("failed to capture command stderr".into()))?;
    let stdout_reader = thread::spawn(move || read_command_pipe(stdout));
    let stderr_reader = thread::spawn(move || read_command_pipe(stderr));
    loop {
        if should_cancel() {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            let _ = stderr_reader.join();
            return Err(AppError::Cancelled);
        }
        if let Some(status) = child.try_wait()? {
            let stdout = join_pipe_reader(stdout_reader, "stdout")?;
            let stderr = join_pipe_reader(stderr_reader, "stderr")?;
            return Ok(Output {
                status,
                stdout,
                stderr,
            });
        }
        thread::sleep(CANCEL_POLL_INTERVAL);
    }
}

fn check_cancel(should_cancel: &mut impl FnMut() -> bool) -> Result<()> {
    if should_cancel() {
        return Err(AppError::Cancelled);
    }
    Ok(())
}

fn read_command_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn join_pipe_reader(
    reader: thread::JoinHandle<std::io::Result<Vec<u8>>>,
    stream_name: &str,
) -> Result<Vec<u8>> {
    reader
        .join()
        .map_err(|_| {
            AppError::InvalidIndex(format!("failed to join command {stream_name} reader"))
        })?
        .map_err(AppError::from)
}

#[cfg(test)]
mod commit_diff_tests {
    use super::*;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .status()
            .expect("git should start");
        assert!(status.success(), "git command failed: {args:?}");
    }

    fn head(repo: &Path) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git should start");
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .expect("head should be utf8")
            .trim()
            .to_string()
    }

    #[test]
    #[cfg(unix)]
    fn commit_diff_classifies_add_modify_delete_type_and_rename_without_rename_detection() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().expect("tempdir should succeed");
        let repo = temp.path();
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.email", "flashgrep@example.invalid"]);
        git(repo, &["config", "user.name", "Flashgrep Test"]);
        for path in ["modify.txt", "delete.txt", "rename-old.txt", "type.txt"] {
            fs::write(repo.join(path), format!("initial {path}\n")).expect("write should succeed");
        }
        git(repo, &["add", "."]);
        git(repo, &["commit", "-q", "-m", "base"]);
        let from = head(repo);

        fs::write(repo.join("modify.txt"), "modified\n").expect("rewrite should succeed");
        fs::remove_file(repo.join("delete.txt")).expect("delete should succeed");
        fs::write(repo.join("added.txt"), "added\n").expect("add should succeed");
        fs::rename(repo.join("rename-old.txt"), repo.join("rename-new.txt"))
            .expect("rename should succeed");
        fs::remove_file(repo.join("type.txt")).expect("type source delete should succeed");
        symlink("modify.txt", repo.join("type.txt")).expect("symlink should succeed");
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "changes"]);
        let to = head(repo);

        let diff =
            commit_diff_paths(repo, &from, &to, &mut || false).expect("commit diff should succeed");
        assert_eq!(
            diff.added.into_iter().collect::<HashSet<_>>(),
            HashSet::from(["added.txt".into(), "rename-new.txt".into()])
        );
        assert_eq!(
            diff.modified.into_iter().collect::<HashSet<_>>(),
            HashSet::from(["modify.txt".into(), "type.txt".into()])
        );
        assert_eq!(
            diff.deleted.into_iter().collect::<HashSet<_>>(),
            HashSet::from(["delete.txt".into(), "rename-old.txt".into()])
        );
    }
}
