use crate::index_engine::{manifest::ManifestSnapshot, query::QueryRequest};

pub struct QuerySession {
    pub snapshot: ManifestSnapshot,
    pub query: QueryRequest,
}
