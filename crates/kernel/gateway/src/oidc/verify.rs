//! RS256 JWT verification against configured OIDC trust rules.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hofund_secrets::oidc::{Rule, RuleStore, claim_pattern_matches};
use ring::signature::{UnparsedPublicKey, RSA_PKCS1_2048_8192_SHA256};
use serde_json::Value;

const LEEWAY: Duration = Duration::from_secs(60);
const KEYS_FRESH: Duration = Duration::from_secs(3600);
const REFETCH_GAP: Duration = Duration::from_secs(10);

type FetchClient = Arc<dyn Fn(&str) -> Result<Vec<u8>, String> + Send + Sync>;

static FETCH: Mutex<Option<FetchClient>> = Mutex::new(None);
static NOW: Mutex<Option<SystemTime>> = Mutex::new(None);

/// Verified token claims.
pub type Claims = BTreeMap<String, Value>;

/// Installs a custom GET fetcher for tests.
pub fn set_fetch_client_for_tests(fetch: FetchClient) {
    *FETCH.lock().expect("fetch") = Some(fetch);
}

/// Overrides the clock used for exp, nbf, and iat checks in tests.
pub fn set_now_for_tests(now: Option<SystemTime>) {
    *NOW.lock().expect("clock") = now;
}

fn clock_now() -> SystemTime {
    NOW.lock()
        .expect("clock")
        .unwrap_or_else(SystemTime::now)
}

/// Shortens an unverified value for errors and logs.
pub fn clip(text: &str) -> String {
    if text.len() > 100 {
        format!("{}…", &text[..100])
    } else {
        text.to_owned()
    }
}

/// Reads claim `name` as text for matching and error messages.
pub fn claim_str(claims: &Claims, name: &str) -> String {
    match claims.get(name) {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Number(number)) => number.to_string(),
        Some(Value::Bool(value)) => value.to_string(),
        _ => String::new(),
    }
}

/// Verifies `token` and returns the first matching rule and its claims.
pub async fn match_token(
    token: &str,
    rules: &RuleStore,
) -> Result<(Rule, Claims), String> {
    let parts: Vec<&str> = token.split('.').collect();
    if parts.len() != 3 {
        return Err("the bearer token isn't a JWT".to_owned());
    }
    let header: Value =
        decode_json_part(parts[0]).map_err(|_| "the token's header can't be read".to_owned())?;
    let alg = header
        .get("alg")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if alg != "RS256" {
        return Err(format!(
            "the token is signed with {:?}: only RS256 is taken",
            clip(alg)
        ));
    }
    let kid = header
        .get("kid")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let claims: Claims = decode_part(parts[1])
        .map_err(|_| "the token's claims can't be read".to_owned())?;
    let iss = claim_str(&claims, "iss").trim_end_matches('/').to_owned();
    let configured = rules
        .list()
        .map_err(|error| error.to_string())?
        .into_iter()
        .filter(|rule| rule.issuer == iss)
        .collect::<Vec<_>>();
    if configured.is_empty() {
        return Err(format!(
            "no rule takes tokens from {:?} (hofund gateway-key oidc add)",
            clip(&iss)
        ));
    }
    let sig = URL_SAFE_NO_PAD
        .decode(parts[2])
        .map_err(|_| "the token's signature can't be read".to_owned())?;
    let signed = format!("{}.{}", parts[0], parts[1]);
    let public = public_key(&configured[0].issuer, &kid).await?;
    let key = UnparsedPublicKey::new(&RSA_PKCS1_2048_8192_SHA256, public.as_slice());
    key.verify(signed.as_bytes(), &sig)
        .map_err(|_| "the token's signature is not its issuer's".to_owned())?;
    let clock = clock_now();
    let exp = claim_time(&claims, "exp").ok_or_else(|| "the token has no expiry".to_owned())?;
    if exp
        .checked_add(LEEWAY)
        .is_some_and(|limit| clock >= limit)
    {
        return Err("the token has expired".to_owned());
    }
    if let Some(nbf) = claim_time(&claims, "nbf")
        && clock
            .checked_add(LEEWAY)
            .is_some_and(|limit| limit < nbf)
    {
        return Err("the token isn't valid yet".to_owned());
    }
    if let Some(iat) = claim_time(&claims, "iat")
        && clock
            .checked_add(LEEWAY)
            .is_some_and(|limit| limit < iat)
    {
        return Err(
            "the token was issued in the future: check this computer's clock".to_owned(),
        );
    }
    let audiences = audiences(&claims);
    let mismatch = described(&claims, &configured);
    for rule in &configured {
        if !audiences.iter().any(|aud| aud == &rule.audience) {
            continue;
        }
        if !claims_match(&rule.claims, &claims) {
            continue;
        }
        if !claim_str(&claims, "jti").is_empty() {
            let jti = claim_str(&claims, "jti");
            if !first_use(&format!("{iss}\0{jti}"), exp, clock) {
                return Err(
                    "this token was traded for a key already: ask the issuer for a new one"
                        .to_owned(),
                );
            }
        }
        return Ok((rule.clone(), claims));
    }
    Err(format!(
        "the token matches no rule: it has aud {:?} and {}",
        audiences.join(","),
        mismatch
    ))
}

