use crate::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const DEFAULT_GROUP_ID: Uuid = Uuid::from_u128(1);

pub fn default_group_id() -> Uuid {
    DEFAULT_GROUP_ID
}

pub fn default_group_ids() -> Vec<Uuid> {
    vec![DEFAULT_GROUP_ID]
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    pub id: Uuid,
    pub name: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Group {
    pub fn validate_name(name: &str) -> Result<()> {
        if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            return Err(Error::invalid(
                "Group name must contain 1 to 128 bytes without control characters",
            ));
        }
        Ok(())
    }
}
