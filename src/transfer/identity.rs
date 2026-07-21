use blake3::Hasher;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceSnapshot {
    pub path: PathBuf,
    pub size: u64,
    pub modified_nanos: Option<u128>,
}

impl SourceSnapshot {
    pub fn capture(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let metadata = path.metadata()?;
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos());
        Ok(Self {
            path: path.to_path_buf(),
            size: metadata.len(),
            modified_nanos,
        })
    }

    pub fn verify_unchanged(&self) -> io::Result<bool> {
        let current = Self::capture(&self.path)?;
        Ok(self.size == current.size && self.modified_nanos == current.modified_nanos)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub content_id: String,
    pub transfer_key: String,
    pub snapshot: SourceSnapshot,
}

pub fn identify_file(
    path: impl AsRef<Path>,
    sender_device_id: &str,
    filename: &str,
) -> io::Result<FileIdentity> {
    let snapshot = SourceSnapshot::capture(path)?;
    let mut file = File::open(&snapshot.path)?;
    let mut content_hasher = Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        content_hasher.update(&buffer[..count]);
    }

    if !snapshot.verify_unchanged()? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source file changed while hashing",
        ));
    }

    let content_id = content_hasher.finalize().to_hex().to_string();
    let mut key_hasher = Hasher::new();
    for component in [
        sender_device_id.as_bytes(),
        filename.as_bytes(),
        snapshot.size.to_string().as_bytes(),
        content_id.as_bytes(),
    ] {
        key_hasher.update(&(component.len() as u64).to_be_bytes());
        key_hasher.update(component);
    }
    let transfer_key = key_hasher.finalize().to_hex().to_string();
    Ok(FileIdentity {
        content_id,
        transfer_key,
        snapshot,
    })
}

pub fn hash_file(path: impl AsRef<Path>) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_name_and_size_with_different_content_have_different_ids() {
        let root = std::env::temp_dir().join(format!("locsd-identity-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let first = root.join("first.bin");
        let second = root.join("second.bin");
        std::fs::write(&first, b"aaaa").unwrap();
        std::fs::write(&second, b"bbbb").unwrap();
        let a = identify_file(&first, "sender", "same.bin").unwrap();
        let b = identify_file(&second, "sender", "same.bin").unwrap();
        assert_ne!(a.content_id, b.content_id);
        assert_ne!(a.transfer_key, b.transfer_key);
        let _ = std::fs::remove_dir_all(root);
    }
}
