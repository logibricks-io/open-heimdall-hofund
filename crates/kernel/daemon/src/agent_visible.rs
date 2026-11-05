//! Keeps the shared visibility catalog aligned with the published router.

use std::sync::{Arc, RwLock};

use hofund_config::VisibleCatalog;
use hofund_router::Snapshot;

pub(crate) fn rebuild_catalog(
    catalog: &RwLock<VisibleCatalog>,
    config: Arc<hofund_config::Config>,
    snapshot: &Snapshot,
) {
    let route_ids = snapshot.load().models();
    *catalog
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) =
        VisibleCatalog::build(config, &route_ids);
}

pub(crate) fn models_for_agent(
    catalog: &RwLock<VisibleCatalog>,
    snapshot: &Snapshot,
    agent: Option<&str>,
    active_model: Option<&str>,
) -> (Vec<String>, Vec<String>) {
    let route_ids = snapshot.load().models();
    let catalog = catalog
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    catalog.models_for_agent(&route_ids, agent, active_model)
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, RwLock};

    use super::models_for_agent;
    use hofund_config::{VisibleCatalog, load};
    use tempfile::tempdir;

    use crate::RegistryAdapter;
    use crate::runtime::Runtime;

    #[test]
    fn g2_11_daemon_agent_models_follow_config() {
        let text = r#"
listen = "127.0.0.1:3425"
[[providers]]
id = "p"
protocol = "openai-chat"
base_url = "http://127.0.0.1:1"
api_key = "k"
[[routes]]
model = "a"
upstreams = ["p"]
[[routes]]
model = "b"
upstreams = ["p"]
[agents.visible]
codex = ["p"]
[agents.hidden_models]
codex = ["b"]
"#;
        let dir = tempdir().expect("dir");
        let path = dir.path().join("hofund.toml");
        std::fs::write(&path, text).expect("write");
        let config = Arc::new(load(text).expect("config"));
        let runtime = Runtime::new(Arc::new(RegistryAdapter), &path, None);
        let snapshot = runtime
            .initial_snapshot(config.as_ref(), None)
            .expect("snapshot");
        let catalog = Arc::new(RwLock::new(VisibleCatalog::build(
            Arc::clone(&config),
            &snapshot.load().models(),
        )));
        let (shown, _) = models_for_agent(&catalog, &snapshot, Some("codex"), None);
        assert_eq!(shown, ["a"]);
    }

    #[test]
    fn r7a_agent_models_hide_unlisted_exclusive_routes() {
        let text = r#"
listen = "127.0.0.1:3425"
[[providers]]
id = "listed"
protocol = "openai-chat"
base_url = "http://127.0.0.1:1"
api_key = "k"
[[providers]]
id = "secret"
listed = false
protocol = "openai-chat"
base_url = "http://127.0.0.1:2"
api_key = "k"
[[routes]]
model = "visible"
upstreams = ["listed"]
[[routes]]
model = "hidden-only"
upstreams = ["secret"]
[[routes]]
model = "group/work"
upstreams = [{ group = "inner" }]
[[routes]]
model = "group/inner"
upstreams = ["secret"]
"#;
        let dir = tempdir().expect("dir");
        let path = dir.path().join("hofund.toml");
        std::fs::write(&path, text).expect("write");
        let config = Arc::new(load(text).expect("config"));
        let runtime = Runtime::new(Arc::new(RegistryAdapter), &path, None);
        let snapshot = runtime
            .initial_snapshot(config.as_ref(), None)
            .expect("snapshot");
        let catalog = Arc::new(RwLock::new(VisibleCatalog::build(
            Arc::clone(&config),
            &snapshot.load().models(),
        )));
        let (shown, _) = models_for_agent(&catalog, &snapshot, None, None);
        assert!(shown.iter().any(|id| id == "visible"));
        assert!(shown.iter().any(|id| id == "group/work"));
        assert!(!shown.iter().any(|id| id == "hidden-only"));
    }
}
