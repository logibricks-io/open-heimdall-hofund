//! `hofund group copy`: duplicate a named routing group through the control API.

use std::path::Path;

use hofund_control_client::ClientError;
use hofund_control_client::hofund_control_proto::GroupCopyRequest;

use crate::commands::{client, failure};

const COPY_ATTEMPTS: usize = 3;

fn is_conflict(error: &ClientError) -> bool {
    matches!(error, ClientError::Server { status, .. } if status.as_u16() == 409)
}

/// `hofund group copy <id> [name]`.
pub async fn copy(config: &Path, id: &str, name: Option<&str>) -> Result<String, String> {
    let client = client(config).await?;
    let label = name.unwrap_or("").trim();
    let mut attempt = 0;
    loop {
        attempt += 1;
        let current = client.config().await.map_err(failure)?;
        let request = GroupCopyRequest::new(current.disk_revision, label.to_owned());
        match client.copy_group(id, &request).await {
            Err(error) if attempt < COPY_ATTEMPTS && is_conflict(&error) => {}
            Err(error) => return Err(failure(error)),
            Ok(response) => {
                return Ok(format!(
                    "copied to group/{} ({})",
                    response.group.id, response.group.name
                ));
            }
        }
    }
}
