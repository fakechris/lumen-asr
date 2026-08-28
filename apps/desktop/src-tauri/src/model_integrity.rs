//! Post-download integrity verification for sherpa model packages.
//!
//! Hashes are pinned from the known-good published archives. A mismatch means
//! a corrupted or tampered download — the caller fails closed and removes the
//! install. Extra files not listed here are ignored so upstream archive layout
//! additions do not break verification.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, Read};
use std::path::Path;

/// One pinned file inside an installed model directory.
pub struct PinnedFile {
    /// Path relative to the installed package dir.
    pub rel: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

pub const SENSEVOICE_FILES: &[PinnedFile] = &[
    PinnedFile {
        rel: "model.int8.onnx",
        sha256: "c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51",
        bytes: 239_233_841,
    },
    PinnedFile {
        rel: "tokens.txt",
        sha256: "f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc",
        bytes: 315_894,
    },
];

pub const PARAFORMER_OFFLINE_FILES: &[PinnedFile] = &[
    PinnedFile {
        rel: "model.int8.onnx",
        sha256: "f36a0433bcf096bd6d6f11b80a3ac8bed110bdca632fe0d731df8d1a84475945",
        bytes: 243_371_218,
    },
    PinnedFile {
        rel: "tokens.txt",
        sha256: "59aba8873a2ed1e122c25fee421e25f283b63290efbde85c1f01a853d83cb6e6",
        bytes: 75_756,
    },
];

pub const PARAFORMER_STREAMING_FILES: &[PinnedFile] = &[
    PinnedFile {
        rel: "encoder.int8.onnx",
        sha256: "81a70226a8934e6ed92aa1d4fc486b428b5398e2f2619ed4897b7294cab90e9a",
        bytes: 165_462_184,
    },
    PinnedFile {
        rel: "decoder.int8.onnx",
        sha256: "f3cca9f77bb9d93c8fcbfb63ae617b6b1ee96818df3aa3b151c40658fe38594f",
        bytes: 71_664_561,
    },
    PinnedFile {
        rel: "tokens.txt",
        sha256: "59aba8873a2ed1e122c25fee421e25f283b63290efbde85c1f01a853d83cb6e6",
        bytes: 75_756,
    },
];

/// Verify every pinned file under `dir` against its recorded size + SHA256.
pub fn verify_installed_package(dir: &Path, files: &[PinnedFile]) -> Result<(), String> {
    for pinned in files {
        let path = dir.join(pinned.rel);
        let meta =
            fs::metadata(&path).map_err(|e| format!("model file {} missing: {e}", pinned.rel))?;
        if meta.len() != pinned.bytes {
            return Err(format!(
                "model file {} size mismatch: expected {} bytes, got {}",
                pinned.rel,
                pinned.bytes,
                meta.len()
            ));
        }
        let digest =
            sha256_file(&path).map_err(|e| format!("model file {} unreadable: {e}", pinned.rel))?;
        if digest != pinned.sha256 {
            return Err(format!(
                "model file {} failed integrity check (sha256 mismatch)",
                pinned.rel
            ));
        }
    }
    Ok(())
}

fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_file(dir: &Path, rel: &str, content: &[u8]) {
        let path = dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let mut f = fs::File::create(path).unwrap();
        f.write_all(content).unwrap();
    }

    fn pinned_for(rel: &'static str, content: &[u8]) -> PinnedFile {
        let mut hasher = Sha256::new();
        hasher.update(content);
        // Leak is fine in tests: PinnedFile wants 'static strs.
        let sha256: &'static str = Box::leak(format!("{:x}", hasher.finalize()).into_boxed_str());
        PinnedFile {
            rel,
            sha256,
            bytes: content.len() as u64,
        }
    }

    #[test]
    fn accepts_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(tmp.path(), "a.bin", b"hello");
        let manifest = [pinned_for("a.bin", b"hello")];
        verify_installed_package(tmp.path(), &manifest).unwrap();
    }

    #[test]
    fn rejects_corrupted_content() {
        let tmp = tempfile::tempdir().unwrap();
        write_file(tmp.path(), "a.bin", b"tampered!");
        let manifest = [pinned_for("a.bin", b"hello world!!")];
        let err = verify_installed_package(tmp.path(), &manifest).unwrap_err();
        assert!(
            err.contains("size mismatch") || err.contains("sha256 mismatch"),
            "{err}"
        );
    }

    #[test]
    fn rejects_missing_files() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = [pinned_for("a.bin", b"hello")];
        let err = verify_installed_package(tmp.path(), &manifest).unwrap_err();
        assert!(err.contains("missing"), "{err}");
    }
}
