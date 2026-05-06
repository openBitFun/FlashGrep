use std::{fs, path::PathBuf, thread, time::Duration};

use tempfile::tempdir;

use super::*;
use crate::daemon::protocol::{
    EnsureRepoParams, GlobParams, InitializeParams, OpenRepoParams, PathScope, QuerySpec,
    RepoConfig, Request, Response, SearchBackend, SearchModeConfig, TaskState,
};

mod branch_switch;
mod lifecycle;
mod overlay;
mod support;

use support::*;
