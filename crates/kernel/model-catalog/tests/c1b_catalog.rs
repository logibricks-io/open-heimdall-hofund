//! C-1b catalog visibility: retired, deprecated, and disabled models.

use std::path::Path;
use std::sync::Arc;

use hofund_config::CatalogSettings;
use hofund_model_catalog::{
    CatalogVisibility, ModelCatalog, build_index, glob_match, hop_names, is_deprecated,
    listable_route_models, match_patterns, model_names, parse_catalog,
};

struct IdleFetcher;

impl hofund_model_catalog::CatalogFetcher for IdleFetcher {
    fn fetch(
        &self,
        _: &str,
        _: Option<&str>,
    ) -> Result<hofund_model_catalog::FetchResponse, std::io::Error> {
        Err(std::io::Error::other("unused"))
    }
}

const FIXTURE: &str = r#"{
  "acme": {
    "id": "acme",
    "models": {
      "fresh": {"id": "fresh", "status": "active", "limit": {"context": 32000}},
      "old": {"id": "old", "status": "retired", "limit": {"context": 32000}},
      "legacy": {"id": "legacy", "status": "deprecated", "limit": {"context": 32000}}
    }
  }
}"#;

fn catalog(dir: &Path, settings: CatalogSettings) -> Arc<ModelCatalog> {
    std::fs::create_dir_all(dir.join("catalog")).expect("mkdir");
    std::fs::write(
        dir.join("catalog/models.dev.json"),
        format!(r#"{{"catalog": {FIXTURE}}}"#),
    )
    .expect("cache");
    let catalog = ModelCatalog::open(dir, settings, Arc::new(IdleFetcher)).expect("open");
    catalog.reload().expect("reload");
    catalog
}

#[test]
fn c1b_1_parse_status_marks_deprecated() {
    assert!(is_deprecated("deprecated"));
    assert!(!is_deprecated("retired"));
}

#[test]
fn c1b_2_config_retired_override_matches_bare_id() {
    let settings = CatalogSettings {
        retired: vec!["forced".to_owned()],
        ..Default::default()
    };
    assert!(match_patterns(&settings.retired, &model_names("", "forced")).is_some());
}

#[test]
fn c1b_3_list_hiding_hides_retired_and_deprecated_routes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let catalog = catalog(dir.path(), CatalogSettings::default());
    let policy = catalog.visibility();
    assert!(policy.hide_route_from_lists("old", catalog.as_ref()));
    assert!(policy.hide_route_from_lists("legacy", catalog.as_ref()));
    assert!(!policy.hide_route_from_lists("fresh", catalog.as_ref()));
}

#[test]
fn c1b_4_listable_route_models_filters_hidden() {
    let dir = tempfile::tempdir().expect("tempdir");
    let catalog = catalog(dir.path(), CatalogSettings::default());
    let listed = listable_route_models(
        ["fresh", "old", "legacy"].map(str::to_owned),
        Some(catalog.as_ref()),
        Some(catalog.visibility()),
    );
    assert_eq!(listed, vec!["fresh".to_owned()]);
}

#[test]
fn c1b_5_disabled_config_blocks_id() {
    let settings = CatalogSettings {
        disabled: vec!["fresh".to_owned()],
        ..Default::default()
    };
    let policy = CatalogVisibility::open(settings, Path::new("."));
    assert!(policy.id_disabled(&["fresh".to_owned()]));
}

#[test]
fn c1b_6_disabled_pattern_message_names_model() {
    let settings = CatalogSettings {
        disabled: vec!["fresh".to_owned()],
        ..Default::default()
    };
    let policy = CatalogVisibility::open(settings, Path::new("."));
    let pattern = policy.disabled_pattern("fresh").expect("pattern");
    let message = policy.disabled_message("fresh", &pattern);
    assert!(message.contains("fresh"));
    assert!(message.contains("disabled"));
}

#[test]
fn c1b_7_edit_catalog_disabled_roundtrip() {
    let original = "[catalog]\ndisabled = [\"a\"]\nlisten = \"127.0.0.1:9\"\n";
    let edited =
        hofund_config::edit_catalog_disabled(original, &["b".to_owned()], &[]).expect("add");
    assert!(edited.contains("\"a\""));
    assert!(edited.contains("\"b\""));
    let cleared =
        hofund_config::edit_catalog_disabled(&edited, &[], &["a".to_owned()]).expect("remove");
    assert!(!cleared.contains("\"a\""));
}

#[test]
fn c1b_8_build_index_marks_retired_from_status() {
    let catalog = parse_catalog(FIXTURE.as_bytes()).expect("parse");
    let index = build_index(&catalog, &CatalogSettings::default());
    let entry = index.resolve("old").expect("old");
    assert!(entry.retired);
}

#[test]
fn c1b_19_model_names_include_the_last_path_segment() {
    let names = model_names("", "vendor/m");
    assert!(names.contains(&"m".to_owned()), "{names:?}");
}

#[test]
fn c1b_20_glob_is_case_insensitive_and_question_mark_is_one_byte() {
    assert!(glob_match("Fresh", "fresh"));
    assert!(glob_match("gpt-?", "gpt-5"));
    assert!(!glob_match("gpt-?", "gpt-55"));
}

#[test]
fn c1b_21_config_retired_and_disabled_hide_known_routes_from_lists() {
    let dir = tempfile::tempdir().expect("tempdir");
    let catalog = catalog(dir.path(), CatalogSettings::default());
    let policy = catalog.visibility();
    assert!(!policy.hide_route_from_lists("fresh", catalog.as_ref()));
    policy.replace_settings(CatalogSettings {
        retired: vec!["fresh".to_owned()],
        ..Default::default()
    });
    assert!(policy.hide_route_from_lists("fresh", catalog.as_ref()));
    policy.replace_settings(CatalogSettings {
        disabled: vec!["fresh".to_owned()],
        ..Default::default()
    });
    assert!(policy.hide_route_from_lists("fresh", catalog.as_ref()));
}

#[test]
fn c1b_22_config_retired_hides_an_unknown_route() {
    let dir = tempfile::tempdir().expect("tempdir");
    let catalog = catalog(
        dir.path(),
        CatalogSettings {
            retired: vec!["ghost".to_owned()],
            ..Default::default()
        },
    );
    let policy = catalog.visibility();
    assert!(policy.hide_route_from_lists("ghost", catalog.as_ref()));
    assert!(!policy.hide_route_from_lists("phantom", catalog.as_ref()));
}

#[test]
fn c1b_23_hop_names_cover_the_asked_alias() {
    let names = hop_names("route", "vendor/alias", "p", "wire");
    assert!(names.contains(&"vendor/alias".to_owned()), "{names:?}");
    assert!(names.contains(&"alias".to_owned()), "{names:?}");
}

#[test]
fn c1b_24_override_retired_flag_marks_the_entry() {
    let catalog = parse_catalog(FIXTURE.as_bytes()).expect("parse");
    let settings = hofund_config::load(
        r#"
listen = "127.0.0.1:9"
[[catalog.overrides]]
model = "fresh"
retired = true
[[catalog.overrides]]
model = "old"
retired = false
"#,
    )
    .expect("config");
    let index = build_index(&catalog, &settings.catalog);
    assert!(index.resolve("fresh").expect("fresh").retired);
    assert!(!index.resolve("old").expect("old").retired);
}
