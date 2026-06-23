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
        find_file_recursive(&iso.root, &|name| {
            let lower = name.to_lowercase();
            lower == "vmlinuz" || lower == "bzimage" || lower == "kernel"
        })?
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
        find_file_recursive(&iso.root, &|name| {
            let lower = name.to_lowercase();
            lower.contains("initrd") || lower.contains("initramfs")
        })?
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

fn find_file_recursive<T: ISO9660Reader + 'static>(
    dir: &ISODirectory<T>,
    filter: &dyn Fn(&str) -> bool,
) -> Result<Option<ISOFile<T>>, ExtractorError> {
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
                if let Some(found) = find_file_recursive(&subdir, filter)? {
                    return Ok(Some(found));
                }
            }
        }
    }
    Ok(None)
}
