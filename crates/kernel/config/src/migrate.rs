//! One-time endpoint rewrites for saved provider rows.

const TENCENT_CLOUD_PRESET: &str = "tencent-cloud";
const TENCENT_TOKEN_PLAN_ALIAS: &str = "tencent-token-plan";

const TENCENT_PLAN_MOVED: &[(&str, &str)] = &[
    (
        "https://api.lkeap.cloud.tencent.com/plan/v3",
        "https://tokenhub.tencentmaas.com/plan/v3",
    ),
    (
        "https://api.lkeap.cloud.tencent.com/plan/anthropic",
        "https://tokenhub.tencentmaas.com/plan/anthropic",
    ),
];

/// Rewrites known-retired Tencent Token Plan bases on preset-backed providers.
pub(crate) fn migrate_provider_base_url(preset: Option<&str>, base_url: &mut String) {
    let Some(preset) = preset else {
        return;
    };
    if preset != TENCENT_CLOUD_PRESET && preset != TENCENT_TOKEN_PLAN_ALIAS {
        return;
    }
    let trimmed = base_url.trim();
    for (old, new) in TENCENT_PLAN_MOVED {
        if trimmed.eq_ignore_ascii_case(old) {
            *base_url = (*new).to_owned();
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::migrate_provider_base_url;
    use crate::load;

    #[test]
    fn d53_2_migrate_old_tencent_plan_base_on_load() {
        let mut chat = "https://api.lkeap.cloud.tencent.com/plan/v3".to_owned();
        migrate_provider_base_url(Some("tencent-cloud"), &mut chat);
        assert_eq!(chat, "https://tokenhub.tencentmaas.com/plan/v3");

        let mut messages = "https://api.lkeap.cloud.tencent.com/plan/anthropic".to_owned();
        migrate_provider_base_url(Some("tencent-token-plan"), &mut messages);
        assert_eq!(messages, "https://tokenhub.tencentmaas.com/plan/anthropic");
    }

    #[test]
    fn d53_3_custom_lkeap_plan_without_preset_stays_put() {
        let mut custom = "https://api.lkeap.cloud.tencent.com/plan/v3".to_owned();
        migrate_provider_base_url(None, &mut custom);
        assert_eq!(custom, "https://api.lkeap.cloud.tencent.com/plan/v3");
    }

    #[test]
    fn d53_4_non_plan_lkeap_base_on_custom_provider_is_untouched() {
        let config = load(
            "[[providers]]\nid = \"custom\"\nprotocol = \"openai-chat\"\nbase_url = \"https://api.lkeap.cloud.tencent.com/v1\"\napi_key = \"key\"\n",
        )
        .expect("load");
        assert_eq!(
            config.providers[0].base_url,
            "https://api.lkeap.cloud.tencent.com/v1"
        );
    }

    #[test]
    fn d53_5_load_rewrites_preset_backed_tencent_plan_base() {
        let config = load(
            "[[providers]]\nid = \"tc\"\npreset = \"tencent-cloud\"\nprotocol = \"openai-chat\"\nbase_url = \"https://api.lkeap.cloud.tencent.com/plan/v3\"\napi_key = \"key\"\n",
        )
        .expect("load");
        assert_eq!(
            config.providers[0].base_url,
            "https://tokenhub.tencentmaas.com/plan/v3"
        );
    }
}
