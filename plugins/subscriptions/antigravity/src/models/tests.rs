use hofund_plugin_api::{AccountId, ModelSpec};
use hofund_plugin_sdk::json::json;

use super::{Catalog, RawModel, RefusedKey, TierReading, collapse, merge, parse, sent_id};

fn raw(id: &str, name: &str) -> RawModel {
    RawModel {
        id: id.to_owned(),
        name: name.to_owned(),
        context: 0,
        output: 0,
        images: Some(true),
        remaining: None,
        resets: None,
        picker: 0,
        enum_id: String::new(),
    }
}

fn reference_list() -> Vec<RawModel> {
    [
        ("claude-opus-4-6-thinking", "Claude Opus 4.6 (Thinking)"),
        ("gemini-3-flash", "Gemini 3 Flash"),
        ("gemini-3-flash-agent", "Gemini 3.5 Flash (High)"),
        ("gemini-3.1-pro-high", "Gemini 3.1 Pro (High)"),
        ("gemini-3.1-pro-low", "Gemini 3.1 Pro (Low)"),
        ("gemini-3.5-flash-extra-low", "Gemini 3.5 Flash (Low)"),
        ("gemini-3.5-flash-lite", "Gemini 3.5 Flash Lite"),
        ("gemini-3.5-flash-low", "Gemini 3.5 Flash (Medium)"),
        ("gemini-3.7-flash-high", "Gemini 3.7 Flash (High)"),
        ("gemini-3.7-flash-low", "Gemini 3.7 Flash (Low)"),
        ("gemini-3.7-flash-medium", "Gemini 3.7 Flash (Medium)"),
        ("gemini-3.7-flash-tiered", ""),
        ("gemini-pro-agent", "Gemini 3.1 Pro (High)"),
        ("gpt-oss-120b-medium", "GPT-OSS 120B (Medium)"),
    ]
    .into_iter()
    .map(|(id, name)| raw(id, name))
    .collect()
}

fn shapes(models: Vec<ModelSpec>) -> Vec<String> {
    models
        .into_iter()
        .map(|model| {
            format!(
                "{}|{}|{}",
                model.id(),
                model.display_name().unwrap_or_default(),
                model.efforts().join(",")
            )
        })
        .collect()
}

#[test]
fn r49c_reference_effort_families_and_tiered_names() {
    assert_eq!(
        shapes(collapse(&reference_list()).expect("catalog")),
        [
            "claude-opus-4-6-thinking|Claude Opus 4.6 (Thinking)|",
            "gemini-3-flash|Gemini 3 Flash|",
            "gemini-3-flash-agent|Gemini 3.5 Flash (High)|",
            "gemini-3.1-pro|Gemini 3.1 Pro|low,high",
            "gemini-3.5-flash|Gemini 3.5 Flash|low,medium",
            "gemini-3.5-flash-lite|Gemini 3.5 Flash Lite|",
            "gemini-3.7-flash|Gemini 3.7 Flash|low,medium,high",
            "gemini-3.7-flash-tiered|Gemini 3.7 Flash (Tiered)|low,medium,high",
            "gemini-pro-agent|Gemini 3.1 Pro (High)|",
            "gpt-oss-120b-medium|GPT-OSS 120B (Medium)|",
        ]
    );
    let listed = reference_list();
    for (model, effort, expected) in [
        ("gemini-3.7-flash", None, "gemini-3.7-flash-high"),
        ("gemini-3.7-flash", Some("low"), "gemini-3.7-flash-low"),
        (
            "gemini-3.7-flash",
            Some("medium"),
            "gemini-3.7-flash-medium",
        ),
        ("gemini-3.7-flash-high", None, "gemini-3.7-flash-high"),
        ("gemini-3.7-flash-high", Some("low"), "gemini-3.7-flash-low"),
        ("gemini-3.5-flash", None, "gemini-3.5-flash-low"),
        (
            "gemini-3.5-flash",
            Some("low"),
            "gemini-3.5-flash-extra-low",
        ),
        ("gemini-3.5-flash", Some("medium"), "gemini-3.5-flash-low"),
        ("gemini-3.1-pro", Some("xhigh"), "gemini-3.1-pro-high"),
        ("gemini-3-flash", Some("high"), "gemini-3-flash"),
        ("gpt-oss-120b-medium", None, "gpt-oss-120b-medium"),
    ] {
        assert_eq!(
            sent_id(&listed, model, effort),
            expected,
            "{model} at {effort:?}"
        );
    }
    let no_high = [raw("m-low", ""), raw("m-medium", "")];
    assert_eq!(sent_id(&no_high, "m", None), "m-medium");
    let plain = [raw("m-low", ""), raw("m", ""), raw("m-high", "")];
    assert_eq!(sent_id(&plain, "m", None), "m");
    assert_eq!(sent_id(&plain, "m", Some("minimal")), "m-low");
}

