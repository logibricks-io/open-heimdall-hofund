//! OIDC token verification and JWKS caching for the gateway exchange route.

#[cfg(test)]
pub mod test_issuer;

mod verify;

#[cfg(test)]
mod verify_tests;

pub use verify::{
    Claims, clip, match_token, oidc_key_name, set_fetch_client_for_tests, set_now_for_tests,
};

