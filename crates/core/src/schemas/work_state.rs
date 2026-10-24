//! Spec 6.5: work state (append-only rows, folded on read; last write per key wins).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;
use crate::schemas::common::{Scope, comma_list, opt_from_str};
pub use crate::schemas::recall::WorkStateList;

/// `GET /v1/work-state?scope=kind:key&list=name` (both optional).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkStateQuery {
    #[serde(default, deserialize_with = "opt_from_str::deserialize")]
    pub scope: Option<Scope>,
    #[serde(default, rename = "list")]
    pub list_name: Option<String>,
    #[serde(default, deserialize_with = "comma_list::deserialize")]
    pub space_ids: Option<Vec<Uuid>>,
}

/// `POST /v1/work-state` (memories:write).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkStateWrite {
    /// Default: the caller's current project scope, else global.
    #[serde(default)]
    pub scope: Option<Scope>,
    #[serde(default)]
    pub space_id: Option<Uuid>,
    pub list_name: String,
    pub fields: serde_json::Map<String, serde_json::Value>,
}

impl WorkStateWrite {
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.list_name.is_empty() || self.list_name.chars().count() > 120 {
            return Err(ApiError::invalid("list_name must be 1-120 characters"));
        }
        if self.fields.is_empty() {
            return Err(ApiError::invalid("fields must not be empty"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkStateResponse {
    pub lists: Vec<WorkStateList>,
}