#[test]
fn r49c_empty_catalog_uses_the_reference_fallback() {
    let models = collapse(&[]).expect("fallback");
    assert_eq!(
        models.iter().map(|model| model.id()).collect::<Vec<_>>(),
        [
            "gemini-pro-agent",
            "gemini-3.1-pro-low",
            "gemini-3-flash",
            "gemini-3.1-flash-lite",
            "claude-opus-4-6-thinking",
            "claude-sonnet-4-6",
            "gpt-oss-120b-medium",
        ]
    );
    assert_eq!(models[0].display_name(), Some("Gemini 3.1 Pro (High)"));
    assert_eq!(models[6].image_input(), Some(false));
}

#[test]
fn r49c_name_correction_and_internal_filtering_match_the_reply() {
    let parsed = parse(&json!({"models": {
        "gemini-2.5-flash": {
            "displayName": "Gemini 3.5 Flash Lite",
            "model": "MODEL_GOOGLE_GEMINI_2_5_FLASH"
        },
        "gemini-3.5-flash-lite": {
            "displayName": "Gemini 3.5 Flash Lite",
            "model": "MODEL_PLACEHOLDER_M198"
        },
        "gemini-4-argon-disabled": {"disabled": true},
        "chat_new": {},
        "tab_new": {}
    }}))
    .expect("models");
    let models = collapse(&parsed).expect("catalog");
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id(), "gemini-2.5-flash");
    assert_eq!(models[0].display_name(), Some("gemini-2.5-flash"));
    assert_eq!(models[1].display_name(), Some("Gemini 3.5 Flash Lite"));
    assert!(parse(&json!({})).expect("empty").is_empty());
}

#[test]
fn r49c_merge_preserves_order_and_least_capabilities() {
    let first = vec![
        ModelSpec::new("g", None).expect("g"),
        ModelSpec::new("old", None)
            .expect("old")
            .with_context(100)
            .with_max_output(20)
            .with_image_input(Some(true)),
        ModelSpec::new("oss", None).expect("oss"),
    ];
    let second = vec![
        ModelSpec::new("g", None).expect("g"),
        ModelSpec::new("new", None).expect("new"),
        ModelSpec::new("old", None)
            .expect("old")
            .with_context(200)
            .with_max_output(10)
            .with_image_input(Some(false)),
        ModelSpec::new("oss", None).expect("oss"),
    ];
    let merged = merge(first, second);
    assert_eq!(
        merged.iter().map(|model| model.id()).collect::<Vec<_>>(),
        ["g", "new", "old", "oss"]
    );
    assert_eq!(merged[2].context(), 100);
    assert_eq!(merged[2].max_output(), 10);
    assert_eq!(merged[2].image_input(), Some(false));

    let unknown = ModelSpec::new("m", None).expect("m").with_image_input(None);
    let vision = ModelSpec::new("m", None)
        .expect("m")
        .with_image_input(Some(true));
    assert_eq!(merge(vec![unknown], vec![vision])[0].image_input(), None);
}

#[test]
fn r49c_new_sign_in_and_project_forget_only_their_own_state() {
    let catalog = Catalog::default();
    let first = AccountId::from_static("a-0000000000000001");
    let second = AccountId::from_static("a-0000000000000002");
    {
        let mut state = catalog.state.lock().expect("state");
        for account in [&first, &second] {
            state.listings.insert(account.clone(), reference_list());
            state.tiers.insert(
                account.clone(),
                TierReading {
                    tier: "standard-tier".to_owned(),
                    paid: "g1-pro-tier".to_owned(),
                    at: std::time::SystemTime::UNIX_EPOCH,
                },
            );
            state.refused.insert(
                RefusedKey {
                    account: account.clone(),
                    project: "project".to_owned(),
                    tier: "g1-pro-tier".to_owned(),
                },
                std::time::SystemTime::UNIX_EPOCH,
            );
        }
    }
    catalog.forget_project(&first);
    assert!(catalog.own(&first).len() == reference_list().len());
    assert!(
        !catalog
            .state
            .lock()
            .expect("state")
            .tiers
            .contains_key(&first)
    );
    assert!(
        catalog
            .state
            .lock()
            .expect("state")
            .tiers
            .contains_key(&second)
    );
    catalog.forget(&first);
    assert!(catalog.own(&first).is_empty());
    assert!(!catalog.own(&second).is_empty());
    assert!(
        catalog
            .state
            .lock()
            .expect("state")
            .refused
            .keys()
            .all(|key| key.account == second)
    );
}
