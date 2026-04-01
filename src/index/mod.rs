pub mod builder;
pub mod format;
pub mod overlay;
pub mod searcher;

pub use builder::{
    build_index, build_index_with_options, rebuild_index, rebuild_index_with_options,
    IndexBuildOptions,
};
pub use overlay::OverlaySnapshotInfo;
pub use searcher::{IndexSearcher, IndexWorktreeDiff};
