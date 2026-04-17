mod base_snapshot;
mod git_snapshot;
mod overlay;
mod search_view;

pub use base_snapshot::{WorkspaceBaseSnapshotIndex, WorkspaceBaseSnapshotMetadata};
pub use git_snapshot::GitSnapshotIndex;
pub use overlay::WorkspaceOverlayIndex;
pub(crate) use overlay::{
    WorkspaceOverlayIngestBatch, WorkspaceOverlayMergeEvent, WorkspaceOverlayPublishPolicy,
    WorkspaceOverlayRuntimeStats, WorkspaceOverlaySnapshotState,
    DEFAULT_WORKSPACE_OVERLAY_AUTO_CHECKPOINT_MAX_UNCOMMITTED_OPS,
};
pub use search_view::{SnapshotHandle, WorkspaceSearchView};
