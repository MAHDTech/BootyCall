use crate::error::ExtractorError;
use iso9660::{DirectoryEntry, ISO9660, ISO9660Reader, ISODirectory, ISOFile};
use std::fs::File;
use std::io;
use std::path::Path;

pub fn extract_from_iso(
    iso_path: &Path,
    kernel_override: Option<&str>,
    initrd_override: Option<&str>,
    out_kernel_path: &Path,
    out_initrd_path: &Path,
) -> Result<(), ExtractorError> {
    let file = File::open(iso_path)?;
    let iso = ISO9660::new(file).map_err(|e| ExtractorError::Iso(format!("{:?}", e)))?;

    // 1. Resolve Kernel
    let kernel_file = if let Some(kp) = kernel_override {
        // Open exact path
        match iso
            .open(kp)
            .map_err(|e| ExtractorError::Iso(format!("{:?}", e)))?
        {
            Some(DirectoryEntry::File(file)) => file,
            _ => return Err(ExtractorError::KernelNotFound),
        }
    } else {
        // Search recursively
        find_file_recursive(&iso.root, &|name| crate::is_kernel_name(name))?
            .ok_or(ExtractorError::KernelNotFound)?
    };

    // 2. Resolve Initrd
    let initrd_file = if let Some(ip) = initrd_override {
        // Open exact path
        match iso
            .open(ip)
            .map_err(|e| ExtractorError::Iso(format!("{:?}", e)))?
        {
            Some(DirectoryEntry::File(file)) => file,
            _ => return Err(ExtractorError::InitrdNotFound),
        }
    } else {
        // Search recursively
        find_file_recursive(&iso.root, &|name| crate::is_initrd_name(name))?
            .ok_or(ExtractorError::InitrdNotFound)?
    };

    // 3. Extract Kernel
    {
        let mut reader = kernel_file.read();
        let mut out_file = File::create(out_kernel_path)?;
        io::copy(&mut reader, &mut out_file)?;
    }

    // 4. Extract Initrd
    {
        let mut reader = initrd_file.read();
        let mut out_file = File::create(out_initrd_path)?;
        io::copy(&mut reader, &mut out_file)?;
    }

    Ok(())
}

/// Maximum directory nesting an ISO/FAT walk will traverse. A crafted or
/// cyclic image otherwise stack-overflows and aborts the whole single-binary
/// suite before the OLED even shows an error.
pub(crate) const MAX_DIR_DEPTH: usize = 64;

/// Returns `true` when the current directory-walk depth has hit the cap.
/// Both walkers (`find_file_recursive_bounded` and
/// `find_file_recursive_fat_bounded`) call this so the check is uniform and
/// unit-testable without needing a synthetic ISO or FAT image.
pub(crate) fn depth_exceeded(depth: usize) -> bool {
    depth >= MAX_DIR_DEPTH
}

fn find_file_recursive<T: ISO9660Reader + 'static>(
    dir: &ISODirectory<T>,
    filter: &dyn Fn(&str) -> bool,
) -> Result<Option<ISOFile<T>>, ExtractorError> {
    find_file_recursive_bounded(dir, filter, 0)
}

fn find_file_recursive_bounded<T: ISO9660Reader + 'static>(
    dir: &ISODirectory<T>,
    filter: &dyn Fn(&str) -> bool,
    depth: usize,
) -> Result<Option<ISOFile<T>>, ExtractorError> {
    if depth_exceeded(depth) {
        bootycall_log::warn!(
            "ISO walker hit MAX_DIR_DEPTH={} — refusing to recurse further",
            MAX_DIR_DEPTH
        );
        return Ok(None);
    }
    for entry_res in dir.contents() {
        let entry = entry_res.map_err(|e| ExtractorError::Iso(format!("{:?}", e)))?;
        match entry {
            DirectoryEntry::File(file) => {
                if filter(&file.identifier) {
                    return Ok(Some(file));
                }
            }
            DirectoryEntry::Directory(subdir) => {
                if subdir.identifier == "." || subdir.identifier == ".." {
                    continue;
                }
                if let Some(found) = find_file_recursive_bounded(&subdir, filter, depth + 1)? {
                    return Ok(Some(found));
                }
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_guard_allows_shallow_recursion() {
        assert!(!depth_exceeded(0));
        assert!(!depth_exceeded(1));
        assert!(!depth_exceeded(MAX_DIR_DEPTH - 1));
    }

    #[test]
    fn depth_guard_stops_at_or_past_cap() {
        // At the cap the walker must refuse to recurse further; anything
        // deeper is a hostile or cyclic image.
        assert!(depth_exceeded(MAX_DIR_DEPTH));
        assert!(depth_exceeded(MAX_DIR_DEPTH + 1));
        assert!(depth_exceeded(usize::MAX));
    }

    #[test]
    fn depth_cap_is_reasonable() {
        // Real ISO/FAT trees rarely go past ~10 levels; guard against a
        // future edit that either accidentally removes the cap or sets it
        // absurdly low. Evaluated in a const block so clippy's
        // `assertions_on_constants` accepts the compile-time check.
        const _: () = assert!(MAX_DIR_DEPTH >= 16);
        const _: () = assert!(MAX_DIR_DEPTH <= 1024);
    }
}
