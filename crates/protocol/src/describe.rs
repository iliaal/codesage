use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{TargetKind, TargetResolution};

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum DescribeDetail {
    #[default]
    Compact,
    Standard,
    Full,
}

impl DescribeDetail {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Compact => "compact",
            Self::Standard => "standard",
            Self::Full => "full",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "compact" => Some(Self::Compact),
            "standard" => Some(Self::Standard),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DescribeExpansion {
    pub tool: String,
    pub arguments: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DescribeIncomplete {
    pub kind: String,
    pub reason: String,
    pub recover: DescribeExpansion,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DescribeSection {
    pub data: Value,
    pub expand: DescribeExpansion,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completeness: Option<DescribeIncomplete>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DescribeCard {
    pub handle: String,
    pub kind: TargetKind,
    pub sections: BTreeMap<String, DescribeSection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DescribeCost {
    pub ms: u64,
    pub bytes: usize,
    pub detail: DescribeDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DescribeResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<DescribeCard>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(description = "Resolution candidates when the requested entity is ambiguous.")]
    pub target: Option<TargetResolution>,
    #[schemars(
        description = "Wall time, serialized response size, and detail level for this describe call."
    )]
    pub cost: DescribeCost,
}
