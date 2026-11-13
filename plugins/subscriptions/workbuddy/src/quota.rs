//! Billing quota from WorkBuddy profile usage meters.

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hofund_plugin_api::{PluginError, QuotaWindow};
use hofund_plugin_sdk::json::{Value, json};
use hofund_plugin_sdk::time;

use crate::site::Site;

const PERCENT: u64 = 100;
const PAGE_SIZE: usize = 200;
const MAX_PAGES: u32 = 10;
const SLICE: &str = "4";
const FREE_MON: &str = "TCACA_code_008_cfWoLwvjU4";
const PAID_CODES: &[&str] = &[
    "TCACA_code_002_AkiJS3ZHF5",
    "TCACA_code_005_maRGyrHhw1",
    "TCACA_code_003_FAnt7lcmRT",
    "TCACA_code_023_4xbGhMrE6q",
    "TCACA_code_026_BaESVICNoi",
    "TCACA_code_027_0FCGVA6vSa",
    "TCACA_code_043_6oIkxT7Z0S",
    "TCACA_code_009_0XmEQc2xOf",
    "TCACA_code_038_OhvqZtiPKr",
    "TCACA_code_036_lupO5WgNdG",
];
const FREE_CODES: &[&str] = &[
    "TCACA_code_001_PqouKr6QWV",
    FREE_MON,
    "TCACA_code_035_ArVxJcGDsm",
    "TCACA_code_006_DbXS0lrypC",
    "TCACA_code_039_KRcQj7wUat",
    "TCACA_code_040_mi9rCYg46x",
    "TCACA_code_007_nzdH5h4Nl0",
    "TCACA_code_028_NtpWi0jzXs",
    "TCACA_code_037_WxOD3MpI2o",
    "TCACA_code_029_6wCGEWquYy",
    "TCACA_code_030_BjSt89qTvr",
];
const PLAN_CODES: &[&str] = &[
    "TCACA_code_002_AkiJS3ZHF5",
    "TCACA_code_005_maRGyrHhw1",
    "TCACA_code_003_FAnt7lcmRT",
    "TCACA_code_023_4xbGhMrE6q",
    "TCACA_code_026_BaESVICNoi",
    "TCACA_code_027_0FCGVA6vSa",
    "TCACA_code_043_6oIkxT7Z0S",
    "TCACA_code_035_ArVxJcGDsm",
    "TCACA_code_039_KRcQj7wUat",
    "TCACA_code_040_mi9rCYg46x",
];
const EXTRA_CODES: &[&str] = &[
    "TCACA_code_028_NtpWi0jzXs",
    "TCACA_code_037_WxOD3MpI2o",
    "TCACA_code_009_0XmEQc2xOf",
    "TCACA_code_038_OhvqZtiPKr",
    "TCACA_code_036_lupO5WgNdG",
    "TCACA_code_007_nzdH5h4Nl0",
    "TCACA_code_029_6wCGEWquYy",
    "TCACA_code_030_BjSt89qTvr",
];
const EXTRA_AI: &[&str] = &[FREE_MON, "TCACA_code_006_DbXS0lrypC"];

#[derive(Clone, Copy, Debug, Default)]
struct Credits {
    total: f64,
    used: f64,
}

#[derive(Clone, Copy, Debug, Default)]
struct Kinds {
    plan: Credits,
    extra: Credits,
    left: f64,
    bags: u64,
}

pub(crate) struct Summary {
    pub(crate) windows: Vec<QuotaWindow>,
    data: Value,
    kinds: Kinds,
}

impl Summary {
    pub(crate) fn has_credits(&self) -> bool {
        self.windows.iter().any(|window| window.name() == "Credits")
    }

    pub(crate) fn apply_packages(
        &mut self,
        packages: &[Value],
        now: SystemTime,
        site: Site,
    ) -> Result<(), PluginError> {
        if let Some(reset) = cycle_of(&self.data, packages, now)
            && let Some(index) = self
                .windows
                .iter()
                .position(|window| window.name() == "Credits")
        {
            self.windows[index] = with_reset(&self.windows[index], reset)?;
        }
        let already_split = self
            .windows
            .iter()
            .any(|window| window.name() == "Extra credits");
        if !already_split
            && self.kinds.plan.total > 0.0
            && self.kinds.extra.total == 0.0
            && let Some(extra) = listed_extra(&self.data, packages, site, now)
        {
            let window = credit_window("Extra credits", extra)?.with_blocking(false);
            let index = self
                .windows
                .iter()
                .position(|window| window.name() == "Credits")
                .map_or(self.windows.len(), |index| index + 1);
            self.windows.insert(index, window);
        }
        Ok(())
    }
}

