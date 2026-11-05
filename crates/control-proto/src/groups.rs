use serde::{Deserialize, Serialize};

use crate::{
    API_VERSION, ControlError, ControlErrorKind, FieldIssue, MAX_ID_BYTES, MAX_IDS,
    validate_revision,
};

/// One routing group member on the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupMember {
    /// Member route ref (`provider/model`, `group/id`, or subscription form).
    #[serde(rename = "ref")]
    pub reference: String,
    /// Fixed reasoning effort, when set.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub effort: String,
    /// Whether the member is sent in fast mode.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub fast: bool,
    /// Whether the member takes requests.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// One routing rule line stored in config.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupRule {
    /// Original rule line the daemon parses.
    pub line: String,
}

/// One routing group.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Group {
    /// Group id (`group/<id>` without the prefix).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Member-ordering strategy.
    pub routing: String,
    /// Conversation affinity.
    pub stays: String,
    /// Optional visibility family.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    /// Declared context size in tokens, when set.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<i64>,
    /// Offered reasoning levels, when set.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub levels: Vec<String>,
    /// Classifier route ref, when set.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub classifier: String,
    /// Ordered members.
    pub members: Vec<GroupMember>,
    /// Parsed routing rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<GroupRule>,
    /// Hidden found group the user removed.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Derived auto group not yet stored in config.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub found: bool,
}

/// Group list response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupListResponse {
    /// Body wire version.
    pub version: u32,
    /// SHA-256 disk-content revision.
    pub disk_revision: String,
    /// Configured and found groups.
    pub groups: Vec<Group>,
}

impl Default for GroupListResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            disk_revision: String::new(),
            groups: Vec::new(),
        }
    }
}

/// Creates a routing group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct GroupCreateRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 host-state content revision.
    pub expected_revision: String,
    /// Display name and route id source.
    pub name: String,
    /// First member ref.
    pub first_member: String,
}

impl GroupCreateRequest {
    /// Validates the create request.
    pub fn validate(&self) -> Result<(), ControlError> {
        validate_revision("expected_revision", &self.expected_revision)?;
        if self.name.trim().is_empty() {
            return Err(group_field_error(
                "name",
                "empty",
                "a group name is required",
            ));
        }
        validate_ref("first_member", &self.first_member)?;
        Ok(())
    }
}

/// Patches one routing group.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct GroupPatchRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 host-state content revision.
    pub expected_revision: String,
    /// New member-ordering strategy, when present.
    #[serde(default)]
    pub routing: Option<String>,
    /// New conversation affinity, when present.
    #[serde(default)]
    pub stays: Option<String>,
    /// Family tag update; outer `None` leaves unchanged.
    #[serde(default)]
    pub family: Option<Option<String>>,
    /// Context token limit update; outer `None` leaves unchanged.
    #[serde(default)]
    pub context: Option<Option<i64>>,
    /// Reasoning level list replacement, when present.
    #[serde(default)]
    pub levels: Option<Vec<String>>,
    /// Classifier route ref replacement, when present.
    #[serde(default)]
    pub classifier: Option<String>,
    /// Full member list replacement, when present.
    #[serde(default)]
    pub members: Option<Vec<GroupMemberWire>>,
    /// Rule line list replacement, when present.
    #[serde(default)]
    pub rules: Option<Vec<String>>,
    /// New group route id, when renaming.
    #[serde(default)]
    pub rename_to: Option<String>,
}

/// Member entry on a group patch body.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupMemberWire {
    /// Member route ref.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Fixed reasoning effort, when set.
    #[serde(default)]
    pub effort: String,
    /// Whether the member is sent in fast mode.
    #[serde(default)]
    pub fast: bool,
    /// Whether the member takes requests.
    #[serde(default)]
    pub enabled: bool,
}

