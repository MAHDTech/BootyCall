pub mod disk;
pub mod error;
pub mod iso;

use crate::error::ExtractorError;
use bootycall_core::config::{Config, HostConfig};
use bootycall_log::{error, info, warn};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Copy `reader` into the artifact at `out_path`, refusing to write more than
/// `max_bytes` when a cap is configured. Returns
/// [`ExtractorError::ArtifactTooLarge`] once the source exceeds the cap — this
/// is what keeps a hostile or genuinely huge initrd from filling the
/// appliance's small eMMC (issue 006). `None` means unbounded.
///
/// The bytes are staged into a `.tmp` sibling (e.g. `kernel.tmp`) and then
/// atomically renamed over `out_path` (same directory, so same filesystem).
/// Concurrent readers — `serve_cache_file` streaming to a booting client, or
/// the `host_cache_ready` health probe — therefore only ever observe either
/// the previous complete artifact or the new complete one, never a truncated
/// in-progress write (issue 085). On failure the staging file is removed and
/// `out_path` is left untouched.
pub(crate) fn copy_capped<R: Read>(
    reader: &mut R,
    out_path: &Path,
    max_bytes: Option<u64>,
) -> Result<(), ExtractorError> {
    let tmp_path = tmp_artifact_path(out_path);
    // `File::create` inside `stage_capped` truncates, so a stale `.tmp` left
    // behind by a crashed earlier extraction is simply overwritten here.
    if let Err(e) = stage_capped(reader, &tmp_path, max_bytes) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp_path, out_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    Ok(())
}

/// Staging path for an artifact write: `cache/<mac>/kernel` ->
/// `cache/<mac>/kernel.tmp`. Always a sibling in the same directory so the
/// final `fs::rename` never crosses a filesystem boundary.
pub(crate) fn tmp_artifact_path(out_path: &Path) -> PathBuf {
    let mut tmp = out_path.as_os_str().to_os_string();
    tmp.push(".tmp");
    PathBuf::from(tmp)
}

/// Stream `reader` into a freshly created file at `tmp_path`, enforcing the
/// optional `max_bytes` cap, then flush it to disk. Callers are responsible
/// for cleaning up `tmp_path` on failure.
fn stage_capped<R: Read>(
    reader: &mut R,
    tmp_path: &Path,
    max_bytes: Option<u64>,
) -> Result<(), ExtractorError> {
    let mut out = fs::File::create(tmp_path)?;
    match max_bytes {
        None => {
            std::io::copy(reader, &mut out)?;
        }
        Some(limit) => {
            let mut written: u64 = 0;
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = reader.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                written += n as u64;
                if written > limit {
                    return Err(ExtractorError::ArtifactTooLarge { limit });
                }
                out.write_all(&buf[..n])?;
            }
        }
    }
    // Flush the staged bytes to disk before the rename publishes them: a
    // power loss shortly after the rename must not surface a truncated
    // artifact at the final path — that is exactly what the staging dance
    // exists to prevent.
    out.sync_all()?;
    Ok(())
}

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

/// Kernel filename heuristic for auto-detection, shared by the ISO and GPT/FAT
/// walkers so the two never drift. Case-insensitive. `Image` (capitalised) is
/// the conventional arm64 kernel name — this is an aarch64 appliance, so it
/// must be recognised alongside the x86 `vmlinuz`/`bzimage` names.
pub(crate) fn is_kernel_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    matches!(lower.as_str(), "vmlinuz" | "bzimage" | "kernel" | "image")
}

/// Initrd filename heuristic (case-insensitive substring match), shared by both
/// walkers.
pub(crate) fn is_initrd_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("initrd") || lower.contains("initramfs")
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