pub(crate) fn summary(body: &[u8], site: Site) -> Result<Summary, PluginError> {
    let root = decode(body)?;
    ensure_success(&root)?;
    let data = data(&root).clone();
    let kinds = kinds_of(&data, site)?;
    let mut windows = usage_of(kinds)?;
    if kinds.bags > 0 && (kinds.plan.total + kinds.extra.total == 0.0 || kinds.left > 0.0) {
        windows.push(
            QuotaWindow::new(
                format!("Prepaid credits: {} credits", compact(kinds.left)),
                0,
                None,
                None,
            )?
            .with_blocking(false),
        );
    }
    Ok(Summary {
        windows,
        data,
        kinds,
    })
}

pub(crate) fn enterprise(body: &[u8]) -> Result<Option<QuotaWindow>, PluginError> {
    let root = decode(body)?;
    ensure_success(&root)?;
    let data = data(&root);
    let limit = capacity(data.get("limitNum"))?;
    if limit <= 0.0 {
        return Ok(None);
    }
    let used = capacity(data.get("credit"))?;
    let reset = data
        .get("cycleResetTime")
        .and_then(Value::as_str)
        .and_then(beijing);
    let window = credit_window("Credits", Credits { total: limit, used })?;
    match reset {
        Some(reset) => Ok(Some(with_reset(&window, reset)?)),
        None => Ok(Some(window)),
    }
}

pub(crate) fn paid_package_body(page: u32) -> Vec<u8> {
    json!({
        "PageNumber": page,
        "PageSize": PAGE_SIZE,
        "Status": [0, 3],
        "PackageCodes": PAID_CODES,
        "NeedRenewInfo": true,
    })
    .to_string()
    .into_bytes()
}

pub(crate) fn free_package_body(page: u32, now: SystemTime) -> Vec<u8> {
    let day = now
        .checked_add(Duration::from_secs(8 * 3_600))
        .map(time::format_rfc3339)
        .unwrap_or_default();
    let day = day.get(..10).unwrap_or("1970-01-01");
    json!({
        "PageNumber": page,
        "PageSize": PAGE_SIZE,
        "Status": [0, 3],
        "PackageCodes": FREE_CODES,
        "SlicePeriodStartTime": format!("{day} 00:00:00"),
        "SlicePeriodEndTime": format!("{day} 23:59:59"),
    })
    .to_string()
    .into_bytes()
}

pub(crate) struct PackagePage {
    pub(crate) accounts: Vec<Value>,
    pub(crate) total_count: usize,
}

