//! Raw, unframed GetCliModelConfigs protobuf messages.

use hofund_plugin_sdk::protobuf::{ProtoBuf, fields};

use super::metadata;

pub(crate) fn encode_request(session_token: &str, os: &str) -> Vec<u8> {
    ProtoBuf::new()
        .bytes(1, &metadata(session_token, os))
        .finish()
}

pub(crate) fn model_uids(body: &[u8]) -> Vec<String> {
    fields(body)
        .iter()
        .filter(|field| field.number == 1)
        .filter_map(|field| field.as_bytes())
        .filter_map(|config| {
            fields(config)
                .iter()
                .filter(|field| field.number == 22)
                .filter_map(|field| field.as_string())
                .next_back()
                .filter(|uid| !uid.is_empty())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use hofund_plugin_sdk::protobuf::{deframe, fields};

    use super::{encode_request, model_uids};

    const MODEL_CONFIGS: &[u8] = crate::connect::testdata::MODEL_CONFIGS;
    const MODEL_CONFIGS_1015: &[u8] = crate::connect::testdata::MODEL_CONFIGS_1015;

    #[test]
    fn request_is_one_raw_metadata_field() {
        let encoded = encode_request("config-session", "windows");
        let outer = fields(&encoded);
        assert_eq!(outer.len(), 1);
        assert_eq!(outer[0].number, 1);
        let metadata = fields(outer[0].as_bytes().expect("metadata"));
        assert_eq!(
            metadata
                .iter()
                .find(|field| field.number == 3)
                .and_then(|field| field.as_string())
                .as_deref(),
            Some("config-session")
        );
        assert_eq!(
            metadata
                .iter()
                .find(|field| field.number == 5)
                .and_then(|field| field.as_string())
                .as_deref(),
            Some("windows")
        );
        assert!(deframe(&encoded).is_err(), "config RPC must not be framed");
    }

    #[test]
    fn model_configs_golden_yields_the_reference_variant_uids() {
        assert_eq!(
            model_uids(MODEL_CONFIGS),
            [
                "swe-2-high",
                "gemini-4-argon-medium",
                "glm-5-2",
                "gemini-4-argon-low",
                "gemini-4-argon-high",
                "glm-5-2-max",
                "deepseek-v4-flash-high",
                "deepseek-v4-flash-max",
                "swe-2-medium",
                "swe-2-max",
                "MODEL_PRIVATE_11",
            ]
        );
    }

    #[test]
    fn model_configs_1015_golden_yields_the_reference_variant_uids() {
        assert_eq!(
            model_uids(MODEL_CONFIGS_1015),
            [
                "swe-2-high",
                "swe-1-7-lightning",
                "gemini-4-argon-medium",
                "glm-5-2",
                "gemini-4-argon-low",
                "gemini-4-argon-high",
                "swe-2-medium",
                "swe-2-max",
                "swe-2-medium-lightning",
                "swe-2-high-lightning",
                "swe-2-max-lightning",
            ]
        );
    }

    #[test]
    fn malformed_config_tail_keeps_uid_before_the_tail() {
        let mut truncated = MODEL_CONFIGS.to_vec();
        truncated.truncate(truncated.len() - 3);
        let uids = model_uids(&truncated);
        assert_eq!(uids.len(), 11);
        assert_eq!(uids[0], "swe-2-high");
        assert_eq!(uids[10], "MODEL_PRIVATE_11");
    }
}