/// Outcome of a full-fleet cache sync. Carries per-host failures so the caller
/// (`main`) can tell "one host's image is temporarily missing" (warn, keep
/// serving) from "every host failed" (surface prominently, feed the health
/// probe) instead of the old `Ok(())`-always behaviour that made a
/// zero-artifact box look healthy — see issues 002 / 020.
#[derive(Debug, Default)]
pub struct SyncSummary {
    /// Number of hosts whose cache is present/valid after the sync.
    pub succeeded: usize,
    /// `(host name, error)` for each host that failed extraction.
    pub failed: Vec<(String, ExtractorError)>,
}

impl SyncSummary {
    /// Total hosts processed (succeeded + failed).
    pub fn total(&self) -> usize {
        self.succeeded + self.failed.len()
    }

    /// True when at least one host was configured and every one failed — the
    /// box will serve no boot artifacts.
    pub fn all_failed(&self) -> bool {
        self.succeeded == 0 && !self.failed.is_empty()
    }

    /// True when some (but not all) hosts failed.
    pub fn partial_failure(&self) -> bool {
        self.succeeded > 0 && !self.failed.is_empty()
    }
}

/// Synchronises the cache directories for all hosts defined in the
/// configuration. Returns `Err` only when the cache directory itself cannot be
/// created (a genuine setup failure); per-host extraction failures are
/// collected into the returned [`SyncSummary`] rather than aborting the sweep.
pub fn sync_all_hosts_cache(config: &Config) -> Result<SyncSummary, ExtractorError> {
    let cache_dir = &config.server.cache_dir;
    if !cache_dir.exists() {
        fs::create_dir_all(cache_dir)?;
    }

    let max_artifact_bytes = config.server.max_artifact_bytes;
    let mut summary = SyncSummary::default();
    for host in &config.hosts {
        match sync_host_cache(host, cache_dir, max_artifact_bytes) {
            Ok(()) => summary.succeeded += 1,
            Err(e) => {
                error!(
                    "Failed to sync cache for host {} (MAC: {}): {:?}",
                    host.name, host.mac, e
                );
                summary.failed.push((host.name.clone(), e));
            }
        }
    }

    if summary.all_failed() {
        bootycall_log::event!(
            "extract_sync_all_failed",
            hosts = summary.total() as u64,
            failed = summary.failed.len() as u64,
        );
    } else if summary.partial_failure() {
        bootycall_log::event!(
            "extract_sync_partial",
            hosts = summary.total() as u64,
            succeeded = summary.succeeded as u64,
            failed = summary.failed.len() as u64,
        );
    }

    Ok(summary)
}

/// Read-only readiness check for a host's cache: are non-empty `kernel` and
/// `initrd` artifacts present under `cache_dir/<mac>/`?
///
/// Used by the HTTP `/api/health` probe (issue 032) to answer "can the box
/// serve this host's boot artifacts right now". It does NOT stat the source
/// image or trigger extraction, so it is cheap to poll and does not report
/// degraded merely because a source ISO is momentarily unreachable.
///
/// Blocking: this performs two synchronous `std::fs` stats per call. Like the
/// rest of this crate it is written for the synchronous extraction path;
/// async callers must not invoke it directly on a tokio worker thread — wrap
/// the sweep in `tokio::task::spawn_blocking`, as the HTTP `/api/health`
/// handler does (issue 070).
pub fn host_cache_ready(mac: &str, cache_dir: &Path) -> bool {
    let host_cache_dir = cache_dir.join(mac);
    let nonempty = |name: &str| {
        fs::metadata(host_cache_dir.join(name))
            .map(|m| m.is_file() && m.len() > 0)
            .unwrap_or(false)
    };
    nonempty("kernel") && nonempty("initrd")
}

