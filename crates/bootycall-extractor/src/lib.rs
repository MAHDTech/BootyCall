pub mod disk;
pub mod error;
pub mod iso;

use crate::error::ExtractorError;
use bootycall_core::config::{Config, HostConfig};
use bootycall_log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Persisted alongside the extracted kernel/initrd so we can decide whether
/// re-extraction is needed. The set of fields is the full cache key: any
/// field drifting between what was extracted and what the host config now
/// says forces re-extraction.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct CacheMetadata {
    image_path: PathBuf,
    mtime_secs: u64,
    size: u64,
    /// Explicit override for the kernel path inside the image. `None` means
    /// "auto-detect", which is a distinct cache key from any concrete value.
    #[serde(default)]
    kernel_override: Option<String>,
    /// Same idea for initrd.
    #[serde(default)]
    initrd_override: Option<String>,
}

impl CacheMetadata {
    fn write_to_file(&self, path: &Path) -> std::io::Result<()> {
        let content = serde_json::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        fs::write(path, content)
    }

    fn read_from_file(path: &Path) -> Result<Self, std::io::Error> {
        let content = fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

/// Synchronises the cache directories for all hosts defined in the configuration.
pub fn sync_all_hosts_cache(config: &Config) -> Result<(), ExtractorError> {
    let cache_dir = &config.server.cache_dir;
    if !cache_dir.exists() {
        fs::create_dir_all(cache_dir)?;
    }

    for host in &config.hosts {
        if let Err(e) = sync_host_cache(host, cache_dir) {
            error!(
                "Failed to sync cache for host {} (MAC: {}): {:?}",
                host.name, host.mac, e
            );
        }
    }

    Ok(())
}

/// Synchronises the cache directory for a single host.
pub fn sync_host_cache(host: &HostConfig, cache_dir: &Path) -> Result<(), ExtractorError> {
    let host_cache_dir = cache_dir.join(&host.mac);
    let metadata_path = host_cache_dir.join("metadata.json");
    let kernel_path = host_cache_dir.join("kernel");
    let initrd_path = host_cache_dir.join("initrd");
    // Legacy 3-line text metadata written by earlier builds; if we see one,
    // treat the cache as stale so re-extraction produces a JSON metadata file
    // going forward. The stale legacy file is removed below.
    let legacy_metadata_path = host_cache_dir.join("metadata.txt");

    // Get current image metadata
    if !host.image_path.exists() {
        return Err(ExtractorError::ImageNotFound(host.image_path.clone()));
    }

    let file_meta = fs::metadata(&host.image_path)?;
    let current_mtime = file_meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(std::io::Error::other)?
        .as_secs();
    let current_size = file_meta.len();

    // Check if cache is still valid. The override paths participate in the
    // cache key so that editing `kernel_path` / `initrd_path` in the host
    // config invalidates the cache — the previous 3-line text format only
    // compared image path/mtime/size and served old artefacts across an
    // override change.
    let mut cache_valid = false;
    if metadata_path.exists() && kernel_path.exists() && initrd_path.exists() {
        cache_valid = CacheMetadata::read_from_file(&metadata_path).is_ok_and(|meta| {
            meta.image_path == host.image_path
                && meta.mtime_secs == current_mtime
                && meta.size == current_size
                && meta.kernel_override.as_deref() == host.kernel_path.as_deref()
                && meta.initrd_override.as_deref() == host.initrd_path.as_deref()
        });
    }

    // Drop the legacy text metadata file if it lingers alongside the JSON
    // one — it's ignored by the new reader and would confuse manual audits.
    if legacy_metadata_path.exists() {
        let _ = fs::remove_file(&legacy_metadata_path);
    }

    if cache_valid {
        info!("Cache is valid for host {} (MAC: {})", host.name, host.mac);
        return Ok(());
    }

    info!(
        "Cache stale or missing for host {} (MAC: {}). Extracting files...",
        host.name, host.mac
    );

    // Ensure cache folder exists
    if !host_cache_dir.exists() {
        fs::create_dir_all(&host_cache_dir)?;
    }

    // Try extracting as ISO9660 first
    let extraction_result = iso::extract_from_iso(
        &host.image_path,
        host.kernel_path.as_deref(),
        host.initrd_path.as_deref(),
        &kernel_path,
        &initrd_path,
    );

    // If ISO failed, try as GPT/FAT image
    let extraction_result = match extraction_result {
        Ok(()) => Ok(()),
        Err(e) => {
            warn!(
                "ISO extraction failed ({:?}) for {}, trying GPT/FAT partition extraction...",
                e,
                host.image_path.display()
            );
            disk::extract_from_disk(
                &host.image_path,
                host.kernel_path.as_deref(),
                host.initrd_path.as_deref(),
                &kernel_path,
                &initrd_path,
            )
        }
    };

    match extraction_result {
        Ok(()) => {
            // Write metadata
            let meta = CacheMetadata {
                image_path: host.image_path.clone(),
                mtime_secs: current_mtime,
                size: current_size,
                kernel_override: host.kernel_path.clone(),
                initrd_override: host.initrd_path.clone(),
            };
            meta.write_to_file(&metadata_path)?;
            info!(
                "Successfully extracted kernel and initrd for host {} (MAC: {})",
                host.name, host.mac
            );
            Ok(())
        }
        Err(e) => {
            // Clean up potentially incomplete cache files
            let _ = fs::remove_file(&kernel_path);
            let _ = fs::remove_file(&initrd_path);
            let _ = fs::remove_file(&metadata_path);
            Err(e)
        }
    }
}
