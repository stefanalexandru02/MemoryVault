//! Foundation integration tests (ported from the Python foundation suite). One binary; each
//! `#[sqlx::test]` gets a fresh database with every migration applied.

mod auth;
mod db;
mod http;
mod migrations;
mod rls;
mod schemas;
mod tei;