impl GroupPatchRequest {
    /// Validates the patch request.
    pub fn validate(&self) -> Result<(), ControlError> {
        validate_revision("expected_revision", &self.expected_revision)?;
        if let Some(members) = &self.members {
            if members.is_empty() {
                return Err(group_field_error(
                    "members",
                    "empty",
                    "a group must keep at least one member",
                ));
            }
            for member in members {
                validate_ref("members", &member.reference)?;
            }
        }
        if let Some(rules) = &self.rules {
            validate_id_count("rules", rules.len())?;
        }
        if let Some(rename) = &self.rename_to {
            validate_group_id("rename_to", rename)?;
        }
        Ok(())
    }
}

/// Deletes one routing group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct GroupDeleteRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 host-state content revision.
    pub expected_revision: String,
}

impl GroupDeleteRequest {
    /// Validates the delete request.
    pub fn validate(&self) -> Result<(), ControlError> {
        validate_revision("expected_revision", &self.expected_revision)
    }
}

/// Restores a hidden found group.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
#[serde(deny_unknown_fields)]
pub struct GroupRestoreRequest {
    /// Body wire version.
    pub version: u32,
    /// Expected SHA-256 host-state content revision.
    pub expected_revision: String,
}

impl GroupRestoreRequest {
    /// Validates the restore request.
    pub fn validate(&self) -> Result<(), ControlError> {
        validate_revision("expected_revision", &self.expected_revision)
    }
}

/// Group write response envelope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupWriteResponse {
    /// Body wire version.
    pub version: u32,
    /// Updated group snapshot.
    pub group: Group,
    /// SHA-256 disk-content revision after the write.
    pub disk_revision: String,
    /// Runtime revision after the write.
    pub runtime_revision: String,
    /// Restart-only fields written to disk but not yet active.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_restart: Vec<String>,
}

/// Group delete response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupDeleteResponse {
    /// Body wire version.
    pub version: u32,
    /// SHA-256 disk-content revision after the write.
    pub disk_revision: String,
    /// Removed group route id.
    pub removed: String,
}

impl Default for GroupDeleteResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            disk_revision: String::new(),
            removed: String::new(),
        }
    }
}

/// One routing member candidate row.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupMemberCandidate {
    /// Served route ref offered to the picker.
    #[serde(rename = "ref")]
    pub reference: String,
    /// Whether the ref is a decision route rather than a chat model.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub decision: bool,
}

/// Member and decider picker lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct GroupMemberCandidatesResponse {
    /// Body wire version.
    pub version: u32,
    /// Chat and group refs currently served.
    pub served: Vec<GroupMemberCandidate>,
    /// Decision refs offered for classifier groups.
    pub deciders: Vec<GroupMemberCandidate>,
}

impl Default for GroupWriteResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            group: Group::default(),
            disk_revision: String::new(),
            runtime_revision: String::new(),
            pending_restart: Vec::new(),
        }
    }
}

impl Default for GroupMemberCandidatesResponse {
    fn default() -> Self {
        Self {
            version: API_VERSION,
            served: Vec::new(),
            deciders: Vec::new(),
        }
    }
}

fn group_field_error(field: &str, reason: &str, message: &str) -> ControlError {
    let mut error = ControlError::new(ControlErrorKind::Unprocessable, message, "validate");
    error.details.push(FieldIssue::new(field, reason, message));
    error
}

fn validate_ref(field: &str, reference: &str) -> Result<(), ControlError> {
    if reference.trim().is_empty() || reference.len() > MAX_ID_BYTES {
        return Err(group_field_error(
            field,
            "invalid",
            "a member reference is missing or too long",
        ));
    }
    Ok(())
}

fn validate_group_id(field: &str, id: &str) -> Result<(), ControlError> {
    let trimmed = id.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_ID_BYTES {
        return Err(group_field_error(
            field,
            "invalid",
            "a group id is missing or too long",
        ));
    }
    Ok(())
}

fn validate_id_count(field: &str, count: usize) -> Result<(), ControlError> {
    if count > MAX_IDS {
        return Err(group_field_error(
            field,
            "too_many",
            "the list exceeds the 4096-entry limit",
        ));
    }
    Ok(())
}
