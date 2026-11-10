mod base_snapshot;
mod git_snapshot;
mod overlay;
mod search_view;

pub(crate) use base_snapshot::base_snapshot_config_fingerprint_for;
pub use base_snapshot::{WorkspaceBaseSnapshotIndex, WorkspaceBaseSnapshotMetadata};
pub use git_snapshot::GitSnapshotIndex;
pub use overlay::WorkspaceOverlayIndex;
pub(crate) use overlay::{
    WorkspaceOverlayIngestBatch, WorkspaceOverlayMergeEvent, WorkspaceOverlayPublication,
    WorkspaceOverlayPublishPolicy, WorkspaceOverlayRuntimeStats, WorkspaceOverlaySnapshotState,
};
pub use search_view::{SnapshotHandle, WorkspaceSearchView};