fn decode_json_part(part: &str) -> Result<Value, ()> {
    let bytes = URL_SAFE_NO_PAD.decode(part).map_err(|_| ())?;
    serde_json::from_slice(&bytes).map_err(|_| ())
}

fn decode_part(part: &str) -> Result<Claims, ()> {
    let bytes = URL_SAFE_NO_PAD.decode(part).map_err(|_| ())?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
    match value {
        Value::Object(map) => Ok(map.into_iter().collect()),
        _ => Err(()),
    }
}

fn claim_time(claims: &Claims, name: &str) -> Option<SystemTime> {
    let number = claims.get(name)?;
    let seconds = match number {
        Value::Number(number) => number.as_f64()?,
        _ => return None,
    };
    UNIX_EPOCH.checked_add(Duration::from_secs_f64(seconds))
}

fn audiences(claims: &Claims) -> Vec<String> {
    match claims.get("aud") {
        Some(Value::String(aud)) => vec![aud.clone()],
        Some(Value::Array(values)) => values
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect(),
        _ => Vec::new(),
    }
}

fn claims_match(want: &BTreeMap<String, String>, claims: &Claims) -> bool {
    want.iter().all(|(name, pattern)| {
        claims.contains_key(name) && claim_pattern_matches(pattern, &claim_str(claims, name))
    })
}

fn described(claims: &Claims, rules: &[Rule]) -> String {
    let mut names = BTreeMap::new();
    for rule in rules {
        for name in rule.claims.keys() {
            names.insert(name.clone(), ());
        }
    }
    names
        .keys()
        .map(|name| format!("{name}={:?}", clip(&claim_str(claims, name))))
        .collect::<Vec<_>>()
        .join(", ")
}

static USED: LazyLock<Mutex<HashMap<String, SystemTime>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn first_use(id: &str, until: SystemTime, now: SystemTime) -> bool {
    let mut used = USED.lock().expect("used jti");
    used.retain(|_, expiry| now < *expiry);
    if used.contains_key(id) {
        return false;
    }
    used.insert(id.to_owned(), until.checked_add(LEEWAY).unwrap_or(until));
    true
}

#[derive(Default)]
struct KeySet {
    keys: HashMap<String, Vec<u8>>,
    fetched: Option<SystemTime>,
    tried: Option<SystemTime>,
    loading: Option<Arc<()>>,
}

static SETS: LazyLock<Mutex<HashMap<String, KeySet>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

