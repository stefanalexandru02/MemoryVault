//! Integration tests: the real `mem-hook` binary against a fake HTTP server and a fake `claude`.

mod connect_clients;
mod doctor;
mod env;
mod events;
mod fake_server;
mod generic;
mod ingest;
mod observe;
mod session_start;
mod spool;
