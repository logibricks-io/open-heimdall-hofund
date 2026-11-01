use std::collections::BTreeSet;
use std::time::{Instant, SystemTime};

use hofund_config::{Provider, ProviderKey};
use hofund_control_proto::{
    ProviderKeyAction, ProviderKeyRequest, ProviderKeyRest, ProviderKeysResponse,
};
use hofund_plugin_api::MemberId;
use hofund_router::Cooldowns;
use sha2::{Digest, Sha256};

use crate::DaemonError;
use crate::membership;

#[derive(Clone)]
pub(crate) struct KeyMember {
    pub provider: String,
    pub member: MemberId,
    pub weight: u64,
    pub rank: usize,
}

pub(crate) struct ResolvedKey {
    pub entry: ProviderKey,
    pub id: String,
}

pub(crate) fn key_id(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    digest[..5]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn masked(secret: &str) -> String {
    if secret.is_empty() {
        return String::new();
    }
    let characters = secret.chars().count();
    if characters >= 12 {
        let suffix: String = secret.chars().skip(characters - 4).collect();
        format!("…{suffix}")
    } else {
        "…".to_owned()
    }
}

pub(crate) fn split(text: &str) -> Vec<String> {
    let mut seen = BTreeSet::new();
    // Full-width comma and semicolon split too, as in the reference.
    text.split(|character: char| {
        character.is_whitespace() || matches!(character, ',' | ';' | '\u{ff0c}' | '\u{ff1b}')
    })
    .map(|key| key.trim_matches(['"', '\'', '`']))
    .filter(|key| !key.is_empty())
    .filter(|key| seen.insert((*key).to_owned()))
    .map(str::to_owned)
    .collect()
}

pub(crate) fn resolve(provider: &Provider) -> Result<Vec<ResolvedKey>, DaemonError> {
    let mut seen = BTreeSet::new();
    let mut resolved = Vec::new();
    for entry in provider.key_entries() {
        let secret = hofund_secrets::resolve(&entry.key).map_err(|error| {
            DaemonError::new(format!("provider `{}` key: {error}", provider.id))
        })?;
        if secret.is_empty() {
            continue;
        }
        let id = key_id(&secret);
        if seen.insert(id.clone()) {
            resolved.push(ResolvedKey { entry, id });
        }
    }
    Ok(resolved)
}

pub(crate) fn enabled(
    provider: &Provider,
    protocol: &str,
) -> Result<(bool, Vec<ResolvedKey>), DaemonError> {
    let resolved = resolve(provider)?;
    let enabled = resolved
        .into_iter()
        .filter(|key| key.entry.on)
        .collect::<Vec<_>>();
    let several = enabled.len() > 1;
    let suitable = enabled
        .into_iter()
        .filter(|key| {
            key.entry
                .protocol
                .as_deref()
                .is_none_or(|restricted| restricted == protocol)
        })
        .collect();
    Ok((several, suitable))
}

pub(crate) fn built_id(provider: &str, key: &str) -> String {
    format!("{provider}#{key}")
}

pub(crate) struct Mutation {
    pub text: String,
    pub changed: bool,
    pub moves: Vec<(MemberId, MemberId)>,
    pub added: u64,
    pub already_had: u64,
    pub removed: u64,
    pub before: String,
    pub after: String,
}

pub(crate) fn list(
    provider: &Provider,
    cooldowns: &Cooldowns,
) -> Result<ProviderKeysResponse, DaemonError> {
    let resolved = resolve(provider)?;
    let several = resolved.iter().filter(|key| key.entry.on).count() > 1;
    let rests = cooldowns.entries();
    let now = Instant::now();
    let wall = SystemTime::now();
    let mut response = ProviderKeysResponse::default();
    response.provider = provider.id.clone();
    for key in resolved {
        let secret = hofund_secrets::resolve(&key.entry.key).map_err(|error| {
            DaemonError::new(format!("provider `{}` key: {error}", provider.id))
        })?;
        let member = key_member(&provider.id, &key.id, several);
        let rest = rests
            .iter()
            .find(|(id, _)| *id == member)
            .map(|(_, cooling)| {
                let until = wall
                    .checked_add(cooling.until().saturating_duration_since(now))
                    .unwrap_or(SystemTime::UNIX_EPOCH);
                let mut rest = ProviderKeyRest::default();
                rest.why = cooling.cause().as_str().to_owned();
                rest.status = cooling.status();
                rest.until_ms = until
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .map_or(0, |duration| {
                        i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
                    });
                rest.probed = cooling.background_probe();
                rest
            });
        let mut listed = hofund_control_proto::ProviderKey::default();
        listed.id = key.id;
        listed.name = key.entry.name;
        listed.masked = masked(&secret);
        listed.on = key.entry.on;
        listed.protocol = key.entry.protocol.unwrap_or_default();
        listed.weight = key.entry.weight;
        listed.rest = rest;
        response.keys.push(listed);
    }
    Ok(response)
}

pub(crate) fn mutate(
    text: &str,
    provider_id: &str,
    request: &ProviderKeyRequest,
) -> Result<Mutation, String> {
    let config = hofund_config::load(text)
        .map_err(|_| "the configuration is not valid; nothing was changed".to_owned())?;
    let provider = config
        .providers
        .iter()
        .find(|provider| provider.id == provider_id)
        .ok_or_else(|| format!("unknown provider `{provider_id}`"))?;
    let mut entries = resolve(provider).map_err(|error| error.to_string())?;
    let several = entries.iter().filter(|key| key.entry.on).count() > 1;
    let find = |entries: &[ResolvedKey], id: &str| {
        entries
            .iter()
            .position(|key| key.id == id)
            .ok_or_else(|| format!("provider `{provider_id}` has no such key"))
    };
    let lone_before = (!several)
        .then(|| {
            entries
                .iter()
                .find(|key| key.entry.on)
                .map(|key| key.id.clone())
        })
        .flatten();
    let mut moves = Vec::new();
    let mut added = 0_u64;
    let mut already_had = 0_u64;
    let mut removed = 0_u64;
    let mut before = String::new();
    let mut after = String::new();
    let mut changed = true;
    match request.action {
        ProviderKeyAction::Add => {
            let submitted = request
                .keys
                .iter()
                .flat_map(|text| split(text))
                .collect::<Vec<_>>();
            if submitted.is_empty() {
                return Err("paste the keys to add".to_owned());
            }
            let protocol = key_protocol(&request.protocol)?;
            for spec in submitted {
                let secret = hofund_secrets::resolve(&spec)
                    .map_err(|error| format!("provider `{provider_id}` key: {error}"))?;
                if secret.is_empty() {
                    continue;
                }
                let id = key_id(&secret);
                if entries.iter().any(|key| key.id == id) {
                    already_had += 1;
                    continue;
                }
                entries.push(ResolvedKey {
                    entry: ProviderKey {
                        key: spec,
                        name: String::new(),
                        on: true,
                        protocol: protocol.clone(),
                        weight: 1,
                    },
                    id,
                });
                added += 1;
            }
        }
        ProviderKeyAction::Remove => {
            let index = find(&entries, &request.key_id)?;
            if index == 0 && entries.iter().filter(|key| key.entry.on).count() == 1 {
                return Err("that is the only key in use; turn another on first".to_owned());
            }
            entries.remove(index);
            if index == 0 {
                let next = entries
                    .iter()
                    .position(|key| key.entry.on)
                    .ok_or_else(|| "that is the only key in use; keep one on".to_owned())?;
                let promoted = entries.remove(next);
                entries.insert(0, promoted);
            }
        }
        ProviderKeyAction::RemoveMany => {
            let gone = request
                .keys
                .iter()
                .flat_map(|text| split(text))
                .collect::<BTreeSet<_>>();
            let first_gone = entries.first().is_some_and(|key| gone.contains(&key.id));
            let before_count = entries.len();
            entries.retain(|key| !gone.contains(&key.id));
            removed = u64::try_from(before_count - entries.len()).unwrap_or(u64::MAX);
            if removed == 0 {
                return Err(format!("provider `{provider_id}` has none of these keys"));
            }
            if first_gone {
                let next = entries
                    .iter()
                    .position(|key| key.entry.on)
                    .ok_or_else(|| "those are all the keys in use; keep one on".to_owned())?;
                let promoted = entries.remove(next);
                entries.insert(0, promoted);
            }
        }
        ProviderKeyAction::Weight => {
            if request.weight > hofund_config::MAX_KEY_WEIGHT {
                return Err(format!(
                    "a key's weight is 0 to {}",
                    hofund_config::MAX_KEY_WEIGHT
                ));
            }
            let index = find(&entries, &request.key_id)?;
            entries[index].entry.weight = request.weight.max(1);
        }
        ProviderKeyAction::Protocol => {
            let protocol = key_protocol(&request.protocol)?;
            let index = find(&entries, &request.key_id)?;
            entries[index].entry.protocol = protocol;
        }
        ProviderKeyAction::On => {
            let index = find(&entries, &request.key_id)?;
            if index == 0 || entries[index].entry.on {
                changed = false;
            } else {
                entries[index].entry.on = true;
            }
        }
        ProviderKeyAction::Off => {
            let index = find(&entries, &request.key_id)?;
            if index == 0 {
                if entries.iter().filter(|key| key.entry.on).count() == 1 {
                    return Err("that is the only key in use; turn another on first".to_owned());
                }
                entries[0].entry.on = false;
                let next = entries
                    .iter()
                    .position(|key| key.entry.on)
                    .expect("another enabled key");
                let promoted = entries.remove(next);
                entries.insert(0, promoted);
            } else if entries[index].entry.on {
                entries[index].entry.on = false;
            } else {
                changed = false;
            }
        }
        ProviderKeyAction::Use => {
            let index = find(&entries, &request.key_id)?;
            if index == 0 && entries[0].entry.on {
                changed = false;
            } else {
                let mut selected = entries.remove(index);
                selected.entry.on = true;
                entries.insert(0, selected);
            }
        }
        ProviderKeyAction::Rename => {
            let index = find(&entries, &request.key_id)?;
            entries[index].entry.name = request.name.trim().to_owned();
        }
        ProviderKeyAction::Replace => {
            let index = find(&entries, &request.key_id)?;
            let submitted = request
                .keys
                .iter()
                .flat_map(|text| split(text))
                .collect::<Vec<_>>();
            if submitted.len() != 1 {
                return Err("paste exactly one replacement key".to_owned());
            }
            let secret = hofund_secrets::resolve(&submitted[0])
                .map_err(|error| format!("provider `{provider_id}` key: {error}"))?;
            if secret.is_empty() {
                return Err("paste the key's new value".to_owned());
            }
            let now = key_id(&secret);
            before.clone_from(&request.key_id);
            if now == request.key_id {
                after = now;
                changed = false;
            } else {
                if entries.iter().any(|key| key.id == now) {
                    return Err(format!("provider `{provider_id}` already has this key"));
                }
                let old = key_member(provider_id, &request.key_id, several);
                entries[index].entry.key.clone_from(&submitted[0]);
                entries[index].id.clone_from(&now);
                let new = key_member(provider_id, &now, several);
                moves.push((old, new));
                after = now;
            }
        }
        _ => return Err("unknown provider-key action".to_owned()),
    }
    if added == 0 && already_had > 0 && matches!(request.action, ProviderKeyAction::Add) {
        changed = false;
    }
    if !changed {
        return Ok(Mutation {
            text: text.to_owned(),
            changed,
            moves,
            added,
            already_had,
            removed,
            before,
            after,
        });
    }
    if entries.is_empty() {
        return Err("a provider must keep one key".to_owned());
    }
    entries[0].entry.on = true;
    // A key keeps its rests when the provider goes from one key in use to
    // several, or back: its member id changes between the provider's and
    // its own.
    let several_after = entries.iter().filter(|key| key.entry.on).count() > 1;
    if several != several_after {
        let lone = if several_after {
            lone_before.filter(|id| entries.iter().any(|key| key.id == *id && key.entry.on))
        } else {
            entries
                .iter()
                .find(|key| key.entry.on)
                .map(|key| key.id.clone())
        };
        if let Some(id) = lone {
            moves.push((
                key_member(provider_id, &id, several),
                key_member(provider_id, &id, several_after),
            ));
        }
    }
    let entries = entries.into_iter().map(|key| key.entry).collect::<Vec<_>>();
    let text = hofund_config::edit_provider_keys(text, provider_id, &entries)
        .map_err(|error| error.to_string())?;
    Ok(Mutation {
        text,
        changed,
        moves,
        added,
        already_had,
        removed,
        before,
        after,
    })
}

fn key_protocol(protocol: &str) -> Result<Option<String>, String> {
    let protocol = protocol.trim();
    if protocol.is_empty() || protocol == "any" {
        return Ok(None);
    }
    if hofund_config::KEY_PROTOCOLS.contains(&protocol) {
        Ok(Some(protocol.to_owned()))
    } else {
        Err(format!(
            "a key's protocol is one of {} or any",
            hofund_config::KEY_PROTOCOLS.join(", ")
        ))
    }
}

fn key_member(provider: &str, key: &str, several: bool) -> MemberId {
    if several {
        membership::key_member_id(provider, key)
    } else {
        membership::member_id(provider)
    }
}

#[cfg(test)]
mod tests {
    use hofund_config::load;

    use super::{key_id, masked, split};

    #[test]
    fn k2_fingerprint_is_ten_hex_and_mask_holds_only_the_suffix() {
        assert_eq!(key_id("first-provider-key"), "1ce17fffce");
        assert_eq!(masked("first-provider-key"), "…-key");
        assert_eq!(masked("short"), "…");
        assert_eq!(masked(""), "");
        // A multi-byte suffix is cut on a character, never inside one.
        assert_eq!(
            masked("abcdefgh-\u{e9}\u{e9}\u{e9}\u{e9}"),
            "…\u{e9}\u{e9}\u{e9}\u{e9}"
        );
    }

    #[test]
    fn k6a_split_keys_accepts_lines_space_commas_and_semicolons() {
        assert_eq!(
            split("one\ntwo, three; \"four\" `five` one\n"),
            ["one", "two", "three", "four", "five"]
        );
        assert!(split(" \n,; ").is_empty());
        assert_eq!(
            split("six\u{ff0c}seven\u{ff1b}eight\u{3000}nine"),
            ["six", "seven", "eight", "nine"]
        );
    }

    #[test]
    fn k3a_empty_and_resolved_duplicate_keys_are_one_candidate() {
        let config = load(
            "[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"same\"\n[[providers.keys]]\nkey = \"same\"\n[[providers.keys]]\nkey = \"\"\n",
        )
        .expect("config");
        let (several, keys) = super::enabled(&config.providers[0], "openai-chat").expect("keys");
        assert!(!several);
        assert_eq!(keys.len(), 1);

        // Two different specs that resolve to the same secret are one key.
        let name = std::env::var("CARGO_PKG_NAME").expect("cargo sets the package name");
        let config = load(&format!(
            "[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"env:CARGO_PKG_NAME\"\n[[providers.keys]]\nkey = \"{name}\"\n"
        ))
        .expect("config");
        assert_eq!(super::resolve(&config.providers[0]).expect("keys").len(), 1);
    }

    fn change(
        text: &str,
        action: hofund_control_proto::ProviderKeyAction,
        id: &str,
        keys: &[&str],
    ) -> super::Mutation {
        let mut request = hofund_control_proto::ProviderKeyRequest::new(action).for_id(id);
        request.keys = keys.iter().map(|key| (*key).to_owned()).collect();
        super::mutate(text, "p", &request).expect("mutation")
    }

    #[test]
    fn k6_operations_keep_order_attributes_and_replacement_state() {
        let original = "[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"first\"\n[[providers.keys]]\nkey = \"second\"\nname = \"Team\"\nweight = 2\n";
        let added = change(
            original,
            hofund_control_proto::ProviderKeyAction::Add,
            "",
            &["third,fourth third first"],
        );
        assert_eq!(added.added, 2);
        assert_eq!(added.already_had, 1);
        assert_eq!(
            load(&added.text).expect("added").providers[0]
                .key_entries()
                .len(),
            4
        );

        let second = key_id("second");
        let mut rename = hofund_control_proto::ProviderKeyRequest::new(
            hofund_control_proto::ProviderKeyAction::Rename,
        )
        .for_id(&second);
        rename.name = "Renamed".to_owned();
        let renamed = super::mutate(&added.text, "p", &rename).expect("rename");
        let used = change(
            &renamed.text,
            hofund_control_proto::ProviderKeyAction::Use,
            &second,
            &[],
        );
        let provider = &load(&used.text).expect("used").providers[0];
        assert_eq!(provider.api_key, "second");
        assert_eq!(provider.key_name, "Renamed");
        assert_eq!(provider.key_weight, 2);

        let off = change(
            &used.text,
            hofund_control_proto::ProviderKeyAction::Off,
            &second,
            &[],
        );
        let provider = &load(&off.text).expect("off").providers[0];
        assert_eq!(provider.api_key, "first");
        assert!(!provider.keys[0].on);
        change(
            &off.text,
            hofund_control_proto::ProviderKeyAction::On,
            &second,
            &[],
        );
        let removed = change(
            &off.text,
            hofund_control_proto::ProviderKeyAction::Remove,
            &key_id("first"),
            &[],
        );
        let provider = &load(&removed.text).expect("removed").providers[0];
        assert_eq!(provider.api_key, "third");
        assert!(
            provider
                .keys
                .iter()
                .any(|key| key.key == "second" && !key.on)
        );

        let replaced = change(
            &removed.text,
            hofund_control_proto::ProviderKeyAction::Replace,
            &key_id("third"),
            &["new-third"],
        );
        assert_eq!(replaced.before, key_id("third"));
        assert_eq!(replaced.after, key_id("new-third"));
        assert_eq!(replaced.moves.len(), 1);
        let provider = &load(&replaced.text).expect("replaced").providers[0];
        assert_eq!(provider.api_key, "new-third");
    }

    #[test]
    fn k6_last_enabled_key_stays_on_and_present() {
        let original = "[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"only\"\n";
        for action in [
            hofund_control_proto::ProviderKeyAction::Off,
            hofund_control_proto::ProviderKeyAction::Remove,
        ] {
            let mut request =
                hofund_control_proto::ProviderKeyRequest::new(action).for_id(key_id("only"));
            request.keys.clear();
            let refused = super::mutate(original, "p", &request).err();
            if action == hofund_control_proto::ProviderKeyAction::Remove {
                assert_eq!(
                    refused.as_deref(),
                    Some("that is the only key in use; turn another on first")
                );
            } else {
                assert!(refused.is_some());
            }
        }

        // The only enabled key heads the list; another key that is off does not save it.
        let with_off = "[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"only\"\n[[providers.keys]]\nkey = \"spare\"\non = false\n";
        let request = hofund_control_proto::ProviderKeyRequest::new(
            hofund_control_proto::ProviderKeyAction::Remove,
        )
        .for_id(key_id("only"));
        assert_eq!(
            super::mutate(with_off, "p", &request).err().as_deref(),
            Some("that is the only key in use; turn another on first")
        );
    }

    #[test]
    fn k6e_list_shows_a_single_enabled_key_with_the_provider_rest() {
        let config = load(
            "[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"only\"\n[[providers.keys]]\nkey = \"spare\"\non = false\n",
        )
        .expect("config");
        let cooldowns = hofund_router::Cooldowns::new(hofund_router::CooldownPolicy::default());
        cooldowns.record_panic(&crate::membership::member_id("p"));
        let listed = super::list(&config.providers[0], &cooldowns).expect("list");
        assert_eq!(listed.keys.len(), 2);
        assert!(listed.keys[0].rest.is_some());
    }

    #[test]
    fn k6_mutation_debug_never_prints_key_material() {
        let mut request = hofund_control_proto::ProviderKeyRequest::new(
            hofund_control_proto::ProviderKeyAction::Replace,
        )
        .for_id("0123456789");
        request.keys = vec!["replacement-secret".to_owned()];
        let debug = format!("{request:?}");
        assert!(!debug.contains("replacement-secret"), "{debug}");
        assert!(debug.contains("keys: 1"), "{debug}");
    }

    #[test]
    fn k6b_bulk_remove_promotes_the_next_enabled_key_and_keeps_one_on() {
        let original = "[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"first\"\n[[providers.keys]]\nkey = \"second\"\non = false\n[[providers.keys]]\nkey = \"third\"\n[[providers.keys]]\nkey = \"fourth\"\n";
        let gone = format!("{} {} 0000000000", key_id("first"), key_id("fourth"));
        let removed = change(
            original,
            hofund_control_proto::ProviderKeyAction::RemoveMany,
            "",
            &[gone.as_str()],
        );
        assert_eq!(removed.removed, 2);
        let provider = &load(&removed.text).expect("removed").providers[0];
        assert_eq!(provider.api_key, "third");
        assert_eq!(provider.keys.len(), 1);
        assert!(provider.keys[0].key == "second" && !provider.keys[0].on);

        let first_only = change(
            original,
            hofund_control_proto::ProviderKeyAction::RemoveMany,
            "",
            &[key_id("first").as_str()],
        );
        let provider = &load(&first_only.text).expect("first gone").providers[0];
        assert_eq!(provider.api_key, "third");

        let mut none = hofund_control_proto::ProviderKeyRequest::new(
            hofund_control_proto::ProviderKeyAction::RemoveMany,
        );
        none.keys = vec!["0000000000".to_owned()];
        assert_eq!(
            super::mutate(original, "p", &none).err().as_deref(),
            Some("provider `p` has none of these keys")
        );
        let mut every_on = none.clone();
        every_on.keys = vec![format!(
            "{},{},{}",
            key_id("first"),
            key_id("third"),
            key_id("fourth")
        )];
        assert_eq!(
            super::mutate(original, "p", &every_on).err().as_deref(),
            Some("those are all the keys in use; keep one on")
        );
    }

    #[test]
    fn k6d_a_key_keeps_its_rests_when_the_provider_gains_or_loses_keys_in_use() {
        let single = "[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"first\"\n";
        let added = change(
            single,
            hofund_control_proto::ProviderKeyAction::Add,
            "",
            &["second"],
        );
        let provider = crate::membership::member_id("p");
        let first = crate::membership::key_member_id("p", &key_id("first"));
        assert_eq!(added.moves, vec![(provider.clone(), first.clone())]);

        let removed = change(
            &added.text,
            hofund_control_proto::ProviderKeyAction::Remove,
            &key_id("second"),
            &[],
        );
        assert_eq!(removed.moves, vec![(first.clone(), provider.clone())]);

        let off = change(
            &added.text,
            hofund_control_proto::ProviderKeyAction::Off,
            &key_id("first"),
            &[],
        );
        let second = crate::membership::key_member_id("p", &key_id("second"));
        assert_eq!(off.moves, vec![(second, provider)]);

        let renamed = change(
            &added.text,
            hofund_control_proto::ProviderKeyAction::Rename,
            &key_id("first"),
            &[],
        );
        assert!(renamed.moves.is_empty());
    }

    #[test]
    fn k6c_weight_and_protocol_are_set_per_key_and_bounded() {
        let original = "[[providers]]\nid = \"p\"\npreset = \"openai\"\napi_key = \"first\"\n[[providers.keys]]\nkey = \"second\"\n";
        let mut weight = hofund_control_proto::ProviderKeyRequest::new(
            hofund_control_proto::ProviderKeyAction::Weight,
        )
        .for_id(key_id("second"));
        weight.weight = 1000;
        let heavy = super::mutate(original, "p", &weight).expect("weight");
        assert_eq!(
            load(&heavy.text).expect("heavy").providers[0].keys[0].weight,
            1000
        );
        weight.weight = 0;
        let reset = super::mutate(&heavy.text, "p", &weight).expect("reset");
        assert_eq!(
            load(&reset.text).expect("reset").providers[0].keys[0].weight,
            1
        );
        assert!(!reset.text.contains("weight"), "{}", reset.text);
        weight.weight = 1001;
        assert_eq!(
            super::mutate(original, "p", &weight).err().as_deref(),
            Some("a key's weight is 0 to 1000")
        );

        let mut protocol = hofund_control_proto::ProviderKeyRequest::new(
            hofund_control_proto::ProviderKeyAction::Protocol,
        )
        .for_id(key_id("first"));
        protocol.protocol = "openai-responses".to_owned();
        let narrowed = super::mutate(original, "p", &protocol).expect("protocol");
        let provider = &load(&narrowed.text).expect("narrowed").providers[0];
        assert_eq!(provider.key_protocol.as_deref(), Some("openai-responses"));
        protocol.protocol = "any".to_owned();
        let cleared = super::mutate(&narrowed.text, "p", &protocol).expect("any");
        assert_eq!(
            load(&cleared.text).expect("cleared").providers[0].key_protocol,
            None
        );
        protocol.protocol = "smtp".to_owned();
        assert!(
            super::mutate(original, "p", &protocol)
                .err()
                .is_some_and(|message| message.starts_with("a key's protocol is one of"))
        );

        let mut add = hofund_control_proto::ProviderKeyRequest::new(
            hofund_control_proto::ProviderKeyAction::Add,
        );
        add.keys = vec!["third fourth".to_owned()];
        add.protocol = "anthropic-messages".to_owned();
        let added = super::mutate(original, "p", &add).expect("add");
        let provider = &load(&added.text).expect("added").providers[0];
        assert_eq!(provider.keys.len(), 3);
        assert_eq!(provider.keys[0].protocol, None);
        assert!(
            provider.keys[1..]
                .iter()
                .all(|key| key.protocol.as_deref() == Some("anthropic-messages"))
        );
    }
}
