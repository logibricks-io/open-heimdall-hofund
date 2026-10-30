//! PKCE (RFC 7636), the `state` value, and the authorize URL for a browser
//! sign-in. The kernel's loopback listener checks the callback; these build
//! what the browser is sent to.

use hofund_plugin_api::PluginError;
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use secrecy::{ExposeSecret, SecretString};

/// Random bytes in a verifier or state: 256 bits, 43 characters encoded.
const RANDOM_BYTES: usize = 32;

/// A PKCE verifier and its `S256` challenge. `Debug` redacts the verifier.
pub struct Pkce {
    /// Sent with the code exchange. Secret until then.
    pub verifier: SecretString,
    /// Sent in the authorize URL with `code_challenge_method=S256`.
    pub challenge: String,
}

impl Pkce {
    /// A fresh verifier from the system random source.
    ///
    /// # Errors
    ///
    /// Returns [`PluginError`] when the system random source fails.
    pub fn generate() -> Result<Self, PluginError> {
        Ok(Self::from_verifier(&random_token()?))
    }

    /// The challenge for a known verifier.
    fn from_verifier(verifier: &str) -> Self {
        let hash = digest(&SHA256, verifier.as_bytes());
        Self {
            challenge: base64url(hash.as_ref()),
            verifier: SecretString::from(verifier.to_owned()),
        }
    }
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Pkce")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// A fresh `state` value: 256 random bits, URL-safe.
///
/// # Errors
///
/// Returns [`PluginError`] when the system random source fails.
pub fn new_state() -> Result<String, PluginError> {
    random_token()
}

/// `base` with `pairs` appended as a form-encoded query.
///
/// # Errors
///
/// Returns [`PluginError`] when `base` is not an absolute `https` URL, or is
/// `http` on a host other than a loopback address, or already carries a
/// fragment.
pub fn authorize_url(base: &str, pairs: &[(&str, &str)]) -> Result<String, PluginError> {
    let mut url = url::Url::parse(base)
        .map_err(|_| PluginError::config("authorize URL is not an absolute URL"))?;
    let loopback = url.host().is_some_and(|host| match host {
        url::Host::Domain(name) => name == "localhost",
        url::Host::Ipv4(address) => address.is_loopback(),
        url::Host::Ipv6(address) => address.is_loopback(),
    });
    if !(url.scheme() == "https" || (url.scheme() == "http" && loopback)) {
        return Err(PluginError::config("authorize URL must use https"));
    }
    if url.fragment().is_some() || !url.username().is_empty() || url.password().is_some() {
        return Err(PluginError::config(
            "authorize URL must not carry a fragment or user info",
        ));
    }
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(key, value);
        }
    }
    Ok(url.into())
}

/// The authorization-code grant form (RFC 6749 section 4.1.3) with the PKCE
/// verifier (RFC 7636 section 4.5).
///
/// It contains the code and the verifier. Do not log it.
#[must_use]
pub fn code_exchange_form(
    client_id: &str,
    redirect_uri: &str,
    code: &SecretString,
    verifier: &SecretString,
) -> String {
    super::form_body(&[
        ("grant_type", "authorization_code"),
        ("code", code.expose_secret()),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("code_verifier", verifier.expose_secret()),
    ])
}

/// [`code_exchange_form`] for a client that has a secret, such as an
/// installed-app client whose secret ships in public builds.
///
/// It contains the code, the verifier, and the client secret. Do not log it.
#[must_use]
pub fn client_code_exchange_form(
    client_id: &str,
    client_secret: &str,
    redirect_uri: &str,
    code: &SecretString,
    verifier: &SecretString,
) -> String {
    super::form_body(&[
        ("grant_type", "authorization_code"),
        ("code", code.expose_secret()),
        ("redirect_uri", redirect_uri),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("code_verifier", verifier.expose_secret()),
    ])
}

fn random_token() -> Result<String, PluginError> {
    let mut bytes = [0_u8; RANDOM_BYTES];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| PluginError::internal("the system random source failed"))?;
    Ok(base64url(&bytes))
}

