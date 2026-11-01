
#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use hofund_config::Config;
    use hofund_control_proto::QuotaKind;
    use hofund_plugin_registry as _;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::runtime::Runtime;
    use crate::{RegistryAdapter, provider_keys};

    fn config(text: &str) -> Config {
        hofund_config::load(text).expect("config")
    }

    fn runtime(dir: &Path, config: &Config) -> Arc<Runtime> {
        let runtime = Runtime::new(
            Arc::new(RegistryAdapter),
            &dir.join("hofund.toml"),
            Some(0),
        );
        runtime.initial_snapshot(config, None).expect("snapshot");
        runtime
    }

    fn kimi_config(url: &str, fail: bool) -> Config {
        let extra = if fail {
            "[[providers.keys]]\nkey = \"second\"\nname = \"two\"\n"
        } else {
            ""
        };
        config(&format!(
            "[[providers]]\nid = \"kimi\"\npreset = \"kimi-code\"\napi_key = \"first\"\nkey_name = \"one\"\n\
             [providers.options]\nplan_quota_url = \"{url}\"\n{extra}\
             [[routes]]\nmodel = \"demo\"\nupstreams = [\"kimi\"]\n"
        ))
    }

    fn kimi_body(used: f64) -> String {
        format!(
            "{{\"usages\":{{\"limit_5h\":{{\"used_ratio\":{},\"reset_time\":\"2026-11-17T00:00:00Z\"}},\"limit_month_total\":{{\"used_ratio\":0.4,\"reset_time\":\"2026-12-01T00:00:00Z\"}}}}}}",
            used / 100.0
        )
    }

    #[tokio::test]
    async fn p11_plan_windows_feed_cache_history_and_router_allowances() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usages"))
            .respond_with(ResponseTemplate::new(200).set_body_string(kimi_body(49.0)))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("dir");
        let config = kimi_config(&format!("{}/usages", server.uri()), true);
        let runtime = runtime(dir.path(), &config);
        let (kept, _) = runtime.quota_report(true).await;
        assert!(kept.is_empty());
        assert!(server.received_requests().await.expect("requests").is_empty());

        let (cards, _) = runtime.quota_report(false).await;
        assert_eq!(cards.len(), 2);
        assert!(cards.iter().all(|card| card.kind == QuotaKind::Plan));
        assert!(cards.iter().all(|card| card.windows.len() == 2));
        assert_eq!(cards[0].windows[1].name, "Month");
        assert_eq!(cards[0].windows[1].used, 40.0);
        let first = provider_keys::key_id("first");
        let second = provider_keys::key_id("second");
        assert_eq!(cards[0].member, format!("kimi#{first}"));
        assert_eq!(cards[1].member, format!("kimi#{second}"));
        assert_eq!(cards[1].alias, "two");

        let now = runtime.quota_now();
        let allowances = runtime.quota_allowances();
        assert_eq!(allowances.len(), 2);
        assert_eq!(allowances[0].member, format!("kimi#{first}"));
        assert_eq!(allowances[1].member, format!("kimi#{second}"));
        assert_eq!(allowances[1].used_for_routing("demo", now), 49.0);
        assert_eq!(allowances[1].windows[1].name, "Month");
        let history = runtime.quota_histories(None, Some("kimi"), None);
        assert_eq!(history.len(), 2);
        assert!(history.iter().all(|entry| entry.lines.len() == 2));
        let kept = runtime.quota_report(true).await.0;
        assert_eq!(kept.len(), 2);
        assert_eq!(server.received_requests().await.expect("requests").len(), 2);
    }

    #[tokio::test]
    async fn p12_per_key_plan_and_failed_read_keep_the_last_card() {
        let server = MockServer::start().await;
        let limited = Arc::new(AtomicBool::new(false));
        let failing = Arc::clone(&limited);
        Mock::given(method("GET"))
            .and(path("/api/monitor/usage/quota/limit"))
            .respond_with(move |request: &wiremock::Request| {
                let authorization = request
                    .headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or("");
                if failing.load(Ordering::SeqCst) && authorization == "second" {
                    return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "code": 1302, "msg": "too busy", "success": false
                    }));
                }
                let used = if authorization == "second" { 90 } else { 20 };
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "code": 200, "success": true,
                    "data": {"level": "pro", "limits": [
                        {"type": "TOKENS_LIMIT", "unit": 3, "number": 5, "percentage": used, "nextResetTime": 4_102_444_800_000_i64}
                    ]}
                }))
            })
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().expect("dir");
        let config = config(&format!(
            "[[providers]]\nid = \"glm\"\npreset = \"zhipu\"\napi_key = \"first\"\nkey_name = \"one\"\nkey_plan = \"api\"\n\
             [providers.options]\nregion = \"api\"\nplan_quota_url = \"{}/api/monitor/usage/quota/limit\"\n\
             [[providers.keys]]\nkey = \"second\"\nname = \"two\"\nplan = \"coding\"\n\
             [[routes]]\nmodel = \"demo\"\nupstreams = [\"glm\"]\n",
            server.uri()
        ));
        let runtime = runtime(dir.path(), &config);
        let cards = runtime.quota_report(false).await.0;
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].windows[0].used, 20.0);
        assert_eq!(cards[1].windows[0].used, 90.0);
        assert_eq!(cards[1].member, format!("glm#{}", provider_keys::key_id("second")));

        limited.store(true, Ordering::SeqCst);
        let cards = runtime.quota_report(false).await.0;
        assert_eq!(cards.len(), 2);
        assert!(cards[1].as_of.is_some());
        assert_eq!(cards[1].windows[0].used, 90.0);
        assert!(cards[1].error.is_empty());
        let before = server.received_requests().await.expect("requests").len();
        let kept = runtime.quota_report(true).await.0;
        assert_eq!(kept.len(), 2);
        assert_eq!(server.received_requests().await.expect("requests").len(), before);
    }

    #[tokio::test]
    async fn p13_many_plan_keys_read_eight_at_a_time() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let address = listener.local_addr().expect("address");
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let server = {
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            let calls = Arc::clone(&calls);
            tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let active = Arc::clone(&active);
                    let maximum = Arc::clone(&maximum);
                    let calls = Arc::clone(&calls);
                    tokio::spawn(async move {
                        let now = active.fetch_add(1, Ordering::SeqCst) + 1;
                        maximum.fetch_max(now, Ordering::SeqCst);
                        calls.fetch_add(1, Ordering::SeqCst);
                        let mut request = Vec::new();
                        let mut buffer = [0_u8; 2048];
                        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                            let Ok(read) = socket.read(&mut buffer).await else {
                                break;
                            };
                            if read == 0 {
                                break;
                            }
                            request.extend_from_slice(&buffer[..read]);
                        }
                        tokio::time::sleep(Duration::from_millis(25)).await;
                        let body = br#"{"usages":{"limit_5h":{"used_ratio":0.5}}}"#;
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            body.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                        let _ = socket.write_all(body).await;
                        let _ = socket.shutdown().await;
                        active.fetch_sub(1, Ordering::SeqCst);
                    });
                }
            })
        };
        let url = format!("http://{address}/usages");
        let mut text = format!(
            "[[providers]]\nid = \"kimi\"\npreset = \"kimi-code\"\napi_key = \"key-0\"\n[providers.options]\nplan_quota_url = \"{url}\"\n"
        );
        for index in 1..31 {
            text.push_str(&format!("[[providers.keys]]\nkey = \"key-{index}\"\n"));
        }
        text.push_str("[[routes]]\nmodel = \"demo\"\nupstreams = [\"kimi\"]\n");
        let dir = tempfile::tempdir().expect("dir");
        let runtime = runtime(dir.path(), &config(&text));
        let cards = runtime.quota_report(false).await.0;
        server.abort();
        assert_eq!(cards.len(), 31);
        assert_eq!(calls.load(Ordering::SeqCst), 31);
        assert!(maximum.load(Ordering::SeqCst) <= 8, "too many at once: {}", maximum.load(Ordering::SeqCst));
    }
}
