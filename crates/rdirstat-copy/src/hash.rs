//! Streaming SHA-256.

use std::fmt::Write as _;
use std::io::{self, Read as _};
use std::path::Path;

use sha2::{Digest as _, Sha256};

const HASH_BUFFER: usize = 1024 * 1024;

/// SHA-256 of a file's contents, read in 1 MiB blocks.
///
/// The file is opened read-only. A symlink is followed (hash the bytes a
/// reader would see); callers that must not follow check the kind first.
///
/// # Errors
///
/// Open or read failure.
pub fn sha256_file(path: &Path) -> io::Result<[u8; 32]> {
    let _phase = crate::phase::enter(crate::phase::Phase::Hash);
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; HASH_BUFFER];
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                hasher.update(&buffer[..count]);
                crate::phase::add_bytes(crate::phase::Phase::Hash, count as u64);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(hasher.finalize().into())
}

/// Lowercase hex, the form the catalog stores.
#[must_use]
pub fn sha256_hex(digest: &[u8; 32]) -> String {
    let mut text = String::with_capacity(64);
    for byte in digest {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vectors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let empty = dir.path().join("empty");
        std::fs::write(&empty, b"").expect("write");
        assert_eq!(
            sha256_hex(&sha256_file(&empty).expect("hash")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let abc = dir.path().join("abc");
        std::fs::write(&abc, b"abc").expect("write");
        assert_eq!(
            sha256_hex(&sha256_file(&abc).expect("hash")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_multi_buffer_file_hashes_like_one_update() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("big");
        let payload: Vec<u8> = (0..(HASH_BUFFER * 2 + 7))
            .map(|i| u8::try_from(i % 253).unwrap_or_default())
            .collect();
        std::fs::write(&path, &payload).expect("write");
        let expected: [u8; 32] = Sha256::digest(&payload).into();
        assert_eq!(sha256_file(&path).expect("hash"), expected);
    }
}
