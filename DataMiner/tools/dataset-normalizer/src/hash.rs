use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::Context;
use sha2::{Digest, Sha256};

pub fn sha256_bytes(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let file = File::open(path)
        .with_context(|| format!("failed to open {} for hashing", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader
            .read(&mut buffer)
            .with_context(|| format!("failed to hash {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("sha256:{:x}", hasher.finalize()))
}