pub(crate) fn package_page(body: &[u8]) -> Result<PackagePage, PluginError> {
    let root = decode(body)?;
    ensure_success(&root)?;
    let data = data(&root);
    Ok(PackagePage {
        accounts: data
            .get("Accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        total_count: data
            .get("TotalCount")
            .and_then(Value::as_u64)
            .and_then(|count| usize::try_from(count).ok())
            .unwrap_or(0),
    })
}

pub(crate) const fn package_page_limit() -> u32 {
    MAX_PAGES
}

fn usage_of(kinds: Kinds) -> Result<Vec<QuotaWindow>, PluginError> {
    if kinds.plan.total > 0.0 && kinds.extra.total > 0.0 {
        let credits_blocking =
            kinds.plan.used < kinds.plan.total || kinds.extra.used >= kinds.extra.total;
        return Ok(vec![
            credit_window("Credits", kinds.plan)?.with_blocking(credits_blocking),
            credit_window("Extra credits", kinds.extra)?.with_blocking(!credits_blocking),
        ]);
    }
    let combined = Credits {
        total: kinds.plan.total + kinds.extra.total,
        used: kinds.plan.used + kinds.extra.used,
    };
    if combined.total > 0.0 {
        return Ok(vec![credit_window("Credits", combined)?]);
    }
    Ok(Vec::new())
}

fn kinds_of(sum: &Value, site: Site) -> Result<Kinds, PluginError> {
    let mut kinds = Kinds::default();
    let groups: Vec<&Value> = sum
        .get("CreditGroups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|group| has_total(group))
        .collect();
    for group in groups.iter().copied() {
        add_credits(
            if group.get("Kind").and_then(Value::as_str) == Some("plan_base") {
                &mut kinds.plan
            } else {
                &mut kinds.extra
            },
            group,
        )?;
    }
    for package in packages(sum) {
        if has_total(package) {
            if groups.is_empty() {
                add_credits(
                    if of_plan(sum, package, site) {
                        &mut kinds.plan
                    } else {
                        &mut kinds.extra
                    },
                    package,
                )?;
            }
        } else if package
            .get("CycleRemainCapacity")
            .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
        {
            kinds.left += capacity(package.get("CycleRemainCapacity"))?;
            kinds.bags += 1;
        }
    }
    Ok(kinds)
}

fn add_credits(kind: &mut Credits, package: &Value) -> Result<(), PluginError> {
    kind.total += capacity(package.get("CycleTotalCapacity"))?;
    kind.used += capacity(package.get("CycleUsedCapacity"))?;
    Ok(())
}

fn credit_window(name: &str, credits: Credits) -> Result<QuotaWindow, PluginError> {
    let percent = if credits.total <= 0.0 {
        0
    } else {
        ((PERCENT as f64 * credits.used / credits.total)
            .round()
            .clamp(0.0, 100.0)) as u64
    };
    QuotaWindow::new(name, percent, Some(PERCENT), None)
}

fn with_reset(window: &QuotaWindow, reset: SystemTime) -> Result<QuotaWindow, PluginError> {
    Ok(
        QuotaWindow::new(window.name(), window.used(), window.limit(), Some(reset))?
            .with_blocking(window.blocking()),
    )
}

fn of_plan(sum: &Value, package: &Value, site: Site) -> bool {
    let Some(code) = package.get("PackageCode").and_then(Value::as_str) else {
        return false;
    };
    !code.is_empty()
        && (sum.get("SubscriptionPackageCode").and_then(Value::as_str) == Some(code)
            || PLAN_CODES.contains(&code)
            || (site == Site::China && code == FREE_MON))
}

fn cycle_of(sum: &Value, packages: &[Value], now: SystemTime) -> Option<SystemTime> {
    let subscription = sum
        .get("SubscriptionPackageCode")
        .and_then(Value::as_str)
        .unwrap_or("");
    let package = if subscription.is_empty() {
        packages.iter().find(|package| is_slice(package))
    } else {
        packages.iter().find(|package| {
            package.get("PackageCode").and_then(Value::as_str) == Some(subscription)
        })
    }?;
    let end = package
        .get("CycleEndTime")
        .and_then(Value::as_str)
        .and_then(beijing)?;
    (end > now).then_some(end)
}

fn listed_extra(sum: &Value, listed: &[Value], site: Site, now: SystemTime) -> Option<Credits> {
    let summed: HashSet<&str> = packages(sum)
        .iter()
        .filter_map(|package| package.get("PackageCode").and_then(Value::as_str))
        .collect();
    let mut credits = Credits::default();
    for package in listed {
        let Some(code) = package.get("PackageCode").and_then(Value::as_str) else {
            continue;
        };
        let listed = EXTRA_CODES.contains(&code)
            || (site == Site::International && EXTRA_AI.contains(&code));
        if !listed || summed.contains(code) {
            continue;
        }
        if end_of(package).is_some_and(|end| end <= now) {
            continue;
        }
        let slice = if is_slice(package) {
            package
                .get("SlicePeriodUsageDetails")
                .and_then(Value::as_array)
                .and_then(|details| details.first())
        } else {
            None
        };
        let source = slice.unwrap_or(package);
        let (total, left) = if slice.is_some() {
            (
                positive(source.get("SlicePeriodCapacitySizePrecise")),
                positive(source.get("SlicePeriodCapacityRemainPrecise")),
            )
        } else {
            (
                positive(source.get("CycleCapacitySizePrecise")),
                positive(source.get("CycleCapacityRemainPrecise")),
            )
        };
        if total > 0.0 {
            credits.total += total;
            credits.used += total - left.min(total);
        }
    }
    (credits.total > 0.0).then_some(credits)
}

fn is_slice(package: &Value) -> bool {
    package
        .get("CapacityType")
        .is_some_and(|value| value.as_str() == Some(SLICE) || value.as_i64() == Some(4))
}

fn end_of(package: &Value) -> Option<SystemTime> {
    if let Some(end) = package.get("DeductionEndTime").and_then(milliseconds) {
        return Some(end);
    }
    ["ExpiredTime", "CycleEndTime"]
        .into_iter()
        .filter_map(|key| package.get(key).and_then(Value::as_str))
        .find_map(|text| beijing(text).or_else(|| time::parse_rfc3339(text)))
}

fn milliseconds(value: &Value) -> Option<SystemTime> {
    let milliseconds = value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())?;
    if !milliseconds.is_finite() || milliseconds <= 0.0 || milliseconds > u64::MAX as f64 {
        return None;
    }
    UNIX_EPOCH.checked_add(Duration::from_millis(milliseconds as u64))
}

fn beijing(text: &str) -> Option<SystemTime> {
    let text = text.trim();
    if text.len() != 19 {
        return None;
    }
    time::parse_rfc3339(&format!("{}+08:00", text.replace(' ', "T")))
}

fn positive(value: Option<&Value>) -> f64 {
    value
        .and_then(|value| {
            value
                .as_f64()
                .or_else(|| value.as_str()?.trim().parse().ok())
        })
        .filter(|number| number.is_finite() && *number > 0.0)
        .unwrap_or(0.0)
}

fn capacity(value: Option<&Value>) -> Result<f64, PluginError> {
    match value {
        None | Some(Value::Null) => Ok(0.0),
        Some(Value::String(text)) if text.is_empty() => Ok(0.0),
        Some(Value::Number(number)) => number
            .as_f64()
            .ok_or_else(|| PluginError::decode("WorkBuddy billing capacity is invalid")),
        Some(Value::String(text)) => text
            .parse()
            .map_err(|_| PluginError::decode("WorkBuddy billing capacity is invalid")),
        Some(_) => Err(PluginError::decode("WorkBuddy billing capacity is invalid")),
    }
}

fn has_total(package: &Value) -> bool {
    package
        .get("CycleTotalCapacity")
        .is_some_and(|value| !value.is_null() && value.as_str() != Some(""))
}

fn packages(sum: &Value) -> &[Value] {
    sum.get("Packages")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn compact(number: f64) -> String {
    if number.fract() == 0.0 {
        return format!("{number:.0}");
    }
    format!("{number:.2}")
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_owned()
}

fn decode(body: &[u8]) -> Result<Value, PluginError> {
    hofund_plugin_sdk::json::from_slice(body)
        .map_err(|_| PluginError::decode("WorkBuddy billing JSON is invalid"))
}

fn ensure_success(root: &Value) -> Result<(), PluginError> {
    let refused = root.get("code").is_some_and(|code| {
        code.as_i64().unwrap_or(0) != 0
            || code
                .as_str()
                .and_then(|text| text.parse::<i64>().ok())
                .unwrap_or(0)
                != 0
    });
    if refused {
        Err(PluginError::decode("WorkBuddy billing refused the request"))
    } else {
        Ok(())
    }
}

fn data(root: &Value) -> &Value {
    root.get("data").unwrap_or(root)
}

#[cfg(test)]
mod tests {
    use super::{Site, enterprise, summary};
    use hofund_plugin_sdk::time;

    #[test]
    fn r55b_summary_splits_plan_and_extra_groups() {
        let summary = summary(
            br#"{"data":{"Packages":[],"CreditGroups":[
                {"Kind":"plan_base","CycleTotalCapacity":"1000","CycleUsedCapacity":"100"},
                {"Kind":"plan_bonus","CycleTotalCapacity":"500","CycleUsedCapacity":"50"},
                {"Kind":"addon","CycleTotalCapacity":"500","CycleUsedCapacity":"50"}
            ]}}"#,
            Site::International,
        )
        .expect("summary");
        assert_eq!(summary.windows.len(), 2);
        assert_eq!(summary.windows[0].name(), "Credits");
        assert_eq!(summary.windows[0].used(), 10);
        assert!(summary.windows[0].blocking());
        assert_eq!(summary.windows[1].name(), "Extra credits");
        assert_eq!(summary.windows[1].used(), 10);
        assert!(!summary.windows[1].blocking());
    }

    #[test]
    fn r55b_summary_uses_reference_codes_and_truthful_balances() {
        let summary = summary(
            br#"{"Packages":[
                {"PackageCode":"TCACA_code_008_cfWoLwvjU4","CycleTotalCapacity":"500","CycleUsedCapacity":"0"},
                {"PackageCode":"TCACA_code_007_nzdH5h4Nl0","CycleTotalCapacity":"300","CycleUsedCapacity":"20"},
                {"PackageCode":"WALLET","CycleTotalCapacity":"","CycleRemainCapacity":"750.25"}
            ]}"#,
            Site::China,
        )
        .expect("summary");
        assert_eq!(summary.windows.len(), 3);
        assert_eq!(summary.windows[0].name(), "Credits");
        assert_eq!(summary.windows[0].used(), 0);
        assert_eq!(summary.windows[1].name(), "Extra credits");
        assert_eq!(summary.windows[1].used(), 7);
        assert_eq!(summary.windows[2].name(), "Prepaid credits: 750.25 credits");
        assert_eq!(summary.windows[2].limit(), None);

        let empty = super::summary(
            br#"{"Packages":[],"TotalCredits":"120","usedCredits":10}"#,
            Site::China,
        )
        .expect("empty summary");
        assert!(empty.windows.is_empty());
    }

    #[test]
    fn r55b_enterprise_reads_limit_and_utc8_reset() {
        let window = enterprise(
            br#"{"code":0,"data":{"credit":196.13,"limitNum":2000,"cycleStartTime":"2026-10-08 00:00:00","cycleResetTime":"2026-11-09 00:00:00"}}"#,
        )
        .expect("enterprise")
        .expect("window");
        assert_eq!(window.name(), "Credits");
        assert_eq!(window.used(), 10);
        assert_eq!(
            window.resets_at(),
            time::parse_rfc3339("2026-11-08T16:00:00Z")
        );
    }
}
