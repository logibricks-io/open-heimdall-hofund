//! Read `/client/users/me` and pick an organization with a paid plan.

use hofund_plugin_api::PluginError;
use hofund_plugin_sdk::json::Value;

use crate::creds::Credential;

#[must_use]
pub(crate) fn paid(plan: &str) -> bool {
    !plan.is_empty() && plan != "zed_free"
}

#[must_use]
pub(crate) fn plan_name(id: &str) -> String {
    match id {
        "zed_pro" => "Pro".to_owned(),
        "zed_pro_trial" => "Pro Trial".to_owned(),
        "zed_business" => "Business".to_owned(),
        "zed_student" => "Student".to_owned(),
        "zed_vip" => "VIP".to_owned(),
        "zed_free" | "" => "Free".to_owned(),
        other => other.strip_prefix("zed_").unwrap_or(other).to_owned(),
    }
}

fn models_off(me: &Value, org: &str) -> bool {
    me.get("configuration_by_organization")
        .and_then(Value::as_object)
        .and_then(|map| map.get(org))
        .and_then(Value::as_object)
        .and_then(|cfg| cfg.get("is_zed_model_provider_enabled"))
        .and_then(Value::as_bool)
        == Some(false)
}

fn plan_for(me: &Value, org: &str) -> String {
    me.get("plans_by_organization")
        .and_then(Value::as_object)
        .and_then(|map| map.get(org))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

fn usable(me: &Value, org: &Value) -> bool {
    let Some(id) = org.get("id").and_then(Value::as_str) else {
        return false;
    };
    paid(&plan_for(me, id)) && !models_off(me, id)
}

/// Organization id models should be asked under, matching the reference plugin.
#[must_use]
pub(crate) fn org_of(me: &Value, saved: &str) -> String {
    let orgs: Vec<&Value> = me
        .get("organizations")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|o| o.get("id").and_then(Value::as_str).is_some())
                .collect()
        })
        .unwrap_or_default();
    let keep = orgs
        .iter()
        .find(|o| o.get("id").and_then(Value::as_str) == Some(saved));
    let def = orgs.iter().find(|o| {
        o.get("id").and_then(Value::as_str)
            == me.get("default_organization_id").and_then(Value::as_str)
    });
    for candidate in [keep, def].into_iter().flatten() {
        if usable(me, candidate) {
            return candidate
                .get("id")
                .and_then(Value::as_str)
                .expect("id")
                .to_owned();
        }
    }
    if let Some(org) = orgs
        .iter()
        .find(|o| o.get("is_personal").and_then(Value::as_bool) == Some(true) && usable(me, o))
    {
        return org
            .get("id")
            .and_then(Value::as_str)
            .expect("id")
            .to_owned();
    }
    if let Some(org) = orgs.iter().find(|o| usable(me, o)) {
        return org
            .get("id")
            .and_then(Value::as_str)
            .expect("id")
            .to_owned();
    }
    if let Some(org) = keep
        .copied()
        .or(def.copied())
        .or_else(|| orgs.first().copied())
    {
        return org
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or(saved)
            .to_owned();
    }
    if !saved.is_empty() {
        return saved.to_owned();
    }
    me.get("default_organization_id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

#[must_use]
pub(crate) fn plan_of(me: &Value, org: &str) -> String {
    let known = !org.is_empty()
        && me
            .get("organizations")
            .and_then(Value::as_array)
            .is_some_and(|orgs| {
                orgs.iter()
                    .any(|o| o.get("id").and_then(Value::as_str) == Some(org))
            })
        && me
            .get("plans_by_organization")
            .and_then(Value::as_object)
            .is_some();
    if known {
        return plan_for(me, org);
    }
    me.get("plan")
        .and_then(|p| p.get("plan_v3"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

pub(crate) fn apply_org(credential: &mut Credential, me: &Value) {
    let org = org_of(me, &credential.org);
    credential.org = org.clone();
    credential.plan = plan_of(me, &org);
}

pub(crate) fn parse_me(body: &[u8]) -> Result<Value, PluginError> {
    let me: Value = hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("Zed account JSON is invalid"))?;
    if me.is_null() {
        return Ok(hofund_plugin_sdk::json::json!({}));
    }
    if !me.is_object() {
        return Err(PluginError::decode("Zed account JSON is unreadable"));
    }
    Ok(me)
}
