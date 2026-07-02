pub mod disk;
pub mod error;
pub mod iso;

use crate::error::ExtractorError;
use bootycall_core::config::{Config, HostConfig};
use bootycall_log::{error, info, warn};
use std::fs;
use std::path::{Path, PathBuf};

struct CacheMetadata {
    image_path: PathBuf,
    mtime_secs: u64,
    size: u64,
}

impl CacheMetadata {
    fn write_to_file(&self, path: &Path) -> std::io::Result<()> {
        let content = format!(
            "{}\n{}\n{}\n",
            self.image_path.display(),
            self.mtime_secs,
            self.size
        );
        fs::write(path, content)
    }

    fn read_from_file(path: &Path) -> Result<Self, std::io::Error> {
        let content = fs::read_to_string(path)?;
        let lines: Vec<&str> = content.lines().collect();
        if lines.len() < 3 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Incomplete cache metadata",
            ));
        }
        let image_path = PathBuf::from(lines[0]);
        let mtime_secs = lines[1]
            .parse::<u64>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        let size = lines[2]
            .parse::<u64>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Self {
            image_path,
            mtime_secs,
            size,
        })
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
    let metadata_path = host_cache_dir.join("metadata.txt");
    let kernel_path = host_cache_dir.join("kernel");
    let initrd_path = host_cache_dir.join("initrd");

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

    // Check if cache is still valid
    let mut cache_valid = false;
    if metadata_path.exists() && kernel_path.exists() && initrd_path.exists() {
        cache_valid = CacheMetadata::read_from_file(&metadata_path).is_ok_and(|meta| {
            meta.image_path == host.image_path
                && meta.mtime_secs == current_mtime
                && meta.size == current_size
        });
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
