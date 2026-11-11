//! Raw, unframed GetCliModelConfigs protobuf messages.

use std::collections::HashMap;

use hofund_plugin_sdk::protobuf::{ProtoBuf, fields};

use super::metadata;
use crate::models::{Family, Variant, snapshot_families};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelConfigs {
    pub(crate) families: Vec<Family>,
    pub(crate) images: HashMap<String, bool>,
}

pub(crate) fn parse(body: &[u8]) -> Option<ModelConfigs> {
    let mut images = HashMap::new();
    let mut families: Vec<Family> = Vec::new();
    for config in fields(body)
        .iter()
        .filter(|field| field.number == 1)
        .filter_map(|field| field.as_bytes())
    {
        let mut uid = String::new();
        let mut name = String::new();
        let mut context = 0;
        let mut output = 0;
        let mut family_uid = String::new();
        let mut family_label = String::new();
        let mut supports_images = false;
        for field in fields(config) {
            match field.number {
                1 => name = field.as_string().unwrap_or_default(),
                5 => supports_images = field.as_varint().is_some_and(|value| value != 0),
                18 => context = field.as_varint().unwrap_or(0),
                22 => uid = field.as_string().unwrap_or_default(),
                23 => {
                    for info in field.as_bytes().map(fields).unwrap_or_default() {
                        match info.number {
                            13 => output = info.as_varint().unwrap_or(0),
                            23 => family_uid = info.as_string().unwrap_or_default(),
                            _ => {}
                        }
                    }
                }
                30 => {
                    family_label = field
                        .as_bytes()
                        .map(fields)
                        .unwrap_or_default()
                        .iter()
                        .find(|field| field.number == 1)
                        .and_then(|field| field.as_string())
                        .unwrap_or_default();
                }
                _ => {}
            }
        }
        if uid.is_empty() {
            continue;
        }
        images.insert(uid.clone(), supports_images);
        let id = if !family_uid.is_empty() {
            family_uid
        } else if !family_label.is_empty() {
            family_label.clone()
        } else if !name.is_empty() {
            name.clone()
        } else {
            uid.clone()
        };
        if id.eq_ignore_ascii_case("adaptive") || id.eq_ignore_ascii_case("fusion") {
            continue;
        }
        let family = if let Some(index) = families.iter().position(|family| family.uid == id) {
            &mut families[index]
        } else {
            let aliases = snapshot_families()
                .iter()
                .find(|family| family.uid == id)
                .map(|family| family.aliases.clone())
                .unwrap_or_default();
            let label = if !family_label.is_empty() {
                family_label
            } else if !name.is_empty() {
                name.clone()
            } else {
                id.clone()
            };
            families.push(Family {
                uid: id,
                label,
                aliases,
                models: Vec::new(),
            });
            families.last_mut().expect("family was just pushed")
        };
        family.models.push(Variant {
            name: if name.is_empty() { uid.clone() } else { name },
            context,
            output,
            id: uid,
        });
    }
    (!images.is_empty() && !families.is_empty()).then_some(ModelConfigs { families, images })
}

pub(crate) fn encode_request(session_token: &str, os: &str) -> Vec<u8> {
    ProtoBuf::new()
        .bytes(1, &metadata(session_token, os))
        .finish()
}

#[cfg(test)]
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

    use super::{encode_request, model_uids, parse};

    const MODEL_CONFIGS: &[u8] = include_bytes!("testdata/model-configs.bin");
    const MODEL_CONFIGS_1015: &[u8] = include_bytes!("testdata/model-configs-1015.bin");

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

    #[test]
    fn golden_parses_family_metadata_and_image_flags() {
        let configs = parse(MODEL_CONFIGS).expect("configs");
        let swe = configs
            .families
            .iter()
            .find(|family| family.uid == "swe-2")
            .expect("SWE family");
        assert_eq!(swe.label, "SWE-2");
        assert_eq!(swe.aliases, ["swe"]);
        assert_eq!(swe.models[0].context, 262_000);
        assert_eq!(swe.models[0].output, 128_000);
        assert_eq!(configs.images.get("swe-2-high"), Some(&true));
        assert!(parse(b"").is_none());
    }
}
