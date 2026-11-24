//! D-264 regional catalog selection and saved-value migration.

use hofund_plugin_registry::{catalogs_of, find_preset, resolve_region};

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
        }
    }
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
