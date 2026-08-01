use blake3::Hasher;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;
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

#[derive(Clone, Debug)]
pub struct FileIdentity {
    pub content_id: String,
    pub transfer_key: String,
    pub snapshot: SendSourceSnapshot,
}

#[derive(Clone, Debug)]
pub struct CachedFileIdentity {
    pub content_id: String,
    pub transfer_key: String,
    pub size: u64,
    pub modified_nanos: Option<u128>,
}

impl FileIdentity {
    pub fn cache_entry(&self) -> CachedFileIdentity {
        CachedFileIdentity {
            content_id: self.content_id.clone(),
            transfer_key: self.transfer_key.clone(),
            size: self.snapshot.size,
            modified_nanos: self.snapshot.modified_nanos,
        }
    }
}

pub fn restore_cached_identity(
    source: SendSource,
    cached: &CachedFileIdentity,
) -> io::Result<Option<FileIdentity>> {
    let snapshot = SendSourceSnapshot::capture(source)?;
    if cached.modified_nanos.is_none()
        || snapshot.size != cached.size
        || snapshot.modified_nanos != cached.modified_nanos
    {
        return Ok(None);
    }
    Ok(Some(FileIdentity {
        content_id: cached.content_id.clone(),
        transfer_key: cached.transfer_key.clone(),
        snapshot,
    }))
}

#[derive(Clone, Debug)]
pub enum SendSource {
    Path(PathBuf),
    #[cfg(unix)]
    FileDescriptor(Arc<File>),
}

impl SendSource {
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self::Path(path.into())
    }

    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Path(path) => Some(path),
            #[cfg(unix)]
            Self::FileDescriptor(_) => None,
        }
    }

    fn metadata(&self) -> io::Result<std::fs::Metadata> {
        match self {
            Self::Path(path) => path.metadata(),
            #[cfg(unix)]
            Self::FileDescriptor(file) => file.metadata(),
        }
    }

    pub fn open_reader(&self) -> io::Result<SendSourceReader> {
        let file = match self {
            Self::Path(path) => File::open(path)?,
            #[cfg(unix)]
            Self::FileDescriptor(file) => file.try_clone()?,
        };
        Ok(SendSourceReader { file })
    }
}

#[derive(Debug)]
pub struct SendSourceReader {
    file: File,
}

impl SendSourceReader {
    pub fn read_at(&self, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileExt;
            self.file.read_at(buffer, offset)
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::FileExt;
            self.file.seek_read(buffer, offset)
        }
        #[cfg(not(any(unix, windows)))]
        {
            use std::io::{Seek, SeekFrom};
            let mut file = self.file.try_clone()?;
            file.seek(SeekFrom::Start(offset))?;
            file.read(buffer)
        }
    }
}

#[derive(Clone, Debug)]
pub struct SendSourceSnapshot {
    pub source: SendSource,
    pub size: u64,
    pub modified_nanos: Option<u128>,
}

impl SendSourceSnapshot {
    pub fn capture(source: SendSource) -> io::Result<Self> {
        let metadata = source.metadata()?;
        let modified_nanos = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos());
        Ok(Self {
            source,
            size: metadata.len(),
            modified_nanos,
        })
    }

    pub fn verify_unchanged(&self) -> io::Result<bool> {
        let current = Self::capture(self.source.clone())?;
        Ok(self.size == current.size && self.modified_nanos == current.modified_nanos)
    }

    pub fn open_reader(&self) -> io::Result<SendSourceReader> {
        self.source.open_reader()
    }
}

pub fn identify_file(
    path: impl AsRef<Path>,
    sender_device_id: &str,
    filename: &str,
) -> io::Result<FileIdentity> {
    identify_source(
        SendSource::from_path(path.as_ref().to_path_buf()),
        sender_device_id,
        filename,
        |_| {},
    )
}

