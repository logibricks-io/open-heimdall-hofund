//! `hofund profile` commands.

use std::path::Path;

use crate::commands::{client, failure};

pub(crate) async fn list(config: &Path) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.profiles().await.map_err(failure)?;
    if response.profiles.is_empty() {
        return Ok("no profiles saved".to_owned());
    }
    let mut lines = Vec::new();
    for profile in response.profiles {
        let marker = if profile.in_use { " (in use)" } else { "" };
        lines.push(format!("{}{marker}", profile.name));
    }
    Ok(lines.join("\n"))
}

pub(crate) async fn show(config: &Path, name: &str) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.profile_detail(name).await.map_err(failure)?;
    let mut lines = vec![response.name.clone()];
    if response.in_use {
        lines.push("(in use)".to_owned());
    }
    for (key, value) in response.fields {
        lines.push(format!("{key} = {value:?}"));
    }
    Ok(lines.join("\n"))
}

pub(crate) async fn save(config: &Path, name: &str) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.save_profile(name).await.map_err(failure)?;
    Ok(format!("saved profile `{}`", response.name))
}

pub(crate) async fn switch(config: &Path, name: &str) -> Result<String, String> {
    let client = client(config).await?;
    let response = client.switch_profile(name).await.map_err(failure)?;
    let mut out = format!(
        "applied profile `{name}` ({} fields changed)",
        response.changed
    );
    for note in response.notes {
        out.push_str(&format!("\nnote: {note}"));
    }
    for warning in response.warnings {
        out.push_str(&format!("\nwarning: {warning}"));
    }
    Ok(out)
}

pub(crate) async fn delete(config: &Path, name: &str) -> Result<String, String> {
    let client = client(config).await?;
    client.delete_profile(name).await.map_err(failure)?;
    Ok(format!("deleted profile `{name}`"))
}