/// Base64url without padding (RFC 4648 section 5).
fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = chunk.get(1).copied().map_or(0, u32::from);
        let b2 = chunk.get(2).copied().map_or(0, u32::from);
        let triple = (b0 << 16) | (b1 << 8) | b2;
        let emit = chunk.len() + 1;
        for index in 0..emit {
            let sextet = (triple >> (18 - 6 * index)) & 0x3f;
            out.push(char::from(ALPHABET[sextet as usize]));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use secrecy::{ExposeSecret, SecretString};

    use super::{
        Pkce, authorize_url, base64url, client_code_exchange_form, code_exchange_form, new_state,
    };

    #[test]
    fn code_exchange_form_carries_the_grant_and_verifier() {
        let form = code_exchange_form(
            "app_1",
            "http://localhost:1455/auth/callback",
            &SecretString::from("c/1".to_owned()),
            &SecretString::from("v-1".to_owned()),
        );
        assert_eq!(
            form,
            "grant_type=authorization_code&code=c%2F1\
             &redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback\
             &client_id=app_1&code_verifier=v-1"
        );
    }

    #[test]
    fn client_code_exchange_form_adds_the_secret() {
        let form = client_code_exchange_form(
            "app_1",
            "s/1",
            "http://127.0.0.1:9/cb",
            &SecretString::from("c".to_owned()),
            &SecretString::from("v".to_owned()),
        );
        assert_eq!(
            form,
            "grant_type=authorization_code&code=c\
             &redirect_uri=http%3A%2F%2F127.0.0.1%3A9%2Fcb\
             &client_id=app_1&client_secret=s%2F1&code_verifier=v"
        );
    }

    #[test]
    fn pkce_matches_rfc7636_appendix_b() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert_eq!(
            pkce.verifier.expose_secret(),
            "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        );
    }

    #[test]
    fn base64url_matches_rfc4648_vectors() {
        let cases: [(&[u8], &str); 7] = [
            (b"", ""),
            (b"f", "Zg"),
            (b"fo", "Zm8"),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg"),
            (b"fooba", "Zm9vYmE"),
            (b"foobar", "Zm9vYmFy"),
        ];
        for (input, expected) in cases {
            assert_eq!(base64url(input), expected);
        }
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn generated_values_are_fresh_url_safe_and_43_chars() {
        let first = Pkce::generate().expect("pkce");
        let second = Pkce::generate().expect("pkce");
        assert_eq!(first.verifier.expose_secret().len(), 43);
        assert_ne!(
            first.verifier.expose_secret(),
            second.verifier.expose_secret()
        );
        assert_eq!(
            first.challenge,
            Pkce::from_verifier(first.verifier.expose_secret()).challenge
        );
        let state = new_state().expect("state");
        assert_eq!(state.len(), 43);
        assert!(
            state
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
        );
        assert_ne!(state, new_state().expect("state"));
    }

    #[test]
    fn pkce_debug_redacts_the_verifier() {
        let pkce = Pkce::from_verifier("verifier-sentinel");
        let shown = format!("{pkce:?}");
        assert!(!shown.contains("verifier-sentinel"), "{shown}");
        assert!(shown.contains(&pkce.challenge));
    }

    #[test]
    fn authorize_url_encodes_pairs_and_keeps_the_base_query() {
        let url = authorize_url(
            "https://auth.example.test/oauth/authorize?prompt=login",
            &[
                ("redirect_uri", "http://localhost:1455/auth/callback"),
                ("scope", "openid profile"),
            ],
        )
        .expect("url");
        assert_eq!(
            url,
            "https://auth.example.test/oauth/authorize?prompt=login\
             &redirect_uri=http%3A%2F%2Flocalhost%3A1455%2Fauth%2Fcallback&scope=openid+profile"
        );
    }

    #[test]
    fn authorize_url_rejects_plain_http_fragments_and_userinfo() {
        for base in [
            "http://auth.example.test/authorize",
            "https://auth.example.test/authorize#x",
            "https://user@auth.example.test/authorize",
            "auth.example.test/authorize",
        ] {
            assert!(authorize_url(base, &[]).is_err(), "{base}");
        }
        assert!(authorize_url("http://127.0.0.1:8080/authorize", &[]).is_ok());
    }
}
