//! Serde types for every request and response of spec 6 (HTTP API) and 7 (MCP tool arguments),
//! packet inputs (9.2) and LLM outputs (10.3, 10.5, 12.2, 12.3), plus the JSON Schema validator
//! for LLM outputs ([`validate`]).
//!
//! Request types carry `validate()` for the field constraints serde cannot express; handlers call
//! it right after extraction.

pub mod admin;
pub mod common;
pub mod events;
pub mod mcp;
pub mod memories;
pub mod outputs;
pub mod packets;
pub mod prompts;
pub mod recall;
pub mod sessions;
pub mod validate;
pub mod work;
pub mod work_state;