/// Synchronises the cache directory for a single host. `max_artifact_bytes`
/// caps the size of each extracted kernel/initrd (`None` = unbounded).
pub fn sync_host_cache(
    host: &HostConfig,
    cache_dir: &Path,
    max_artifact_bytes: Option<u64>,
) -> Result<(), ExtractorError> {
    let host_cache_dir = cache_dir.join(&host.mac);
    let metadata_path = host_cache_dir.join("metadata.json");
    let kernel_path = host_cache_dir.join("kernel");
    let initrd_path = host_cache_dir.join("initrd");
    // Legacy 3-line text metadata written by earlier builds; if we see one,
    // treat the cache as stale so re-extraction produces a JSON metadata file
    // going forward. The stale legacy file is removed below.
    let legacy_metadata_path = host_cache_dir.join("metadata.txt");

    // Get current image metadata. Single stat (no `exists()` pre-check): a
    // `NotFound` maps to the friendlier `ImageNotFound`, other stat errors
    // propagate faithfully. This closes the time-of-check/time-of-use race
    // where the image vanished between an `exists()` check and `metadata()`.
    let file_meta = match fs::metadata(&host.image_path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(ExtractorError::ImageNotFound(host.image_path.clone()));
        }
        Err(e) => return Err(ExtractorError::Io(e)),
    };
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

    // Sweep `.tmp` staging files left behind by a crash mid-extraction. A
    // fresh extraction would overwrite them anyway, but a cache hit below
    // would otherwise leave the crash debris lingering forever. `NotFound`
    // (the common case) is deliberately ignored.
    for tmp in [
        tmp_artifact_path(&kernel_path),
        tmp_artifact_path(&initrd_path),
    ] {
        let _ = fs::remove_file(&tmp);
    }

    if cache_valid {
        info!("Cache is valid for host {} (MAC: {})", host.name, host.mac);
        bootycall_log::event!(
            "extract_cache_hit",
            host = %host.name,
            mac = %host.mac,
            image = %host.image_path.display(),
        );
        return Ok(());
    }

    info!(
        "Cache stale or missing for host {} (MAC: {}). Extracting files...",
        host.name, host.mac
    );
    bootycall_log::event!(
        "extract_cache_miss",
        host = %host.name,
        mac = %host.mac,
        image = %host.image_path.display(),
    );
    let extract_started = std::time::Instant::now();

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
        max_artifact_bytes,
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
                max_artifact_bytes,
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
            bootycall_log::event!(
                "extract_complete",
                host = %host.name,
                mac = %host.mac,
                image = %host.image_path.display(),
                duration_ms = extract_started.elapsed().as_millis() as u64,
            );
            Ok(())
        }
        Err(e) => {
            bootycall_log::event!(
                "extract_failed",
                host = %host.name,
                mac = %host.mac,
                image = %host.image_path.display(),
                error = %e,
            );
            // Clean up. `copy_capped` never leaves a torn final artifact,
            // but a mixed outcome (new kernel already renamed into place,
            // initrd failed) would leave a mismatched pair — remove the
            // final artifacts and metadata so the cache reads as plainly
            // absent rather than falsely ready. Also drop any staging file
            // stranded by a failed rename.
            let _ = fs::remove_file(&kernel_path);
            let _ = fs::remove_file(&initrd_path);
            let _ = fs::remove_file(&metadata_path);
            let _ = fs::remove_file(tmp_artifact_path(&kernel_path));
            let _ = fs::remove_file(tmp_artifact_path(&initrd_path));
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{copy_capped, is_initrd_name, is_kernel_name, tmp_artifact_path};
    use crate::error::ExtractorError;
    use std::fs;
    use std::io::Read as _;
    use std::path::{Path, PathBuf};

    #[test]
    fn tmp_artifact_path_appends_tmp_suffix() {
        assert_eq!(
            tmp_artifact_path(Path::new("/cache/aa:bb:cc:dd:ee:ff/kernel")),
            PathBuf::from("/cache/aa:bb:cc:dd:ee:ff/kernel.tmp")
        );
        assert_eq!(
            tmp_artifact_path(Path::new("initrd")),
            PathBuf::from("initrd.tmp")
        );
    }

    #[test]
    fn copy_capped_publishes_complete_artifact_and_removes_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("kernel");
        fs::write(&out, b"old_complete_artifact").unwrap();

        let mut src: &[u8] = b"new_artifact_bytes";
        copy_capped(&mut src, &out, None).unwrap();

        assert_eq!(fs::read(&out).unwrap(), b"new_artifact_bytes");
        assert!(
            !tmp_artifact_path(&out).exists(),
            "staging file must not survive a successful copy"
        );

        // Same behaviour with a (generous) cap configured.
        let mut src: &[u8] = b"capped_artifact_bytes";
        copy_capped(&mut src, &out, Some(1024)).unwrap();
        assert_eq!(fs::read(&out).unwrap(), b"capped_artifact_bytes");
        assert!(!tmp_artifact_path(&out).exists());
    }

    #[test]
    fn copy_capped_over_cap_leaves_existing_artifact_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("initrd");
        fs::write(&out, b"old_complete_artifact").unwrap();

        let mut src = std::io::repeat(0u8).take(20);
        let err = copy_capped(&mut src, &out, Some(10)).unwrap_err();
        assert!(matches!(
            err,
            ExtractorError::ArtifactTooLarge { limit: 10 }
        ));

        assert_eq!(
            fs::read(&out).unwrap(),
            b"old_complete_artifact",
            "a failed re-extraction must not touch the live artifact"
        );
        assert!(
            !tmp_artifact_path(&out).exists(),
            "failed staging file must be cleaned up"
        );
    }

    #[test]
    fn copy_capped_mid_copy_io_error_leaves_existing_artifact_untouched() {
        /// Yields a few bytes, then fails — simulating a source image that
        /// dies partway through extraction (the crash-mid-copy scenario).
        struct FailAfterSome {
            sent: bool,
        }
        impl std::io::Read for FailAfterSome {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                if self.sent {
                    return Err(std::io::Error::other("simulated mid-copy failure"));
                }
                self.sent = true;
                let n = buf.len().min(7);
                buf[..n].copy_from_slice(&b"partial"[..n]);
                Ok(n)
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("kernel");
        fs::write(&out, b"old_complete_artifact").unwrap();

        let err = copy_capped(&mut FailAfterSome { sent: false }, &out, Some(1024)).unwrap_err();
        assert!(matches!(err, ExtractorError::Io(_)));

        assert_eq!(
            fs::read(&out).unwrap(),
            b"old_complete_artifact",
            "the partial staging write must never reach the final path"
        );
        assert!(!tmp_artifact_path(&out).exists());
    }

    #[test]
    fn copy_capped_overwrites_stale_tmp_from_previous_crash() {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("kernel");
        // A crash between staging and rename leaves a `.tmp` behind; the next
        // extraction must overwrite it, not append or fail.
        fs::write(tmp_artifact_path(&out), b"stale crash leftover").unwrap();

        let mut src: &[u8] = b"fresh";
        copy_capped(&mut src, &out, None).unwrap();

        assert_eq!(fs::read(&out).unwrap(), b"fresh");
        assert!(!tmp_artifact_path(&out).exists());
    }

    #[test]
    fn kernel_name_heuristic() {
        for good in [
            "vmlinuz", "bzImage", "BZIMAGE", "kernel", "Image", "image", "IMAGE",
        ] {
            assert!(is_kernel_name(good), "{good:?} should match a kernel");
        }
        for bad in [
            "initrd.img",
            "grub.cfg",
            "vmlinuz.old",
            "images",
            "kernel.efi",
            "",
        ] {
            assert!(!is_kernel_name(bad), "{bad:?} should NOT match a kernel");
        }
    }

    #[test]
    fn initrd_name_heuristic() {
        for good in [
            "initrd",
            "initrd.img",
            "initramfs-linux.img",
            "INITRAMFS",
            "boot-initrd.gz",
        ] {
            assert!(is_initrd_name(good), "{good:?} should match an initrd");
        }
        for bad in ["vmlinuz", "kernel", "root.squashfs", ""] {
            assert!(!is_initrd_name(bad), "{bad:?} should NOT match an initrd");
        }
    }
}
