//! The `Provider` trait and the upstream adapters that execute a canonical request
//! against one account and endpoint. Adapters own vendor-specific behavior by
//! combining a protocol codec with quirk data. Callers above this crate see
//! providers, not vendors.

// Referenced so this crate's manifest dependencies are compiled, not merely declared.
use hofund_codec as _;
use hofund_quirks as _;
