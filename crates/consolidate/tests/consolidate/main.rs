//! memvault-consolidate integration tests. One binary; each `#[sqlx::test]` gets a fresh database
//! with every migration applied. Appliers run in a `system_tx` through the registry (as the work
//! result route does); the admin routes run through the real router so scopes and RLS apply.

mod admin;
mod appliers;
mod cli;
mod common;
mod doctor;
mod e2e;
mod feedback;
mod planner;
mod revert;
mod triggers;
