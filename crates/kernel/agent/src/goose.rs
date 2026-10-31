use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use yaml_rust2::Yaml;

use crate::AgentError;
use crate::edit::{FileKind, string_at, yaml_key, yaml_root};

const GO_BUILD_INFO_MAGIC: &[u8] = b"\xff Go buildinf:";
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const READ_SIZE: usize = 256 * 1024;
const MAX_BUILD_INFO_STRING: u64 = 1024 * 1024;

pub(crate) const CLEAR_KEYS: &[&str] = &[
    "active_provider",
    "GOOSE_PROVIDER",
    "GOOSE_MODEL",
    "providers.hofund",
];

pub(crate) fn new_layout(_kind: FileKind, text: &str) -> Result<bool, AgentError> {
    match yaml_root(text)? {
        Yaml::Hash(hash) => Ok(hash.contains_key(&yaml_key("providers"))
            || hash.contains_key(&yaml_key("active_provider"))),
        _ => Ok(false),
    }
}

pub(crate) fn configured_model(kind: FileKind, text: &str) -> Result<Option<String>, AgentError> {
    let provider = string_at(kind, text, "active_provider")?
        .filter(|provider| !provider.is_empty())
        .or(string_at(kind, text, "GOOSE_PROVIDER")?)
        .unwrap_or_default();
    if provider != "hofund" {
        return Ok(None);
    }
    Ok(string_at(kind, text, "providers.hofund.model")?
        .filter(|model| !model.is_empty())
        .or(string_at(kind, text, "GOOSE_MODEL")?))
}

pub(crate) fn binary_detected(path: &Path) -> bool {
    let path = path.to_owned();
    detected_with_timeout(PROBE_TIMEOUT, move || go_program(&path))
}

fn detected_with_timeout(timeout: Duration, probe: impl FnOnce() -> bool + Send + 'static) -> bool {
    let (sender, receiver) = mpsc::sync_channel(1);
    let Ok(_worker) = std::thread::Builder::new()
        .name("goose-binary-probe".to_owned())
        .spawn(move || {
            let _ = sender.send(probe());
        })
    else {
        return false;
    };
    receiver
        .recv_timeout(timeout)
        .is_ok_and(|go_program| !go_program)
}

fn go_program(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut buffer = vec![0; READ_SIZE];
    let mut offset = 0_u64;
    let mut kept = 0;
    loop {
        let Ok(read) = file.read(&mut buffer[kept..]) else {
            return false;
        };
        if read == 0 {
            return false;
        }
        offset += read as u64;
        let used = kept + read;
        if let Some(position) = find(&buffer[..used], GO_BUILD_INFO_MAGIC) {
            let after_magic = offset - used as u64 + (position + GO_BUILD_INFO_MAGIC.len()) as u64;
            return valid_build_info(&mut file, after_magic);
        }
        kept = (GO_BUILD_INFO_MAGIC.len() - 1).min(used);
        buffer.copy_within(used - kept..used, 0);
    }
}

fn valid_build_info(file: &mut File, offset: u64) -> bool {
    if file.seek(SeekFrom::Start(offset)).is_err() {
        return false;
    }
    let mut header = [0; 2];
    if file.read_exact(&mut header).is_err() || !matches!(header[0], 4 | 8) {
        return false;
    }
    let flags = header[1];
    if flags & 2 == 0 {
        // Older binaries point at their build strings. The magic and header
        // still distinguish them without parsing their whole symbol tables.
        return true;
    }
    let Some(version) = read_build_string(file) else {
        return false;
    };
    if version.is_empty() || std::str::from_utf8(&version).is_err() {
        return false;
    }
    let Some(module) = read_build_string(file) else {
        return false;
    };
    std::str::from_utf8(&module).is_ok()
}

fn read_build_string(file: &mut File) -> Option<Vec<u8>> {
    let length = read_uvarint(file)?;
    if length > MAX_BUILD_INFO_STRING {
        return None;
    }
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes).ok()?;
    Some(bytes)
}

fn read_uvarint(file: &mut File) -> Option<u64> {
    let mut value = 0_u64;
    for shift in (0..70).step_by(7) {
        let mut byte = [0];
        file.read_exact(&mut byte).ok()?;
        if shift == 63 && byte[0] > 1 {
            return None;
        }
        value |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn b7_goose_probe_timeout_is_not_detected() {
        assert!(!super::detected_with_timeout(Duration::ZERO, || {
            std::thread::sleep(Duration::from_millis(20));
            true
        }));
    }

    #[test]
    fn go_build_info_magic_is_found_across_reads() {
        let mut bytes = vec![0; super::READ_SIZE + 100];
        let at = super::READ_SIZE - 5;
        bytes[at..at + super::GO_BUILD_INFO_MAGIC.len()]
            .copy_from_slice(super::GO_BUILD_INFO_MAGIC);
        let header = at + super::GO_BUILD_INFO_MAGIC.len();
        bytes[header..header + 12]
            .copy_from_slice(&[8, 2, 8, b'g', b'o', b'1', b'.', b'9', b'9', b'.', b'1', 0]);
        let root = tempfile::tempdir().expect("temp dir");
        let path = root.path().join("goose");
        std::fs::write(&path, bytes).expect("binary");
        assert!(super::go_program(&path));
    }
}
