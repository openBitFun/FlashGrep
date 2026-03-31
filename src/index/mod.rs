pub mod builder;
pub mod format;
pub mod overlay;
pub mod searcher;

pub use builder::{build_index, rebuild_index};
pub use overlay::OverlaySnapshotInfo;
pub use searcher::{IndexSearcher, IndexWorktreeDiff};
