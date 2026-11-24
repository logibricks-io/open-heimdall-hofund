//! D-264 regional catalog selection and saved-value migration.

use hofund_plugin_registry::{
    catalogs_of, find_preset, membership_catalogs, model_plan_of, resolve_region,
};

#[test]
fn d264_glm_regions_follow_their_catalog_and_keep_user_choices() {
    for (preset_id, plan_catalog) in [("zhipu", "zhipuai-coding-plan"), ("zai", "zai-coding-plan")]
    {
        let (preset, _) = find_preset(preset_id).expect("preset");
        for region in preset.regions {
            let expected = if region.id == "coding" {
                vec![plan_catalog.to_owned(), "zhipuai".to_owned()]
            } else {
                vec!["zhipuai".to_owned()]
            };
            let mut saved = vec!["", preset.catalog];
            saved.extend(preset.regions.iter().map(|region| region.catalog));
            for saved in saved {
                assert_eq!(
                    catalogs_of(preset, Some(region), saved),
                    expected,
                    "{preset_id}/{} saved with {saved:?}",
                    region.id
                );
            }
            assert_eq!(
                catalogs_of(preset, Some(region), "openrouter"),
                ["openrouter"]
            );
            let expected_membership = if region.id == "coding" {
                vec![plan_catalog.to_owned()]
            } else {
                Vec::new()
            };
            assert_eq!(
                model_plan_of(preset, Some(region)).membership_catalogs,
                expected_membership
            );
        }
    }
}

#[test]
fn d264_saved_catalogs_normalize_before_preset_comparison() {
    for preset_id in ["zhipu", "zai"] {
        let (preset, _) = find_preset(preset_id).expect("preset");
        for region in preset.regions {
            let expected = model_plan_of(preset, Some(region)).catalogs;
            let upper_preset = preset.catalog.to_ascii_uppercase();
            assert_eq!(catalogs_of(preset, Some(region), &upper_preset), expected);
            let compact = region.catalog.to_ascii_uppercase().replace(", ", ",");
            assert_eq!(catalogs_of(preset, Some(region), &compact), expected);
            let spaces = region.catalog.replace(", ", " ");
            assert_eq!(catalogs_of(preset, Some(region), &spaces), expected);
            assert_eq!(
                catalogs_of(preset, Some(region), " OpenRouter openrouter,OPENROUTER "),
                ["openrouter"]
            );
        }
    }
}

#[test]
fn d264_membership_set_contains_only_the_two_glm_plan_catalogs() {
    let other = [
        "alibaba-coding-plan",
        "minimax-coding-plan",
        "volcengine-coding-plan",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    assert!(membership_catalogs(&other).is_empty());
    let glm = ["zhipuai-coding-plan", "zai-coding-plan"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    assert_eq!(membership_catalogs(&glm), glm);
}

#[test]
fn d264_alias_region_uses_the_same_ordered_catalog() {
    let (preset, _) = find_preset("zhipu").expect("preset");
    let region = resolve_region(preset, Some("coding")).expect("region");
    assert_eq!(
        catalogs_of(preset, region, "zhipuai"),
        ["zhipuai-coding-plan", "zhipuai"]
    );
}
