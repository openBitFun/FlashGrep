#[path = "query_runtime/count.rs"]
mod count;
#[path = "query_runtime/literal.rs"]
mod literal;
#[path = "query_runtime/query.rs"]
mod query;
#[path = "query_runtime/shared.rs"]
mod shared;
#[path = "query_runtime/verify.rs"]
mod verify;

pub(crate) use count::{CountKind as CountVerifyKind, VerifyPlan as CountVerifyPlan};
pub(crate) use query::LinePrefilter;
pub(crate) use verify::{
    build_matcher, requires_multiline_verification, verify_candidate_sources_with_resolver,
    VerifyCandidateSource, VerifyPlan as SearchVerifyPlan,
};
