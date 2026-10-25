//! Integration tests: the real `mem-hook` binary against a fake HTTP server and a fake `claude`.

mod doctor;
mod env;
mod events;
mod fake_server;
mod observe;
mod session_start;
mod spool;
