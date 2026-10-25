//! memvault-ingest integration tests. One binary; each `#[sqlx::test]` gets a fresh database with
//! every migration applied. Requests go through the real router (`/v1/...`), so RLS applies.

mod common;
mod events;
mod results;
mod rls;
mod sessions;
mod sweep;
mod triggers;
mod work;
