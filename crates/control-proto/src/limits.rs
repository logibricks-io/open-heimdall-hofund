/// Maximum decoded configuration text size: one mebibyte.
pub const MAX_CONFIG_TEXT_BYTES: usize = 1 << 20;
/// Maximum error message size, truncated on a character boundary.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 4096;
/// Maximum request id size in bytes. Ids must be nonempty ASCII graphic text.
pub const MAX_REQUEST_ID_LEN: usize = 128;
/// Maximum request body size: six bytes of JSON escaping per configuration
/// byte, plus 64 KiB for the enclosing object and other fields.
pub const MAX_REQUEST_BODY_BYTES: usize = 6 * MAX_CONFIG_TEXT_BYTES + 64 * 1024;
/// Maximum response body size, including a maximal configuration document and
/// additive response fields within a bounded envelope.
pub const MAX_RESPONSE_BODY_BYTES: usize = 6 * MAX_CONFIG_TEXT_BYTES + 64 * 1024;
/// Maximum number of ids or edit tokens in one request list.
pub const MAX_IDS: usize = 4096;
/// Maximum size of one model, revision, or plan id in bytes.
pub const MAX_ID_BYTES: usize = 256;
/// Maximum boot id size in bytes.
pub const MAX_BOOT_ID_LEN: usize = 64;