async fn public_key(issuer: &str, kid: &str) -> Result<Vec<u8>, String> {
    loop {
        let mut sets = SETS.lock().expect("jwks sets");
        let entry = sets.entry(issuer.to_owned()).or_default();
        let now = clock_now();
        let have = entry.keys.get(kid).cloned();
        let stale = entry.fetched.is_none()
            || entry
                .fetched
                .is_some_and(|fetched| now.duration_since(fetched).unwrap_or(Duration::ZERO) > KEYS_FRESH)
            || have.is_none();
        if !stale
            || (entry.tried.is_some()
                && entry.loading.is_none()
                && entry
                    .tried
                    .is_some_and(|tried| now.duration_since(tried).unwrap_or(Duration::ZERO) < REFETCH_GAP))
        {
            drop(sets);
            return have.ok_or_else(|| {
                format!(
                    "the token's key {:?} isn't one {} signs with",
                    clip(kid),
                    issuer
                )
            });
        }
        if entry.loading.is_some() {
            let wait = Arc::clone(entry.loading.as_ref().expect("loading"));
            drop(sets);
            tokio::time::sleep(Duration::from_millis(10)).await;
            let _ = wait;
            continue;
        }
        let done = Arc::new(());
        entry.loading = Some(Arc::clone(&done));
        entry.tried = Some(now);
        drop(sets);
        let fetched_result = fetch_keys(issuer).await;
        let mut sets = SETS.lock().expect("jwks sets");
        let entry = sets.get_mut(issuer).expect("issuer set");
        if let Ok(ref keys) = fetched_result {
            entry.keys.clone_from(keys);
            entry.fetched = Some(clock_now());
        }
        entry.loading = None;
        let have = entry.keys.get(kid).cloned();
        drop(sets);
        if let Some(key) = have {
            return Ok(key);
        }
        if let Err(error) = fetched_result {
            return Err(format!("{issuer}'s signing keys can't be read: {error}"));
        }
        return Err(format!(
            "the token's key {:?} isn't one {} signs with",
            clip(kid),
            issuer
        ));
    }
}

fn default_fetch(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| error.to_string())?;
    let response = client
        .get(url)
        .send()
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("{url} answered {}", response.status()));
    }
    response
        .bytes()
        .map(|bytes| bytes.to_vec())
        .map_err(|error| error.to_string())
}

fn fetch(url: &str) -> Result<Vec<u8>, String> {
    if let Some(client) = FETCH.lock().expect("fetch").as_ref() {
        return client(url);
    }
    default_fetch(url)
}

async fn fetch_keys(issuer: &str) -> Result<HashMap<String, Vec<u8>>, String> {
    let discovery = format!("{issuer}/.well-known/openid-configuration");
    let body = tokio::task::spawn_blocking(move || fetch(&discovery))
        .await
        .map_err(|error| error.to_string())??;
    let conf: Value = serde_json::from_slice(&body).map_err(|error| error.to_string())?;
    let doc_issuer = conf
        .get("issuer")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_end_matches('/');
    if doc_issuer != issuer {
        return Err(format!(
            "its discovery document names the issuer {:?}",
            clip(doc_issuer)
        ));
    }
    let jwks_uri = conf
        .get("jwks_uri")
        .and_then(Value::as_str)
        .ok_or_else(|| "its jwks_uri isn't https".to_owned())?
        .to_owned();
    if !jwks_uri.starts_with("https://") {
        return Err(format!("its jwks_uri isn't https: {:?}", clip(&jwks_uri)));
    }
    let body = tokio::task::spawn_blocking(move || fetch(&jwks_uri))
        .await
        .map_err(|error| error.to_string())??;
    parse_jwks(&body)
}

fn parse_jwks(body: &[u8]) -> Result<HashMap<String, Vec<u8>>, String> {
    let set: Value = serde_json::from_slice(body).map_err(|error| error.to_string())?;
    let keys = set
        .get("keys")
        .and_then(Value::as_array)
        .ok_or_else(|| "it lists no RSA signing key".to_owned())?;
    let mut out = HashMap::new();
    for key in keys {
        let Some(object) = key.as_object() else {
            continue;
        };
        if object.get("kty").and_then(Value::as_str) != Some("RSA") {
            continue;
        }
        if let Some(use_) = object.get("use").and_then(Value::as_str)
            && use_ != "sig"
        {
            continue;
        }
        let Some(kid) = object.get("kid").and_then(Value::as_str) else {
            continue;
        };
        let n = object
            .get("n")
            .and_then(Value::as_str)
            .and_then(|text| URL_SAFE_NO_PAD.decode(text).ok());
        let e = object
            .get("e")
            .and_then(Value::as_str)
            .and_then(|text| URL_SAFE_NO_PAD.decode(text).ok());
        let (Some(n), Some(e)) = (n, e) else {
            continue;
        };
        if e.is_empty() || e.len() > 4 {
            continue;
        }
        let exponent = bytes_to_u32(&e);
        if n.len() < 256 || exponent < 3 {
            continue;
        }
        let der = rsa_spki_der(&n, exponent);
        out.insert(kid.to_owned(), der);
    }
    if out.is_empty() {
        Err("it lists no RSA signing key".to_owned())
    } else {
        Ok(out)
    }
}