pub fn identify_source(
    source: SendSource,
    sender_device_id: &str,
    filename: &str,
    mut on_progress: impl FnMut(u64),
) -> io::Result<FileIdentity> {
    let snapshot = SendSourceSnapshot::capture(source)?;
    let reader = snapshot.open_reader()?;
    let mut content_hasher = Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut offset = 0u64;
    loop {
        let count = reader.read_at(&mut buffer, offset)?;
        if count == 0 {
            break;
        }
        content_hasher.update(&buffer[..count]);
        offset = offset.saturating_add(count as u64);
        on_progress(offset.min(snapshot.size));
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

/// Captures a source and creates a stable transfer key without reading the
/// complete file. The complete content hash can then be calculated while the
/// range workers are already transferring data.
pub fn prepare_source(
    source: SendSource,
    sender_device_id: &str,
    filename: &str,
    source_key: Option<&str>,
) -> io::Result<FileIdentity> {
    let snapshot = SendSourceSnapshot::capture(source)?;
    let fallback_key = snapshot
        .source
        .path()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default();
    let source_key = source_key.unwrap_or(&fallback_key);
    let modified = snapshot
        .modified_nanos
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".into());
    let mut key_hasher = Hasher::new();
    for component in [
        b"localsend-deferred-v1".as_slice(),
        sender_device_id.as_bytes(),
        filename.as_bytes(),
        snapshot.size.to_string().as_bytes(),
        modified.as_bytes(),
        source_key.as_bytes(),
    ] {
        key_hasher.update(&(component.len() as u64).to_be_bytes());
        key_hasher.update(component);
    }
    Ok(FileIdentity {
        content_id: String::new(),
        transfer_key: key_hasher.finalize().to_hex().to_string(),
        snapshot,
    })
}

pub fn hash_source_snapshot(
    snapshot: &SendSourceSnapshot,
    mut on_progress: impl FnMut(u64),
) -> io::Result<String> {
    let reader = snapshot.open_reader()?;
    let mut content_hasher = Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut offset = 0u64;
    loop {
        let count = reader.read_at(&mut buffer, offset)?;
        if count == 0 {
            break;
        }
        content_hasher.update(&buffer[..count]);
        offset = offset.saturating_add(count as u64);
        on_progress(offset.min(snapshot.size));
    }
    if !snapshot.verify_unchanged()? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source file changed while hashing",
        ));
    }
    Ok(content_hasher.finalize().to_hex().to_string())
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

    #[test]
    fn cached_identity_rebinds_only_when_source_snapshot_matches() {
        let root =
            std::env::temp_dir().join(format!("locsd-identity-cache-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let path = root.join("source.bin");
        std::fs::write(&path, b"stable-source").unwrap();
        let original = identify_file(&path, "sender", "source.bin").unwrap();
        let cached = original.cache_entry();

        let rebound = restore_cached_identity(SendSource::from_path(&path), &cached)
            .unwrap()
            .unwrap();
        assert_eq!(rebound.content_id, original.content_id);
        assert_eq!(rebound.transfer_key, original.transfer_key);

        std::fs::write(&path, b"changed-source-with-new-size").unwrap();
        assert!(
            restore_cached_identity(SendSource::from_path(&path), &cached)
                .unwrap()
                .is_none()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn prepared_identity_is_immediate_stable_and_hashes_later() {
        let root =
            std::env::temp_dir().join(format!("locsd-prepared-identity-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&root);
        let path = root.join("source.bin");
        std::fs::write(&path, b"deferred content hash").unwrap();
        let first = prepare_source(
            SendSource::from_path(&path),
            "sender",
            "source.bin",
            Some("stable-source-key"),
        )
        .unwrap();
        let second = prepare_source(
            SendSource::from_path(&path),
            "sender",
            "source.bin",
            Some("stable-source-key"),
        )
        .unwrap();
        assert!(first.content_id.is_empty());
        assert_eq!(first.transfer_key, second.transfer_key);
        assert_eq!(
            hash_source_snapshot(&first.snapshot, |_| {}).unwrap(),
            blake3::hash(b"deferred content hash").to_hex().to_string()
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
