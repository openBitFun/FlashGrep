use super::RepoRuntime;
use crate::daemon::protocol::WorkspaceProgress;

impl RepoRuntime {
    pub(super) fn workspace_progress_snapshot(&self) -> WorkspaceProgress {
        *self
            .workspace_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn observe_workspace_revision(&self) -> u64 {
        let mut progress = self
            .workspace_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        progress.observed_revision = progress.observed_revision.saturating_add(1);
        progress.observed_revision
    }

    pub(super) fn record_workspace_reconciled(&self, revision: u64) {
        let mut progress = self
            .workspace_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        progress.reconciled_revision = progress.reconciled_revision.max(revision);
    }

    pub(super) fn record_workspace_published(&self, revision: u64) {
        let mut progress = self
            .workspace_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        progress.published_revision = progress.published_revision.max(revision);
    }
}
