#![allow(clippy::expect_used)]

mod common;

#[path = "e2e/search_index.rs"]
mod search_index;

#[path = "e2e/search_engine.rs"]
mod search_engine;

#[path = "e2e/filtering_api.rs"]
mod filtering_api;

#[path = "e2e/cli_support.rs"]
mod cli_support;

#[path = "e2e/cli_basic.rs"]
mod cli_basic;

#[path = "e2e/cli_context.rs"]
mod cli_context;

#[path = "e2e/cli_json.rs"]
mod cli_json;

#[path = "e2e/cli_regression.rs"]
mod cli_regression;

#[path = "e2e/workspace_index.rs"]
mod workspace_index;
