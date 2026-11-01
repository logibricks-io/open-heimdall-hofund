//! In-memory OIDC issuer for tests (no network).

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ring::rand::SystemRandom;
use ring::signature::{RsaKeyPair, RSA_PKCS1_SHA256};
use ring::rsa::PublicKeyComponents;
use serde_json::{Number, Value};

pub struct TestIssuer {
    pub url: String,
    pair: Arc<RsaKeyPair>,
    spki: Vec<u8>,
    modulus: Vec<u8>,
    exponent: Vec<u8>,
    extra: Mutex<BTreeMap<String, Arc<RsaKeyPair>>>,
    fetches: AtomicU32,
    down: AtomicBool,
}

impl TestIssuer {
    pub fn new() -> Self {
        let pair = Arc::new(generate_key());
        let components = PublicKeyComponents::<Vec<u8>>::from(pair.public());
        let spki = pair.public().as_ref().to_vec();
        Self {
            url: "https://issuer.test".to_owned(),
            spki,
            modulus: components.n,
            exponent: components.e,
            pair,
            extra: Mutex::new(BTreeMap::new()),
            fetches: AtomicU32::new(0),
            down: AtomicBool::new(false),
        }
    }

    pub fn fetch_count(&self) -> u32 {
        self.fetches.load(Ordering::SeqCst)
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    pub fn add_key(&self, kid: &str) {
        self.extra.lock().expect("extra").insert(kid.to_owned(), Arc::clone(&self.pair));
    }

    pub fn claims(
        &self,
        audience: &str,
        over: &BTreeMap<String, Value>,
    ) -> BTreeMap<String, Value> {
        static JTI: AtomicU32 = AtomicU32::new(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .unwrap_or(0);
        let mut claims = BTreeMap::from([
            ("iss".to_owned(), Value::String(self.url.clone())),
            (
                "aud".to_owned(),
                Value::String(audience.to_owned()),
            ),
            (
                "sub".to_owned(),
                Value::String("repo:acme/app:ref:refs/heads/main".to_owned()),
            ),
            ("repository".to_owned(), Value::String("acme/app".to_owned())),
            (
                "repository_owner".to_owned(),
                Value::String("acme".to_owned()),
            ),
            (
                "ref".to_owned(),
                Value::String("refs/heads/main".to_owned()),
            ),
            ("run_id".to_owned(), Value::String("42".to_owned())),
            ("iat".to_owned(), Value::Number(Number::from(now))),
            ("nbf".to_owned(), Value::Number(Number::from(now))),
            ("exp".to_owned(), Value::Number(Number::from(now + 300))),
            (
                "jti".to_owned(),
                Value::String(format!("j{}", JTI.fetch_add(1, Ordering::SeqCst))),
            ),
        ]);
        for (key, value) in over {
            if value.is_null() {
                claims.remove(key);
            } else {
                claims.insert(key.clone(), value.clone());
            }
        }
        claims
    }

    pub fn sign(&self, kid: &str, claims: BTreeMap<String, Value>) -> String {
        let pair = if kid == "k1" {
            Arc::clone(&self.pair)
        } else {
            self.extra
                .lock()
                .expect("extra")
                .get(kid)
                .cloned()
                .unwrap_or_else(|| Arc::clone(&self.pair))
        };
        let header = serde_json::json!({"alg":"RS256","typ":"JWT","kid":kid});
        let header = URL_SAFE_NO_PAD.encode(header.to_string());
        let payload = URL_SAFE_NO_PAD.encode(
            serde_json::to_string(&claims).expect("claims"),
        );
        let signed = format!("{header}.{payload}");
        let mut signature = vec![0; pair.public().modulus_len()];
        pair.sign(
            &RSA_PKCS1_SHA256,
            &SystemRandom::new(),
            signed.as_bytes(),
            &mut signature,
        )
        .expect("sign");
        format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature))
    }

    pub fn spki(&self) -> &[u8] {
        &self.spki
    }

    pub fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        if self.down.load(Ordering::SeqCst) {
            return Err("down".to_owned());
        }
        let discovery = format!("{}/.well-known/openid-configuration", self.url);
        if url == discovery {
            return Ok(format!(
                r#"{{"issuer":"{}","jwks_uri":"{}/jwks"}}"#,
                self.url, self.url
            )
            .into_bytes());
        }
        if url == format!("{}/jwks", self.url) {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            let mut keys = vec![self.jwk("k1", &self.modulus, &self.exponent)];
            for kid in self.extra.lock().expect("extra").keys() {
                keys.push(self.jwk(kid, &self.modulus, &self.exponent));
            }
            return serde_json::to_vec(&serde_json::json!({"keys": keys}))
                .map_err(|error| error.to_string());
        }
        Err(format!("unknown url {url}"))
    }

    fn jwk(&self, kid: &str, n: &[u8], e: &[u8]) -> Value {
        serde_json::json!({
            "kty": "RSA",
            "kid": kid,
            "use": "sig",
            "alg": "RS256",
            "n": URL_SAFE_NO_PAD.encode(n),
            "e": URL_SAFE_NO_PAD.encode(e),
        })
    }
}

fn generate_key() -> RsaKeyPair {
    const PEM: &str = include_str!("test-key.pem");
    let mut body = String::new();
    for line in PEM.lines() {
        let line = line.trim();
        if line.starts_with("-----") {
            continue;
        }
        body.push_str(line);
    }
    let pkcs8 = STANDARD.decode(body.as_bytes()).expect("pem");
    RsaKeyPair::from_pkcs8(&pkcs8).expect("pair")
}
