//! Whether a program on `PATH` is a Go program: some agents share their
//! binary name with an unrelated Go tool.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError, mpsc};
use std::time::{Duration, SystemTime};

const GO_BUILD_INFO_MAGIC: &[u8] = b"\xff Go buildinf:";
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);
const READ_SIZE: usize = 256 * 1024;
const MAX_BUILD_INFO_STRING: u64 = 1024 * 1024;

/// What a finished probe found, keyed by the binary's path, size, and
/// modification time so a replaced binary is probed again.
type ProbeKey = (PathBuf, u64, Option<SystemTime>);

static PROBED: Mutex<BTreeMap<ProbeKey, bool>> = Mutex::new(BTreeMap::new());

/// Whether `path` is the agent rather than a Go program of the same name.
///
/// A probe that outlasts the timeout counts as the agent, since a large
/// non-Go binary is what makes a probe slow. The probe still finishes in the
/// background and its answer serves the next call.
pub(crate) fn not_go_program(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return true;
    };
    let key = (path.to_owned(), metadata.len(), metadata.modified().ok());
    probe_cached(key, PROBE_TIMEOUT, go_program)
}

fn probe_cached(key: ProbeKey, timeout: Duration, probe: fn(&Path) -> bool) -> bool {
    if let Some(go) = PROBED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
    {
        return !*go;
    }
    let (sender, receiver) = mpsc::sync_channel(1);
    let spawned = std::thread::Builder::new()
        .name("go-binary-probe".to_owned())
        .spawn(move || {
            let go = probe(&key.0);
            PROBED
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(key, go);
            let _ = sender.send(go);
        });
    if spawned.is_err() {
        return true;
    }
    receiver.recv_timeout(timeout).map_or(true, |go| !go)
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

    fn slow_go(_: &std::path::Path) -> bool {
        std::thread::sleep(Duration::from_millis(100));
        true
    }

    fn fast_go(_: &std::path::Path) -> bool {
        true
    }

    fn not_go(_: &std::path::Path) -> bool {
        false
    }

    fn key(name: &str) -> super::ProbeKey {
        (std::path::PathBuf::from(name), 1, None)
    }

    #[test]
    fn b7_go_binary_probe_timeout_counts_as_the_agent_and_the_late_answer_is_kept() {
        assert!(super::probe_cached(
            key("/probe/slow"),
            Duration::ZERO,
            slow_go
        ));
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while super::PROBED
            .lock()
            .expect("probe cache")
            .get(&key("/probe/slow"))
            .is_none()
        {
            assert!(std::time::Instant::now() < deadline, "probe never finished");
            std::thread::sleep(Duration::from_millis(10));
        }
        // The cached answer wins over a new probe.
        assert!(!super::probe_cached(
            key("/probe/slow"),
            Duration::ZERO,
            not_go
        ));
    }

    #[test]
    fn b7_go_binary_go_program_within_the_timeout_is_not_the_agent() {
        assert!(!super::probe_cached(
            key("/probe/go"),
            Duration::from_secs(10),
            fast_go
        ));
        assert!(super::probe_cached(
            key("/probe/rust"),
            Duration::from_secs(10),
            not_go
        ));
    }

    #[test]
    fn b7_go_binary_replaced_binary_is_probed_again() {
        assert!(!super::probe_cached(
            key("/probe/swap"),
            Duration::from_secs(10),
            fast_go
        ));
        let replaced = (std::path::PathBuf::from("/probe/swap"), 2, None);
        assert!(super::probe_cached(
            replaced,
            Duration::from_secs(10),
            not_go
        ));
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
        let path = root.path().join("tool");
        std::fs::write(&path, bytes).expect("binary");
        assert!(super::go_program(&path));
    }
}
