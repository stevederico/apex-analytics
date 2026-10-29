//! apex-analytics library crate — exposes the app modules so both the binary
//! (`main.rs`) and the integration tests under `tests/` can build the router and
//! drive it via `tower::ServiceExt::oneshot`.

pub mod app;
pub mod backup;
pub mod db;
pub mod feedback;
pub mod feedback_legacy;
pub mod github;
pub mod grpc;
pub mod jwt;
pub mod middleware;
pub mod models;
pub mod reports;
pub mod routes;
pub mod validate;
pub mod variant;