fn bytes_to_u32(bytes: &[u8]) -> u32 {
    let mut value = 0_u32;
    for byte in bytes {
        value = (value << 8) | u32::from(*byte);
    }
    value
}

fn rsa_spki_der(modulus: &[u8], exponent: u32) -> Vec<u8> {
    let mut int_mod = modulus.to_vec();
    if int_mod.first().is_some_and(|byte| *byte & 0x80 != 0) {
        int_mod.insert(0, 0);
    }
    let int_exp = if exponent >= 0x1000000 {
        let mut bytes = vec![0_u8];
        bytes.extend_from_slice(&exponent.to_be_bytes());
        bytes
    } else if exponent >= 0x10000 {
        exponent.to_be_bytes()[..4].to_vec()
    } else if exponent >= 0x100 {
        exponent.to_be_bytes()[..2].to_vec()
    } else {
        vec![exponent as u8]
    };
    let seq = der_sequence(&[der_integer(&int_mod).as_slice(), der_integer(&int_exp).as_slice()]);
    let alg = der_sequence(&[der_oid_rsa_encryption().as_slice(), der_null().as_slice()]);
    let bit_string = der_bit_string(&seq);
    der_sequence(&[alg.as_slice(), bit_string.as_slice()])
}

fn der_integer(bytes: &[u8]) -> Vec<u8> {
    let mut body = bytes.to_vec();
    if body.first().is_some_and(|byte| *byte & 0x80 != 0) {
        body.insert(0, 0);
    }
    der_tlv(0x02, &body)
}

fn der_null() -> Vec<u8> {
    der_tlv(0x05, &[])
}

fn der_oid_rsa_encryption() -> Vec<u8> {
    der_tlv(0x06, &[0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01])
}

fn der_bit_string(bytes: &[u8]) -> Vec<u8> {
    let mut body = vec![0];
    body.extend_from_slice(bytes);
    der_tlv(0x03, &body)
}

fn der_sequence(parts: &[&[u8]]) -> Vec<u8> {
    let mut body = Vec::new();
    for part in parts {
        body.extend_from_slice(part);
    }
    der_tlv(0x30, &body)
}

fn der_tlv(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    out.extend(der_length(body.len()));
    out.extend_from_slice(body);
    out
}

fn der_length(len: usize) -> Vec<u8> {
    if len < 0x80 {
        vec![len as u8]
    } else if len <= 0xff {
        vec![0x81, len as u8]
    } else {
        vec![0x82, (len >> 8) as u8, len as u8]
    }
}

/// Builds a display name for a key issued from OIDC claims.
pub fn oidc_key_name(claims: &Claims) -> String {
    let mut name = claim_str(claims, "sub");
    let repo = claim_str(claims, "repository");
    if !repo.is_empty() {
        name = repo;
        let reference = claim_str(claims, "ref");
        if !reference.is_empty() {
            let trimmed = reference
                .trim_start_matches("refs/heads/")
                .trim_start_matches("refs/");
            name.push('@');
            name.push_str(trimmed);
        }
        let run = claim_str(claims, "run_id");
        if !run.is_empty() {
            name.push_str(" (run ");
            name.push_str(&run);
            name.push(')');
        }
    }
    let cleaned: String = name
        .chars()
        .map(|ch| {
            if ch.is_control() || ch == '\u{7f}' {
                ' '
            } else {
                ch
            }
        })
        .collect();
    let trimmed = cleaned.trim();
    let mut name = if trimmed.is_empty() {
        "OIDC".to_owned()
    } else {
        trimmed.to_owned()
    };
    if name.chars().count() > 120 {
        name = name.chars().take(119).collect();
        name.push('…');
    }
    name
}
