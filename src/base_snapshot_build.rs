use crate::progress::IndexProgress;

/// Base snapshot build hooks for optional progress reporting and cancellation.
///
/// This keeps the build API stable while allowing callers to opt into richer
/// orchestration behavior.
pub struct BaseSnapshotBuildOptions<'a> {
    pub progress: Option<&'a mut dyn FnMut(IndexProgress)>,
    pub should_cancel: Option<&'a mut dyn FnMut() -> bool>,
    pub build_mode: BaseSnapshotBuildMode,
    pub require_git_head: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseSnapshotBuildMode {
    ReuseExistingSnapshot,
    ForceRebuildSnapshot,
}

impl<'a> Default for BaseSnapshotBuildOptions<'a> {
    fn default() -> Self {
        Self {
            progress: None,
            should_cancel: None,
            build_mode: BaseSnapshotBuildMode::ReuseExistingSnapshot,
            require_git_head: false,
        }
    }
}

impl<'a> BaseSnapshotBuildOptions<'a> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_progress(mut self, progress: &'a mut dyn FnMut(IndexProgress)) -> Self {
        self.progress = Some(progress);
        self
    }

    pub fn with_cancel(mut self, should_cancel: &'a mut dyn FnMut() -> bool) -> Self {
        self.should_cancel = Some(should_cancel);
        self
    }

    pub fn with_build_mode(mut self, build_mode: BaseSnapshotBuildMode) -> Self {
        self.build_mode = build_mode;
        self
    }

    pub fn requiring_git_head(mut self) -> Self {
        self.require_git_head = true;
        self
    }
}
