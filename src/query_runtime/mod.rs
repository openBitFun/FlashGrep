mod count;
mod literal;
mod query;
mod shared;
mod verify;

pub(crate) use count::{CountKind as CountVerifyKind, VerifyPlan as CountVerifyPlan};
pub(crate) use query::LinePrefilter;
pub(crate) use verify::{
    build_matcher, requires_multiline_verification, verify_candidate_sources_with_resolver,
    VerifyCandidateSource, VerifyPlan as SearchVerifyPlan,
};
