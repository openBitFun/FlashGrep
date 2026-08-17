use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crate::error::{AppError, Result};

use super::{
    client::DaemonClient,
    git_workspace::resolve_repo_config,
    protocol::{
        EnsureRepoParams, GlobParams, OpenRepoParams, RepoStatus, Request, Response, SearchParams,
    },
    server::read_state_file,
};

const DEFAULT_DAEMON_STATE_FILE: &str = "daemon-state.json";
const DEFAULT_DAEMON_START_LOCK_FILE: &str = "daemon-state.lock";
const MIN_STALE_STARTUP_LOCK_AGE: Duration = Duration::from_secs(30);
/// How long to keep retrying a repo open while the managed daemon comes up.
/// Opening a large base snapshot is synchronous and can take tens of seconds, so
/// this budget has to outlast a slow open rather than assume a fast one.
const DEFAULT_DAEMON_START_TIMEOUT: Duration = Duration::from_secs(120);
const DAEMON_START_TIMEOUT_ENV: &str = "FLASHGREP_DAEMON_START_TIMEOUT_MS";

fn configured_daemon_start_timeout() -> Duration {
    std::env::var(DAEMON_START_TIMEOUT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .map_or(DEFAULT_DAEMON_START_TIMEOUT, Duration::from_millis)
}

#[derive(Debug, Clone)]
pub struct ManagedDaemonClient {
    daemon_program: Option<OsString>,
    start_timeout: Duration,
    retry_interval: Duration,
}

#[derive(Debug, Clone)]
pub struct EnsuredRepo {
    pub addr: String,
    pub repo_id: String,
    pub status: RepoStatus,
    pub indexed_docs: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct OpenedRepo {
    pub addr: String,
    pub repo_id: String,
    pub status: RepoStatus,
}

impl Default for ManagedDaemonClient {
    fn default() -> Self {
        Self {
            daemon_program: None,
            start_timeout: configured_daemon_start_timeout(),
            retry_interval: Duration::from_millis(100),
        }
    }
}

impl ManagedDaemonClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_daemon_program(mut self, program: impl Into<OsString>) -> Self {
        self.daemon_program = Some(program.into());
        self
    }

    pub fn with_start_timeout(mut self, timeout: Duration) -> Self {
        self.start_timeout = timeout;
        self
    }

    pub fn with_retry_interval(mut self, interval: Duration) -> Self {
        self.retry_interval = interval;
        self
    }

    pub fn ensure_repo(&self, params: EnsureRepoParams) -> Result<EnsuredRepo> {
        let params = normalize_ensure_params(params)?;
        let state_file = daemon_state_file_path(&params)?;
        let lock_file = daemon_start_lock_file_path(&state_file);
        if let Ok(repo) = self.try_ensure_repo(&state_file, &params) {
            return Ok(repo);
        }

        let started = Instant::now();
        loop {
            if let Ok(repo) = self.try_ensure_repo(&state_file, &params) {
                return Ok(repo);
            }

            if let Some(_guard) =
                Self::try_acquire_startup_lock(&lock_file, MIN_STALE_STARTUP_LOCK_AGE)?
            {
                if let Ok(repo) = self.try_ensure_repo(&state_file, &params) {
                    return Ok(repo);
                }
                self.spawn_daemon(&state_file)?;
                loop {
                    match self.try_ensure_repo(&state_file, &params) {
                        Ok(repo) => return Ok(repo),
                        Err(error) if started.elapsed() < self.start_timeout => {
                            let _ = error;
                            std::thread::sleep(self.retry_interval);
                        }
                        Err(error) => return Err(error),
                    }
                }
            }

            match self.try_ensure_repo(&state_file, &params) {
                Ok(repo) => return Ok(repo),
                Err(error) if started.elapsed() < self.start_timeout => {
                    let _ = error;
                    std::thread::sleep(self.retry_interval);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn open_repo(&self, params: OpenRepoParams) -> Result<OpenedRepo> {
        let params = normalize_open_params(params)?;
        let state_file = daemon_state_file_path_from_open(&params)?;
        let lock_file = daemon_start_lock_file_path(&state_file);
        if let Ok(repo) = self.try_open_repo(&state_file, &params) {
            return Ok(repo);
        }

        let started = Instant::now();
        loop {
            if let Ok(repo) = self.try_open_repo(&state_file, &params) {
                return Ok(repo);
            }

            if let Some(_guard) =
                Self::try_acquire_startup_lock(&lock_file, MIN_STALE_STARTUP_LOCK_AGE)?
            {
                if let Ok(repo) = self.try_open_repo(&state_file, &params) {
                    return Ok(repo);
                }
                self.spawn_daemon(&state_file)?;
                loop {
                    match self.try_open_repo(&state_file, &params) {
                        Ok(repo) => return Ok(repo),
                        Err(error) if started.elapsed() < self.start_timeout => {
                            let _ = error;
                            std::thread::sleep(self.retry_interval);
                        }
                        Err(error) => return Err(error),
                    }
                }
            }

            match self.try_open_repo(&state_file, &params) {
                Ok(repo) => return Ok(repo),
                Err(error) if started.elapsed() < self.start_timeout => {
                    let _ = error;
                    std::thread::sleep(self.retry_interval);
                }
                Err(error) => return Err(error),
            }
        }
    }

    pub fn search(&self, repo: EnsureRepoParams, mut params: SearchParams) -> Result<Response> {
        let ensured = self.ensure_repo(repo)?;
        params.repo_id = ensured.repo_id;
        let client = DaemonClient::new(ensured.addr);
        client.search(params)
    }

    pub fn glob(&self, repo: EnsureRepoParams, mut params: GlobParams) -> Result<Response> {
        let ensured = self.ensure_repo(repo)?;
        params.repo_id = ensured.repo_id;
        let client = DaemonClient::new(ensured.addr);
        client.glob(params)
    }

    fn try_ensure_repo(&self, state_file: &Path, params: &EnsureRepoParams) -> Result<EnsuredRepo> {
        let state = read_state_file(state_file)?;
        self.ensure_daemon_healthy(&state.addr)?;
        let client = DaemonClient::new(state.addr.clone());
        match client.ensure_repo(params.clone())? {
            Response::RepoEnsured {
                repo_id,
                status,
                indexed_docs,
            } => Ok(EnsuredRepo {
                addr: state.addr,
                repo_id,
                status,
                indexed_docs,
            }),
            other => Err(AppError::Protocol(format!(
                "unexpected ensure_repo response: {other:?}"
            ))),
        }
    }

    fn try_open_repo(&self, state_file: &Path, params: &OpenRepoParams) -> Result<OpenedRepo> {
        let state = read_state_file(state_file)?;
        self.ensure_daemon_healthy(&state.addr)?;
        let client = DaemonClient::new(state.addr.clone());
        match client.open_repo(params.clone())? {
            Response::RepoOpened { repo_id, status } => Ok(OpenedRepo {
                addr: state.addr,
                repo_id,
                status,
            }),
            other => Err(AppError::Protocol(format!(
                "unexpected open_repo response: {other:?}"
            ))),
        }
    }

    fn try_acquire_startup_lock(
        lock_file: &Path,
        stale_after: Duration,
    ) -> Result<Option<StartupLockGuard>> {
        if let Some(parent) = lock_file.parent() {
            fs::create_dir_all(parent)?;
        }

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(lock_file)
        {
            Ok(mut file) => {
                let _ = writeln!(file, "pid={}", std::process::id());
                Ok(Some(StartupLockGuard {
                    path: lock_file.to_path_buf(),
                }))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if startup_lock_is_stale(lock_file, stale_after) {
                    match fs::remove_file(lock_file) {
                        Ok(()) => Self::try_acquire_startup_lock(lock_file, stale_after),
                        Err(remove_error)
                            if remove_error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            Ok(None)
                        }
                        Err(remove_error) => Err(remove_error.into()),
                    }
                } else {
                    Ok(None)
                }
            }
            Err(error) => Err(error.into()),
        }
    }

    fn spawn_daemon(&self, state_file: &Path) -> Result<()> {
        if state_file.exists() {
            fs::remove_file(state_file)?;
        }
        let program = select_daemon_program(
            self.daemon_program.clone(),
            std::env::var_os("FLASHGREP_DAEMON_BIN"),
            std::env::current_exe().ok(),
        );

        let mut command = Command::new(program);
        command
            .arg("serve")
            .arg("--bind")
            .arg("127.0.0.1:0")
            .arg("--state-file")
            .arg(state_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn()?;
        Ok(())
    }

    fn ensure_daemon_healthy(&self, addr: &str) -> Result<()> {
        match DaemonClient::new(addr.to_string())
            .with_io_timeout(self.retry_interval.max(Duration::from_secs(1)))
            .send(Request::Ping)?
        {
            Response::Pong { .. } => Ok(()),
            other => Err(AppError::Protocol(format!(
                "unexpected ping response from daemon: {other:?}"
            ))),
        }
    }
}

fn select_daemon_program(
    explicit_program: Option<OsString>,
    env_program: Option<OsString>,
    current_exe: Option<PathBuf>,
) -> OsString {
    explicit_program
        .or(env_program)
        .or_else(|| current_exe.map(PathBuf::into_os_string))
        .unwrap_or_else(|| OsString::from("flashgrep"))
}

struct StartupLockGuard {
    path: PathBuf,
}

impl Drop for StartupLockGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

pub fn daemon_state_file_path(params: &EnsureRepoParams) -> Result<PathBuf> {
    let params = normalize_ensure_params(params.clone())?;
    let storage_root = params.storage_root.ok_or_else(|| {
        AppError::Protocol("normalized ensure params are missing a storage root".into())
    })?;
    Ok(storage_root.join(DEFAULT_DAEMON_STATE_FILE))
}

pub fn daemon_state_file_path_from_open(params: &OpenRepoParams) -> Result<PathBuf> {
    let params = normalize_open_params(params.clone())?;
    let storage_root = params.storage_root.ok_or_else(|| {
        AppError::Protocol("normalized open params are missing a storage root".into())
    })?;
    Ok(storage_root.join(DEFAULT_DAEMON_STATE_FILE))
}

fn daemon_start_lock_file_path(state_file: &Path) -> PathBuf {
    state_file.parent().map_or_else(
        || PathBuf::from(DEFAULT_DAEMON_START_LOCK_FILE),
        |parent| parent.join(DEFAULT_DAEMON_START_LOCK_FILE),
    )
}

fn startup_lock_is_stale(path: &Path, stale_after: Duration) -> bool {
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age >= stale_after)
}

fn normalize_ensure_params(params: EnsureRepoParams) -> Result<EnsureRepoParams> {
    let resolved =
        resolve_repo_config(params.repo_path, params.storage_root, params.config.clone())?;
    Ok(EnsureRepoParams {
        repo_path: resolved.build_config.repo_path,
        storage_root: Some(resolved.build_config.storage_root),
        config: params.config,
        refresh: params.refresh,
    })
}

fn normalize_open_params(params: OpenRepoParams) -> Result<OpenRepoParams> {
    let resolved =
        resolve_repo_config(params.repo_path, params.storage_root, params.config.clone())?;
    Ok(OpenRepoParams {
        repo_path: resolved.build_config.repo_path,
        storage_root: Some(resolved.build_config.storage_root),
        config: params.config,
        refresh: params.refresh,
    })
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::select_daemon_program;

    #[test]
    fn select_daemon_program_prefers_explicit_program() {
        let selected = select_daemon_program(
            Some(OsString::from("/tmp/explicit-flashgrep")),
            Some(OsString::from("/tmp/env-flashgrep")),
            Some(PathBuf::from("/tmp/current-flashgrep")),
        );
        assert_eq!(selected, OsString::from("/tmp/explicit-flashgrep"));
    }

    #[test]
    fn select_daemon_program_falls_back_to_env_program() {
        let selected = select_daemon_program(
            None,
            Some(OsString::from("/tmp/env-flashgrep")),
            Some(PathBuf::from("/tmp/current-flashgrep")),
        );
        assert_eq!(selected, OsString::from("/tmp/env-flashgrep"));
    }

    #[test]
    fn select_daemon_program_falls_back_to_current_exe() {
        let selected =
            select_daemon_program(None, None, Some(PathBuf::from("/tmp/current-flashgrep")));
        assert_eq!(selected, OsString::from("/tmp/current-flashgrep"));
    }
}
