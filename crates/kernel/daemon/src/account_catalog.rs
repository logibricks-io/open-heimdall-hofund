//! Accounts and keys the gateway may list or pin. No secret is copied out.

use hofund_config::Config;
use hofund_router::account_pin::AccountFace;

use crate::membership;
use crate::provider_keys;
use crate::subscriptions::Subscriptions;

/// Every provider key and the subscription accounts last resolved.
///
/// A key's id is its fingerprint. The secret stays in the config.
pub(crate) fn faces(config: &Config, subscriptions: &Subscriptions) -> Vec<AccountFace> {
    let mut faces = Vec::new();
    for provider in &config.providers {
        let Ok(keys) = provider_keys::resolve(provider) else {
            continue;
        };
        let several = keys.iter().filter(|key| key.entry.on).count() > 1;
        for key in keys {
            let member = if key.entry.on && !several {
                membership::member_id(&provider.id)
            } else {
                membership::try_key_member_id(&provider.id, &key.id)
                    .unwrap_or_else(|| membership::member_id(&provider.id))
            };
            faces.push(
                AccountFace::key(
                    provider.id.clone(),
                    key.id,
                    key.entry.name,
                    key.entry.plan.unwrap_or_default(),
                    key.entry.on && provider.enabled,
                )
                .with_provider_name(provider.name.clone())
                .with_member(member.to_string()),
            );
        }
    }
    for state in subscriptions.values() {
        faces.extend(state.listed());
    }
    faces
}

#[cfg(test)]
mod tests {
    use super::faces;
    use crate::provider_keys::key_id;
    use crate::subscriptions::Subscriptions;
    use hofund_config::load;

    #[test]
    fn d243_faces_list_keys_without_the_secret() {
        let secret = "sk-live-secret-one";
        let spare = "sk-live-secret-two";
        let config = load(&format!(
            "listen = \"127.0.0.1:0\"\n\
             [[providers]]\n\
             id = \"keyed\"\n\
             name = \"Keyed\"\n\
             preset = \"relay\"\n\
             base_url = \"http://127.0.0.1:9\"\n\
             api_key = \"{secret}\"\n\
             key_name = \"work\"\n\
             key_plan = \"api\"\n\
             [[providers.keys]]\n\
             key = \"{spare}\"\n\
             name = \"spare\"\n\
             on = false\n\
             [[routes]]\n\
             model = \"demo\"\n\
             upstreams = [\"keyed\"]\n"
        ))
        .expect("config");
        let listed = faces(&config, &Subscriptions::new());
        assert_eq!(listed.len(), 2);
        let work = listed
            .iter()
            .find(|face| face.label == "work")
            .expect("work");
        let off = listed
            .iter()
            .find(|face| face.label == "spare")
            .expect("spare");
        assert_eq!(work.id, key_id(secret));
        assert_eq!(off.id, key_id(spare));
        assert!(work.on);
        assert!(!off.on);
        assert_eq!(work.plan, "api");
        assert_eq!(work.provider_name, "Keyed");
        assert_eq!(work.pin(), format!("keyed/{}", work.id));
        let rendered = format!("{listed:?}");
        assert!(!rendered.contains(secret));
        assert!(!rendered.contains(spare));
    }
}
