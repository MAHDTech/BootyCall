use crate::error::ExtractorError;
use fatfs::{Dir as FatDir, File as FatFile, FileSystem, FsOptions, ReadWriteSeek};
use gpt::GptConfig;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;

pub struct PartitionSlice<F> {
    file: F,
    start: u64,
    len: u64,
    pos: u64,
}

impl<F> PartitionSlice<F> {
    pub fn new(file: F, start: u64, len: u64) -> Self {
        Self {
            file,
            start,
            len,
            pos: 0,
        }
    }
}

impl<F: Read + Seek> Read for PartitionSlice<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Ok(0);
        }
        let max_read = (self.len - self.pos) as usize;
        let to_read = std::cmp::min(buf.len(), max_read);
        self.file.seek(SeekFrom::Start(self.start + self.pos))?;
        let bytes_read = self.file.read(&mut buf[..to_read])?;
        self.pos += bytes_read as u64;
        Ok(bytes_read)
    }
}

impl<F: Seek> Seek for PartitionSlice<F> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // BUG-12: use checked arithmetic instead of raw `as` casts. GPT
        // offsets are untrusted (a crafted image can hand us
        // near-`u64::MAX` values), and `i64::MAX as u64` silently wraps
        // an out-of-range Start offset to a very small number without
        // returning an error.
        let new_pos: i64 = match pos {
            SeekFrom::Start(offset) => i64::try_from(offset).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "Seek offset too large")
            })?,
            SeekFrom::End(offset) => {
                let base = i64::try_from(self.len).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Partition length too large")
                })?;
                base.checked_add(offset).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Seek overflowed partition end")
                })?
            }
            SeekFrom::Current(offset) => {
                let base = i64::try_from(self.pos).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidInput, "Partition position too large")
                })?;
                base.checked_add(offset).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "Seek overflowed current position",
                    )
                })?
            }
        };

        if new_pos < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Invalid seek before start of partition",
            ));
        }
        self.pos = new_pos as u64;
        Ok(self.pos)
    }
}

impl<F: Write + Seek> Write for PartitionSlice<F> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.pos >= self.len {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "Write exceeds partition boundary",
            ));
        }
        let max_write = (self.len - self.pos) as usize;
        let to_write = std::cmp::min(buf.len(), max_write);
        self.file.seek(SeekFrom::Start(self.start + self.pos))?;
        let bytes_written = self.file.write(&buf[..to_write])?;
        self.pos += bytes_written as u64;
        Ok(bytes_written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

pub fn extract_from_disk(
    disk_path: &Path,
    kernel_override: Option<&str>,
    initrd_override: Option<&str>,
    out_kernel_path: &Path,
    out_initrd_path: &Path,
) -> Result<(), ExtractorError> {
    let disk = GptConfig::new()
        .writable(false)
        .open(disk_path)
        .map_err(|e| ExtractorError::Disk(format!("Failed to open GPT disk: {:?}", e)))?;

    let lb_size = *disk.logical_block_size();

    // Iterate through partitions and try to find FAT filesystem
    for partition in disk.partitions().values() {
        if !partition.is_used() {
            continue;
        }

        let start = match partition.bytes_start(lb_size) {
            Ok(s) => s,
            Err(e) => {
                bootycall_log::warn!("Failed to get partition start byte: {:?}", e);
                continue;
            }
        };
        let len = match partition.bytes_len(lb_size) {
            Ok(l) => l,
            Err(e) => {
                bootycall_log::warn!("Failed to get partition length: {:?}", e);
                continue;
            }
        };

        // Re-open file for this slice
        let partition_file = File::open(disk_path)?;
        let slice = PartitionSlice::new(partition_file, start, len);

        if let Ok(fs) = FileSystem::new(slice, FsOptions::new()) {
            let root_dir = fs.root_dir();

            // Try to resolve and extract
            let kernel_extracted = match kernel_override {
                Some(kp) => {
                    if let Ok(mut fat_file) = root_dir.open_file(kp) {
                        let mut out_file = File::create(out_kernel_path)?;
                        io::copy(&mut fat_file, &mut out_file)?;
                        true
                    } else {
                        false
                    }
                }
                None => {
                    if let Ok(Some(mut fat_file)) = find_file_recursive_fat(&root_dir, &|name| {
                        let lower = name.to_lowercase();
                        lower == "vmlinuz" || lower == "bzimage" || lower == "kernel"
                    }) {
                        let mut out_file = File::create(out_kernel_path)?;
                        io::copy(&mut fat_file, &mut out_file)?;
                        true
                    } else {
                        false
                    }
                }
            };

            if !kernel_extracted {
                continue; // Try next partition
            }
            // Once we've extracted a kernel we're committed to this
            // partition — a missing initrd here is a genuine error, not
            // a signal to keep hunting through other partitions and
            // eventually claim the kernel is missing too (BUG-13).

            let initrd_extracted = match initrd_override {
                Some(ip) => {
                    if let Ok(mut fat_file) = root_dir.open_file(ip) {
                        let mut out_file = File::create(out_initrd_path)?;
                        io::copy(&mut fat_file, &mut out_file)?;
                        true
                    } else {
                        false
                    }
                }
                None => {
                    if let Ok(Some(mut fat_file)) = find_file_recursive_fat(&root_dir, &|name| {
                        let lower = name.to_lowercase();
                        lower.contains("initrd") || lower.contains("initramfs")
                    }) {
                        let mut out_file = File::create(out_initrd_path)?;
                        io::copy(&mut fat_file, &mut out_file)?;
                        true
                    } else {
                        false
                    }
                }
            };

            if initrd_extracted {
                return Ok(());
            }
            return Err(ExtractorError::InitrdNotFound);
        }
    }

    Err(ExtractorError::KernelNotFound)
}

fn find_file_recursive_fat<'a, T: ReadWriteSeek>(
    dir: &FatDir<'a, T>,
    filter: &dyn Fn(&str) -> bool,
) -> io::Result<Option<FatFile<'a, T>>> {
    find_file_recursive_fat_bounded(dir, filter, 0)
}

/// Bound FAT directory recursion. `fatfs` abstracts cluster indices away, so
/// a cycle-detection visited-set isn't practical from here — the depth cap is
/// the pragmatic guard against pathological or cyclic images taking down the
/// whole process via stack overflow.
fn find_file_recursive_fat_bounded<'a, T: ReadWriteSeek>(
    dir: &FatDir<'a, T>,
    filter: &dyn Fn(&str) -> bool,
    depth: usize,
) -> io::Result<Option<FatFile<'a, T>>> {
    if crate::iso::depth_exceeded(depth) {
        bootycall_log::warn!(
            "FAT walker hit MAX_DIR_DEPTH={} — refusing to recurse further",
            crate::iso::MAX_DIR_DEPTH
        );
        return Ok(None);
    }
    for entry_res in dir.iter() {
        let entry = entry_res?;
        let name = entry.file_name();
        if entry.is_file() {
            if filter(&name) {
                return Ok(Some(entry.to_file()));
            }
        } else if entry.is_dir() {
            if name == "." || name == ".." {
                continue;
            }
            let subdir = entry.to_dir();
            if let Some(found) = find_file_recursive_fat_bounded(&subdir, filter, depth + 1)? {
                return Ok(Some(found));
            }
        }
    }
    Ok(None)
}
