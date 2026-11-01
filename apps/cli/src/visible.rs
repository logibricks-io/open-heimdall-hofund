use std::path::Path;

use hofund_control_client::hofund_control_proto::{
    AgentHiddenSetRequest, AgentModelsResponse, AgentPickedOnlyRequest, AgentVisibleSetRequest,
};

use crate::commands::{client, failure};

pub(crate) async fn models(
    config: &Path,
    agent: &str,
    hide: &[String],
    unhide: &[String],
) -> Result<String, String> {
    let client = client(config).await?;
    if !hide.is_empty() || !unhide.is_empty() {
        let current = client.agent_models(agent).await.map_err(failure)?;
        let ids = hidden_after(&current.hidden, hide, unhide);
        client
            .set_agent_hidden_models(agent, AgentHiddenSetRequest::new(ids))
            .await
            .map_err(failure)?;
    }
    let response = client.agent_models(agent).await.map_err(failure)?;
    Ok(render_models(&response))
}

pub(crate) async fn visible(
    config: &Path,
    agent: &str,
    names: &[String],
    only_picked: Option<bool>,
) -> Result<String, String> {
    let client = client(config).await?;
    if let Some(only) = only_picked {
        client
            .set_agent_picked_only(agent, AgentPickedOnlyRequest::new(only))
            .await
            .map_err(failure)?;
    } else {
        client
            .set_agent_visible(agent, AgentVisibleSetRequest::new(visibility_names(names)))
            .await
            .map_err(failure)?;
    }
    let response = client.agent_models(agent).await.map_err(failure)?;
    Ok(render_models(&response))
}

fn visibility_names(names: &[String]) -> Vec<String> {
    if names.len() == 1 && names[0].eq_ignore_ascii_case("all") {
        Vec::new()
    } else {
        names.to_vec()
    }
}

fn hidden_after(hidden: &[String], hide: &[String], unhide: &[String]) -> Vec<String> {
    let mut out: Vec<String> = hidden
        .iter()
        .filter(|id| !unhide.iter().any(|drop| drop == *id))
        .cloned()
        .collect();
    for id in hide {
        if !out.iter().any(|seen| seen == id) {
            out.push(id.clone());
        }
    }
    out
}

fn render_models(response: &AgentModelsResponse) -> String {
    let mut out = format!("{}\nshown:", response.agent);
    for id in &response.shown {
        out.push('\n');
        out.push_str(id);
    }
    if !response.hidden.is_empty() {
        out.push_str("\nhidden:");
        for id in &response.hidden {
            out.push('\n');
            out.push_str(id);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{hidden_after, render_models, visibility_names};
    use hofund_control_client::hofund_control_proto::AgentModelsResponse;

    #[test]
    fn g2_12_cli_models_render_lists_shown_and_hidden() {
        let mut response = AgentModelsResponse::default();
        response.agent = "fixture-agent".to_owned();
        response.shown = vec!["a".to_owned()];
        response.hidden = vec!["b".to_owned()];
        let text = render_models(&response);
        assert!(text.contains("shown:\na"));
        assert!(text.contains("hidden:\nb"));
    }

    #[test]
    fn g2_14_visible_all_clears_and_hide_merges() {
        assert!(visibility_names(&["all".to_owned()]).is_empty());
        assert!(visibility_names(&["ALL".to_owned()]).is_empty());
        assert_eq!(
            visibility_names(&["relay".to_owned()]),
            ["relay".to_owned()]
        );
        assert_eq!(
            hidden_after(&["b".to_owned()], &["a".to_owned()], &["b".to_owned()]),
            ["a".to_owned()]
        );
    }
}
