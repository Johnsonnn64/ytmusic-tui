//! Shared cover download/cache helpers for terminal artwork and notifications.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use sha1::{Digest, Sha1};

const MAX_IMAGE_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const MAX_CACHE_FILES: usize = 256;
pub(crate) const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

pub(crate) fn cache_directory() -> Option<PathBuf> {
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .map(|root| root.join("ytmusic-tui/artwork"))
}

pub(crate) fn cache_path(directory: &Path, url: &str) -> PathBuf {
    let hash: String = Sha1::digest(url.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    directory.join(format!("{hash}.img"))
}

pub(crate) async fn download(client: &reqwest::Client, url: &str, path: &Path) -> Result<(), ()> {
    let parsed = reqwest::Url::parse(url).map_err(|_| ())?;
    if !matches!(parsed.scheme(), "https" | "http") {
        return Err(());
    }
    let mut response = client
        .get(parsed)
        .send()
        .await
        .map_err(|_| ())?
        .error_for_status()
        .map_err(|_| ())?;
    if response
        .content_length()
        .is_some_and(|size| size > MAX_IMAGE_BYTES as u64)
    {
        return Err(());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| ())? {
        if bytes.len() + chunk.len() > MAX_IMAGE_BYTES {
            return Err(());
        }
        bytes.extend_from_slice(&chunk);
    }
    // Thumbnails are JPEG/PNG/WebP. Reject error pages and animated GIFs before
    // passing data to the external renderer.
    if !is_thumbnail(&bytes) {
        return Err(());
    }
    let mut file = tempfile::NamedTempFile::new_in(path.parent().ok_or(())?).map_err(|_| ())?;
    file.write_all(&bytes).map_err(|_| ())?;
    file.persist(path).map_err(|_| ())?;
    Ok(())
}

pub(crate) fn is_thumbnail(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || (bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP"))
}

/// Keep our own cache bounded. ueberzugpp's separate resized-image disk cache
/// is disabled; currently visible files are protected during eviction.
pub(crate) fn trim_cache(directory: &Path, protected: &HashSet<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut files: Vec<_> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "img") {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            metadata.is_file().then(|| {
                (
                    path,
                    metadata.len(),
                    metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                )
            })
        })
        .collect();
    files.sort_by_key(|(_, _, modified)| *modified);
    let mut count = files.len();
    let mut bytes: u64 = files.iter().map(|(_, size, _)| size).sum();
    for (path, size, _) in files {
        if count <= MAX_CACHE_FILES && bytes <= MAX_CACHE_BYTES {
            break;
        }
        if !protected.contains(&path) && fs::remove_file(path).is_ok() {
            count -= 1;
            bytes = bytes.saturating_sub(size);
        }
    }
}
