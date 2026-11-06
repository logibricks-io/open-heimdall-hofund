//! Inflate helpers for plugin frame decoding.

use std::io::Read;

use flate2::read::GzDecoder;
use hofund_plugin_api::PluginError;

/// Maximum inflated output size for one gzip blob.
pub const GZIP_OUTPUT_CAP: usize = 64 * 1024 * 1024;

/// Inflates one gzip blob with a pure-Rust backend.
///
/// # Errors
///
/// Returns [`PluginError`] when the input is not gzip, the stream is truncated,
/// or inflated output would exceed [`GZIP_OUTPUT_CAP`].
pub fn gzip(input: &[u8]) -> Result<Vec<u8>, PluginError> {
    let mut decoder = GzDecoder::new(input);
    let mut output = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = decoder
            .read(&mut chunk)
            .map_err(|error| PluginError::decode(error.to_string()))?;
        if read == 0 {
            break;
        }
        let new_len = output
            .len()
            .checked_add(read)
            .ok_or_else(|| PluginError::decode("gzip output length overflow"))?;
        if new_len > GZIP_OUTPUT_CAP {
            return Err(PluginError::decode(format!(
                "gzip output exceeds {GZIP_OUTPUT_CAP} bytes"
            )));
        }
        output.extend_from_slice(&chunk[..read]);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::{GZIP_OUTPUT_CAP, gzip};

    #[test]
    fn gzip_round_trips_a_small_payload() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(b"hello frame").expect("compress");
        let compressed = encoder.finish().expect("finish");
        assert_eq!(gzip(&compressed).expect("inflate"), b"hello frame");
    }

    #[test]
    fn gzip_output_at_the_cap_is_accepted() {
        let payload = vec![b'a'; GZIP_OUTPUT_CAP];
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&payload).expect("compress");
        let compressed = encoder.finish().expect("finish");
        assert_eq!(gzip(&compressed).expect("inflate").len(), GZIP_OUTPUT_CAP);
    }

    #[test]
    fn gzip_output_one_byte_over_the_cap_is_rejected() {
        let payload = vec![b'b'; GZIP_OUTPUT_CAP + 1];
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&payload).expect("compress");
        let compressed = encoder.finish().expect("finish");
        assert!(gzip(&compressed).is_err());
    }

    #[test]
    fn inverted_cap_check_fails_when_the_cap_is_off_by_one() {
        let payload = vec![b'c'; GZIP_OUTPUT_CAP];
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&payload).expect("compress");
        let compressed = encoder.finish().expect("finish");
        let inflated = gzip(&compressed).expect("at cap");
        assert_ne!(inflated.len(), GZIP_OUTPUT_CAP + 1);
    }
}
