//! Raw, unframed GetCliModelConfigs protobuf messages.

use std::collections::{HashMap, HashSet};

use hofund_plugin_sdk::protobuf::{ProtoBuf, WireValue, fields};

use super::metadata;
use crate::models::{Family, Variant, snapshot_families};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Price {
    pub(crate) input: u64,
    pub(crate) output: u64,
    pub(crate) cache_read: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelConfigs {
    pub(crate) families: Vec<Family>,
    pub(crate) images: HashMap<String, bool>,
    pub(crate) free: HashSet<String>,
    pub(crate) prices: HashMap<String, Price>,
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
    (!images.is_empty() && !families.is_empty()).then_some(ModelConfigs {
        families,
        images,
        free: parse_free(body),
        prices: parse_prices(body),
    })
}

pub(crate) fn parse_free(body: &[u8]) -> HashSet<String> {
    let mut free = HashSet::new();
    for config in fields(body)
        .iter()
        .filter(|field| field.number == 1)
        .filter_map(|field| field.as_bytes())
    {
        let mut uid = String::new();
        let mut tier = 0;
        for field in fields(config) {
            match field.number {
                22 => uid = field.as_string().unwrap_or_default(),
                24 => tier = field.as_varint().unwrap_or(0),
                _ => {}
            }
        }
        if !uid.is_empty() && tier == 4 {
            free.insert(uid);
        }
    }
    free
}

pub(crate) fn parse_prices(body: &[u8]) -> HashMap<String, Price> {
    let mut prices = HashMap::new();
    for config in fields(body)
        .iter()
        .filter(|field| field.number == 1)
        .filter_map(|field| field.as_bytes())
    {
        let mut uid = String::new();
        let mut input = None;
        let mut output = None;
        let mut cache_read = None;
        for field in fields(config) {
            match field.number {
                22 => uid = field.as_string().unwrap_or_default(),
                32 => {
                    if let Some((name, value)) = field.as_bytes().and_then(price_row) {
                        match name {
                            "input" => input = Some(value),
                            "output" => output = Some(value),
                            "read" => cache_read = Some(value),
                            _ => unreachable!("price rows have fixed labels"),
                        }
                    }
                }
                _ => {}
            }
        }
        if uid.is_empty() || input.unwrap_or(0) == 0 && output.unwrap_or(0) == 0 {
            continue;
        }
        prices.insert(
            uid,
            Price {
                input: input.unwrap_or(0),
                output: output.unwrap_or(0),
                cache_read: cache_read.unwrap_or(0),
            },
        );
    }
    prices
}

fn price_row(bytes: &[u8]) -> Option<(&'static str, u64)> {
    let mut label = String::new();
    let mut unit = String::new();
    let mut value = None;
    for field in fields(bytes) {
        match field.number {
            1 => label = field.as_string().unwrap_or_default(),
            2 => {
                if let WireValue::Fixed32(bits) = field.value {
                    value = price_micros(f32::from_bits(bits));
                }
            }
            3 => unit = field.as_string().unwrap_or_default(),
            _ => {}
        }
    }
    if unit != "1M tokens" {
        return None;
    }
    let name = match label.as_str() {
        "Input" => "input",
        "Cached input" => "read",
        "Output" => "output",
        _ => return None,
    };
    Some((name, value?))
}

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "the finite, nonnegative value is bounded before conversion"
)]
fn price_micros(value: f32) -> Option<u64> {
    let value = f64::from(value);
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let rounded = format!("{value:.5e}").parse::<f64>().ok()?;
    let micros = (rounded * 1_000_000.0).round();
    (micros < 18_446_744_073_709_551_616.0).then_some(micros as u64)
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
    use hofund_plugin_sdk::protobuf::{ProtoBuf, deframe, fields};

    use super::{encode_request, model_uids, parse, parse_free, parse_prices, price_micros};

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

    #[test]
    fn goldens_parse_free_variants_and_six_significant_digit_prices() {
        let mut free: Vec<_> = parse_free(MODEL_CONFIGS).into_iter().collect();
        free.sort();
        assert_eq!(free, ["swe-2-high", "swe-2-max", "swe-2-medium"]);

        let prices = parse_prices(MODEL_CONFIGS_1015);
        assert_eq!(
            prices.get("swe-1-7-lightning"),
            Some(&super::Price {
                input: 2_500_000,
                output: 12_500_000,
                cache_read: 1_000_000,
            })
        );
        assert_eq!(
            prices.get("swe-2-high-lightning"),
            Some(&super::Price {
                input: 1_000_000,
                output: 5_000_000,
                cache_read: 100_000,
            })
        );
        assert_eq!(
            prices.get("glm-5-2"),
            Some(&super::Price {
                input: 1_400_000,
                output: 4_400_000,
                cache_read: 260_000,
            })
        );
        assert!(!prices.contains_key("swe-2-high"));
    }

    #[test]
    fn config_success_requires_a_family_and_output_only_prices_survive() {
        let adaptive = ProtoBuf::new().string(22, "adaptive").varint(5, 1).finish();
        let body = ProtoBuf::new().bytes(1, &adaptive).finish();
        assert!(parse(&body).is_none());

        let row = ProtoBuf::new()
            .string(1, "Output")
            .fixed32(2, 5.0_f32.to_bits())
            .string(3, "1M tokens")
            .finish();
        let config = ProtoBuf::new()
            .string(22, "output-only")
            .bytes(32, &row)
            .finish();
        let body = ProtoBuf::new().bytes(1, &config).finish();
        assert_eq!(
            parse_prices(&body).get("output-only"),
            Some(&super::Price {
                input: 0,
                output: 5_000_000,
                cache_read: 0,
            })
        );
    }

    #[test]
    fn prices_reject_nonfinite_negative_and_unrepresentable_values() {
        assert_eq!(price_micros(0.0), Some(0));
        assert_eq!(price_micros(-1.0), None);
        assert_eq!(price_micros(f32::NAN), None);
        assert_eq!(
            price_micros(18_446_744_073_709_551_616.0_f32),
            None,
            "2^64 dollars in micros does not fit u64"
        );
    }
}
