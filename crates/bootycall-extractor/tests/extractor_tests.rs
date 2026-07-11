use bootycall_core::config::{Config, HostConfig, ServerConfig};
use std::fs::{self, File};
use std::io::Write;
use tempfile::tempdir;

#[test]
fn test_gpt_fat_extraction_and_caching() {
    let dir = tempdir().unwrap();
    let disk_path = dir.path().join("test_disk.img");
    let cache_dir = dir.path().join("cache");

    // 1. Create a 5MB blank disk image
    let disk_size = 5 * 1024 * 1024;
    {
        let f = File::create(&disk_path).unwrap();
        f.set_len(disk_size as u64).unwrap();
    }

    // 2. Write protective MBR
    {
        let mut f = File::options()
            .read(true)
            .write(true)
            .open(&disk_path)
            .unwrap();
        let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
            std::convert::TryFrom::try_from((disk_size / 512) - 1).unwrap(),
        );
        mbr.overwrite_lba0(&mut f).unwrap();
    }

    // 3. Initialize GptDisk
    let start_byte;
    let len_byte;
    {
        let f = File::options()
            .read(true)
            .write(true)
            .open(&disk_path)
            .unwrap();
        let mut gdisk = gpt::GptConfig::default()
            .writable(true)
            .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
            .create_from_device(Box::new(f), None)
            .unwrap();

        gdisk
            .update_partitions(std::collections::BTreeMap::new())
            .unwrap();

        // Add 4MB partition
        gdisk
            .add_partition(
                "boot",
                4 * 1024 * 1024,
                gpt::partition_types::BASIC,
                0,
                None,
            )
            .unwrap();

        let partition = gdisk.partitions().get(&1).unwrap();
        start_byte = partition
            .bytes_start(gpt::disk::LogicalBlockSize::Lb512)
            .unwrap();
        len_byte = partition
            .bytes_len(gpt::disk::LogicalBlockSize::Lb512)
            .unwrap();

        gdisk.write().unwrap();
    }

    // 4. Format partition as FAT12/FAT16 and write dummy kernel/initrd
    {
        let part_file = File::options()
            .read(true)
            .write(true)
            .open(&disk_path)
            .unwrap();
        let mut slice =
            bootycall_extractor::disk::PartitionSlice::new(part_file, start_byte, len_byte);

        let opts = fatfs::FormatVolumeOptions::new();
        fatfs::format_volume(&mut slice, opts).unwrap();

        let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
        let root_dir = fs.root_dir();

        root_dir.create_dir("boot").unwrap();

        let mut k_file = root_dir.create_file("boot/vmlinuz").unwrap();
        k_file.write_all(b"kernel_test_payload").unwrap();

        let mut i_file = root_dir.create_file("boot/initrd.img").unwrap();
        i_file.write_all(b"initrd_test_payload").unwrap();
    }

    // 5. Test caching and extraction engine
    let host = HostConfig {
        mac: "00:11:22:33:44:55".to_string(),
        name: "test-host".to_string(),
        image_path: disk_path.clone(),
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };

    // First sync: extracts because cache is missing
    bootycall_extractor::sync_host_cache(&host, &cache_dir, None).unwrap();

    let host_cache_dir = cache_dir.join(&host.mac);
    let cached_kernel = host_cache_dir.join("kernel");
    let cached_initrd = host_cache_dir.join("initrd");
    let cached_metadata = host_cache_dir.join("metadata.json");

    assert!(cached_kernel.exists());
    assert!(cached_initrd.exists());
    assert!(cached_metadata.exists());

    assert_eq!(
        fs::read_to_string(&cached_kernel).unwrap(),
        "kernel_test_payload"
    );
    assert_eq!(
        fs::read_to_string(&cached_initrd).unwrap(),
        "initrd_test_payload"
    );

    // Second sync: should skip extraction (cached)
    // We can check if it returns Ok
    bootycall_extractor::sync_host_cache(&host, &cache_dir, None).unwrap();

    // Check that files are still there
    assert_eq!(
        fs::read_to_string(&cached_kernel).unwrap(),
        "kernel_test_payload"
    );
}

#[test]
fn test_sync_host_cache_missing_image() {
    let dir = tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let bogus_image_path = dir.path().join("does_not_exist.iso");

    let host = HostConfig {
        mac: "aa:bb:cc:dd:ee:ff".to_string(),
        name: "missing-image-host".to_string(),
        image_path: bogus_image_path.clone(),
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };

    let result = bootycall_extractor::sync_host_cache(&host, &cache_dir, None);
    assert!(result.is_err(), "Expected an error for a missing image");

    let err = result.unwrap_err();
    let err_msg = format!("{err}");
    assert!(
        err_msg.contains("Image file not found"),
        "Error should indicate image not found, got: {err_msg}"
    );
    assert!(
        err_msg.contains("does_not_exist.iso"),
        "Error should contain the missing file name, got: {err_msg}"
    );
}

/// Build a small FAT-formatted GPT image with two kernels + two initrds so
/// tests can flip a host's `kernel_path`/`initrd_path` overrides and verify
/// the cache re-extracts.
fn build_dual_kernel_gpt_image(disk_path: &std::path::Path) {
    let disk_size = 5 * 1024 * 1024;
    {
        let f = File::create(disk_path).unwrap();
        f.set_len(disk_size as u64).unwrap();
    }
    {
        let mut f = File::options()
            .read(true)
            .write(true)
            .open(disk_path)
            .unwrap();
        let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
            std::convert::TryFrom::try_from((disk_size / 512) - 1).unwrap(),
        );
        mbr.overwrite_lba0(&mut f).unwrap();
    }
    let (start_byte, len_byte) = {
        let f = File::options()
            .read(true)
            .write(true)
            .open(disk_path)
            .unwrap();
        let mut gdisk = gpt::GptConfig::default()
            .writable(true)
            .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
            .create_from_device(Box::new(f), None)
            .unwrap();
        gdisk
            .update_partitions(std::collections::BTreeMap::new())
            .unwrap();
        gdisk
            .add_partition(
                "boot",
                4 * 1024 * 1024,
                gpt::partition_types::BASIC,
                0,
                None,
            )
            .unwrap();
        let p = gdisk.partitions().get(&1).unwrap();
        let s = p.bytes_start(gpt::disk::LogicalBlockSize::Lb512).unwrap();
        let l = p.bytes_len(gpt::disk::LogicalBlockSize::Lb512).unwrap();
        gdisk.write().unwrap();
        (s, l)
    };
    let part_file = File::options()
        .read(true)
        .write(true)
        .open(disk_path)
        .unwrap();
    let mut slice = bootycall_extractor::disk::PartitionSlice::new(part_file, start_byte, len_byte);
    fatfs::format_volume(&mut slice, fatfs::FormatVolumeOptions::new()).unwrap();
    let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
    let root_dir = fs.root_dir();
    root_dir.create_dir("boot").unwrap();
    root_dir
        .create_file("boot/vmlinuz")
        .unwrap()
        .write_all(b"default_kernel")
        .unwrap();
    root_dir
        .create_file("boot/initrd.img")
        .unwrap()
        .write_all(b"default_initrd")
        .unwrap();
    root_dir
        .create_file("boot/vmlinuz-override")
        .unwrap()
        .write_all(b"override_kernel")
        .unwrap();
    root_dir
        .create_file("boot/initrd-override.img")
        .unwrap()
        .write_all(b"override_initrd")
        .unwrap();
}

#[test]
fn test_cache_invalidated_on_kernel_path_change() {
    let dir = tempdir().unwrap();
    let disk_path = dir.path().join("test_disk.img");
    let cache_dir = dir.path().join("cache");
    build_dual_kernel_gpt_image(&disk_path);

    let host = HostConfig {
        mac: "00:11:22:33:44:66".to_string(),
        name: "override-host".to_string(),
        image_path: disk_path.clone(),
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };

    // First sync: default (auto-detect) — should pick up "vmlinuz"/"initrd.img".
    bootycall_extractor::sync_host_cache(&host, &cache_dir, None).unwrap();

    let host_cache_dir = cache_dir.join(&host.mac);
    let cached_kernel = host_cache_dir.join("kernel");
    assert_eq!(
        fs::read_to_string(&cached_kernel).unwrap(),
        "default_kernel"
    );

    // Now flip the override to point at the alternative kernel/initrd. The
    // image path/mtime/size are unchanged, so a cache keyed only on those
    // three fields (the old text format) would incorrectly reuse the
    // "default_kernel" file. The fix makes the overrides part of the key.
    let host_with_override = HostConfig {
        kernel_path: Some("boot/vmlinuz-override".to_string()),
        initrd_path: Some("boot/initrd-override.img".to_string()),
        ..host.clone()
    };

    bootycall_extractor::sync_host_cache(&host_with_override, &cache_dir, None).unwrap();

    assert_eq!(
        fs::read_to_string(&cached_kernel).unwrap(),
        "override_kernel",
        "cache should have re-extracted the override kernel"
    );
    let cached_initrd = host_cache_dir.join("initrd");
    assert_eq!(
        fs::read_to_string(&cached_initrd).unwrap(),
        "override_initrd",
        "cache should have re-extracted the override initrd"
    );

    // Third sync with the override still set — must be a cache hit (no
    // re-extraction), proving the JSON metadata correctly records the
    // override and matches on the next run.
    let before = fs::metadata(&cached_kernel).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    bootycall_extractor::sync_host_cache(&host_with_override, &cache_dir, None).unwrap();
    let after = fs::metadata(&cached_kernel).unwrap().modified().unwrap();
    assert_eq!(before, after, "second override sync must not re-extract");
}

#[test]
fn test_stale_tmp_artifacts_swept_on_cache_hit() {
    // Extraction stages artifacts as `kernel.tmp`/`initrd.tmp` before an
    // atomic rename (issue 085). A crash mid-extraction can strand those
    // staging files; a later sync that is a cache HIT (no re-extraction to
    // overwrite them) must still sweep the debris while leaving the live
    // artifacts untouched.
    let dir = tempdir().unwrap();
    let disk_path = dir.path().join("test_disk.img");
    let cache_dir = dir.path().join("cache");
    build_dual_kernel_gpt_image(&disk_path);

    let host = HostConfig {
        mac: "00:11:22:33:44:ee".to_string(),
        name: "tmp-sweep-host".to_string(),
        image_path: disk_path.clone(),
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };

    bootycall_extractor::sync_host_cache(&host, &cache_dir, None).unwrap();

    let host_cache_dir = cache_dir.join(&host.mac);
    let kernel_tmp = host_cache_dir.join("kernel.tmp");
    let initrd_tmp = host_cache_dir.join("initrd.tmp");
    assert!(
        !kernel_tmp.exists() && !initrd_tmp.exists(),
        "no staging files may survive a successful extraction"
    );

    // Plant crash debris, then re-sync (cache hit) — it must be swept.
    fs::write(&kernel_tmp, b"crash leftover").unwrap();
    fs::write(&initrd_tmp, b"crash leftover").unwrap();
    bootycall_extractor::sync_host_cache(&host, &cache_dir, None).unwrap();
    assert!(!kernel_tmp.exists(), "stale kernel.tmp not swept");
    assert!(!initrd_tmp.exists(), "stale initrd.tmp not swept");

    // The live artifacts are untouched by the sweep.
    assert_eq!(
        fs::read_to_string(host_cache_dir.join("kernel")).unwrap(),
        "default_kernel"
    );
    assert_eq!(
        fs::read_to_string(host_cache_dir.join("initrd")).unwrap(),
        "default_initrd"
    );
}

fn server_config(cache_dir: &std::path::Path) -> ServerConfig {
    ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: std::path::PathBuf::from("./tftpboot"),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: cache_dir.to_path_buf(),
        static_dir: "./static".into(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        default_bootloader_bios: "boot/x64/undionly.kpxe".to_string(),
        oled_enabled: false,
        oled_brightness: 255,
        api_token: None,
        max_artifact_bytes: None,
    }
}

#[test]
fn test_sync_all_hosts_cache_partial_failure() {
    // One host with a real GPT/FAT image + one host whose image is missing.
    // sync_all_hosts_cache must report succeeded=1, failed=[missing], not the
    // old Ok(())-always behaviour (issue 002).
    let dir = tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let good_disk = dir.path().join("good.img");
    build_dual_kernel_gpt_image(&good_disk);

    let good = HostConfig {
        mac: "00:11:22:33:44:77".to_string(),
        name: "good-host".to_string(),
        image_path: good_disk,
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };
    let missing = HostConfig {
        mac: "00:11:22:33:44:88".to_string(),
        name: "missing-host".to_string(),
        image_path: dir.path().join("does_not_exist.iso"),
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };
    let config = Config {
        server: server_config(&cache_dir),
        hosts: vec![good, missing],
    };

    let summary = bootycall_extractor::sync_all_hosts_cache(&config).unwrap();
    assert_eq!(summary.succeeded, 1, "the good host should extract");
    assert_eq!(summary.failed.len(), 1, "the missing host should fail");
    assert_eq!(summary.failed[0].0, "missing-host");
    assert_eq!(summary.total(), 2);
    assert!(summary.partial_failure());
    assert!(!summary.all_failed());
}

#[test]
fn test_sync_all_hosts_cache_all_failed() {
    // Every host's image is missing -> all_failed() is true (distinct from a
    // healthy sync), which main surfaces prominently (issue 020).
    let dir = tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    let host = HostConfig {
        mac: "00:11:22:33:44:99".to_string(),
        name: "missing-only".to_string(),
        image_path: dir.path().join("nope.iso"),
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };
    let config = Config {
        server: server_config(&cache_dir),
        hosts: vec![host],
    };
    let summary = bootycall_extractor::sync_all_hosts_cache(&config).unwrap();
    assert_eq!(summary.succeeded, 0);
    assert!(summary.all_failed());
    assert!(!summary.partial_failure());
}

#[test]
fn test_sync_host_cache_size_ceiling() {
    // The synthetic image's kernel is 14 bytes; a 5-byte ceiling must reject
    // it with ArtifactTooLarge and leave no partial cache behind (issue 006).
    let dir = tempdir().unwrap();
    let disk_path = dir.path().join("big.img");
    let cache_dir = dir.path().join("cache");
    build_dual_kernel_gpt_image(&disk_path);

    let host = HostConfig {
        mac: "00:11:22:33:44:aa".to_string(),
        name: "oversized".to_string(),
        image_path: disk_path,
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };

    let err = bootycall_extractor::sync_host_cache(&host, &cache_dir, Some(5)).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("max_artifact_bytes"),
        "expected an artifact-too-large error, got: {msg}"
    );

    // No partial cache artifacts must be left behind.
    let host_cache_dir = cache_dir.join(&host.mac);
    assert!(
        !host_cache_dir.join("kernel").exists(),
        "partial kernel left behind"
    );
    assert!(
        !host_cache_dir.join("initrd").exists(),
        "partial initrd left behind"
    );
    assert!(
        !host_cache_dir.join("metadata.json").exists(),
        "metadata left behind"
    );
    assert!(
        !host_cache_dir.join("kernel.tmp").exists(),
        "staging kernel.tmp left behind"
    );
    assert!(
        !host_cache_dir.join("initrd.tmp").exists(),
        "staging initrd.tmp left behind"
    );

    // A generous ceiling extracts the same host fine.
    bootycall_extractor::sync_host_cache(&host, &cache_dir, Some(1024)).unwrap();
    assert!(host_cache_dir.join("kernel").exists());
}

#[test]
fn test_corrupt_images_error_not_panic() {
    // The extractor parses semi-trusted image bytes; hostile/degenerate input
    // must return Err, never panic (a panic fails this test). Issue 039.
    let dir = tempdir().unwrap();
    let out_k = dir.path().join("k");
    let out_i = dir.path().join("i");

    let assert_both_err = |img: &std::path::Path, label: &str| {
        let iso = bootycall_extractor::iso::extract_from_iso(img, None, None, &out_k, &out_i, None);
        assert!(iso.is_err(), "{label}: ISO extraction should Err");
        let disk =
            bootycall_extractor::disk::extract_from_disk(img, None, None, &out_k, &out_i, None);
        assert!(disk.is_err(), "{label}: disk extraction should Err");
    };

    // (a) A buffer of pseudo-random bytes (deterministic, no rand dep).
    let rnd = dir.path().join("random.img");
    let data: Vec<u8> = (0..65536u32)
        .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
        .collect();
    File::create(&rnd).unwrap().write_all(&data).unwrap();
    assert_both_err(&rnd, "random");

    // (b) A zero-length file.
    let empty = dir.path().join("empty.img");
    File::create(&empty).unwrap();
    assert_both_err(&empty, "empty");

    // (c) A valid-looking-but-empty (zero-filled) 1 MiB image.
    let zeros = dir.path().join("zeros.img");
    File::create(&zeros).unwrap().set_len(1024 * 1024).unwrap();
    assert_both_err(&zeros, "zeros");

    // (d) A truncated GPT: protective MBR present but no GPT table behind it.
    let truncated_img = dir.path().join("truncated_img.img");
    File::create(&truncated_img)
        .unwrap()
        .set_len(1024 * 1024)
        .unwrap();
    {
        let mut f = File::options()
            .read(true)
            .write(true)
            .open(&truncated_img)
            .unwrap();
        let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
            std::convert::TryFrom::try_from((1024 * 1024 / 512) - 1).unwrap(),
        );
        mbr.overwrite_lba0(&mut f).unwrap();
    }
    assert_both_err(&truncated_img, "truncated-gpt");
}

#[test]
fn test_partition_slice_seek_guards_overflow() {
    use std::io::{Cursor, Seek, SeekFrom};
    // BUG-12: `SeekFrom::Start(u64::MAX)` used to cast straight to i64,
    // wrap silently, and land somewhere valid. It should return an error.
    let mut slice =
        bootycall_extractor::disk::PartitionSlice::new(Cursor::new(vec![0u8; 1024]), 0, 1024);
    let err = slice.seek(SeekFrom::Start(u64::MAX)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    // SeekFrom::Current with a positive offset that overflows the sum
    // must return an error rather than wrapping around silently.
    slice.seek(SeekFrom::Start(1)).unwrap();
    let err = slice.seek(SeekFrom::Current(i64::MAX)).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

    // Normal seeks still work.
    assert_eq!(slice.seek(SeekFrom::Start(512)).unwrap(), 512);
    assert_eq!(slice.seek(SeekFrom::End(-16)).unwrap(), 1008);
}

/// Build a GPT/FAT image with `depth` nested directories, placing a kernel
/// (`vmlinuz`) and initrd (`initrd.img`) in the deepest directory.
fn build_deeply_nested_gpt_image(disk_path: &std::path::Path, depth: usize) {
    let disk_size = 8 * 1024 * 1024;
    {
        let f = File::create(disk_path).unwrap();
        f.set_len(disk_size as u64).unwrap();
    }
    {
        let mut f = File::options()
            .read(true)
            .write(true)
            .open(disk_path)
            .unwrap();
        let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
            std::convert::TryFrom::try_from((disk_size / 512) - 1).unwrap(),
        );
        mbr.overwrite_lba0(&mut f).unwrap();
    }
    let (start_byte, len_byte) = {
        let f = File::options()
            .read(true)
            .write(true)
            .open(disk_path)
            .unwrap();
        let mut gdisk = gpt::GptConfig::default()
            .writable(true)
            .logical_block_size(gpt::disk::LogicalBlockSize::Lb512)
            .create_from_device(Box::new(f), None)
            .unwrap();
        gdisk
            .update_partitions(std::collections::BTreeMap::new())
            .unwrap();
        gdisk
            .add_partition(
                "boot",
                6 * 1024 * 1024,
                gpt::partition_types::BASIC,
                0,
                None,
            )
            .unwrap();
        let p = gdisk.partitions().get(&1).unwrap();
        let s = p.bytes_start(gpt::disk::LogicalBlockSize::Lb512).unwrap();
        let l = p.bytes_len(gpt::disk::LogicalBlockSize::Lb512).unwrap();
        gdisk.write().unwrap();
        (s, l)
    };
    let part_file = File::options()
        .read(true)
        .write(true)
        .open(disk_path)
        .unwrap();
    let mut slice = bootycall_extractor::disk::PartitionSlice::new(part_file, start_byte, len_byte);
    fatfs::format_volume(&mut slice, fatfs::FormatVolumeOptions::new()).unwrap();
    let fs = fatfs::FileSystem::new(slice, fatfs::FsOptions::new()).unwrap();
    let root_dir = fs.root_dir();
    let mut path = String::new();
    for i in 0..depth {
        if i > 0 {
            path.push('/');
        }
        path.push_str(&format!("d{i}"));
        root_dir.create_dir(&path).unwrap();
    }
    root_dir
        .create_file(&format!("{path}/vmlinuz"))
        .unwrap()
        .write_all(b"deep_kernel")
        .unwrap();
    root_dir
        .create_file(&format!("{path}/initrd.img"))
        .unwrap()
        .write_all(b"deep_initrd")
        .unwrap();
}

#[test]
fn test_fat_recursion_depth_cap() {
    // A kernel buried past MAX_DIR_DEPTH (64) must NOT be found: the FAT walker
    // bails at the cap (Ok(None) internally) rather than recursing / stack-
    // overflowing. Observable outcome: KernelNotFound, no panic. Issue 040.
    let dir = tempdir().unwrap();
    let cache_dir = dir.path().join("cache");

    let deep = dir.path().join("deep.img");
    build_deeply_nested_gpt_image(&deep, 70);
    let deep_host = HostConfig {
        mac: "00:11:22:33:44:cc".to_string(),
        name: "deep".to_string(),
        image_path: deep,
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };
    let err = bootycall_extractor::sync_host_cache(&deep_host, &cache_dir, None).unwrap_err();
    assert!(
        matches!(
            err,
            bootycall_extractor::error::ExtractorError::KernelNotFound
        ),
        "deep walk should bail at the cap -> KernelNotFound, got {err:?}"
    );

    // Control: the same structure at a shallow depth (10) IS found, proving it
    // is the depth cap — not a builder bug — that blocks the deep case.
    let shallow = dir.path().join("shallow.img");
    build_deeply_nested_gpt_image(&shallow, 10);
    let shallow_host = HostConfig {
        mac: "00:11:22:33:44:dd".to_string(),
        name: "shallow".to_string(),
        image_path: shallow,
        bootloader: None,
        kernel_path: None,
        initrd_path: None,
        cmdline: None,
    };
    bootycall_extractor::sync_host_cache(&shallow_host, &cache_dir, None).unwrap();
    assert_eq!(
        fs::read_to_string(cache_dir.join(&shallow_host.mac).join("kernel")).unwrap(),
        "deep_kernel"
    );
}

// ---- Minimal ISO9660 writer (issue 038) --------------------------------
//
// Builds a tiny single-directory ISO9660 image that the `iso9660` reader crate
// accepts, without any external ISO-authoring tool (none is available in the
// dev shell). Only the fields the reader actually parses are populated: a
// primary volume descriptor at sector 16, a terminator at sector 17, one root
// directory extent at sector 18, and one data sector per file. Integers use the
// both-endian encoding (little then big) the format requires.

const ISO_SECTOR: usize = 2048;

fn iso_both32(buf: &mut [u8], off: usize, val: u32) {
    buf[off..off + 4].copy_from_slice(&val.to_le_bytes());
    buf[off + 4..off + 8].copy_from_slice(&val.to_be_bytes());
}

fn iso_both16(buf: &mut [u8], off: usize, val: u16) {
    buf[off..off + 2].copy_from_slice(&val.to_le_bytes());
    buf[off + 2..off + 4].copy_from_slice(&val.to_be_bytes());
}

/// Write a directory record at `buf[off..]`; returns the record length
/// (padded to an even number of bytes as the format requires).
fn iso_dir_record(
    buf: &mut [u8],
    off: usize,
    id: &[u8],
    extent_lba: u32,
    extent_len: u32,
    is_dir: bool,
) -> usize {
    let mut len = 33 + id.len();
    if !len.is_multiple_of(2) {
        len += 1;
    }
    buf[off] = len as u8; // directory record length
    buf[off + 1] = 0; // extended attribute record length
    iso_both32(buf, off + 2, extent_lba);
    iso_both32(buf, off + 10, extent_len);
    // bytes off+18..off+25: 7-byte recording date/time — zeros are accepted.
    buf[off + 25] = if is_dir { 0x02 } else { 0x00 }; // file flags (bit1 = dir)
    buf[off + 26] = 0; // file unit size
    buf[off + 27] = 0; // interleave gap size
    iso_both16(buf, off + 28, 1); // volume sequence number
    buf[off + 32] = id.len() as u8; // identifier length
    buf[off + 33..off + 33 + id.len()].copy_from_slice(id);
    len
}

/// Build a minimal ISO9660 image at `path` whose root directory contains the
/// given `(name, data)` files. Each file's data occupies one sector, so each
/// datum must be <= 2048 bytes (true for these tests).
fn build_minimal_iso(path: &std::path::Path, files: &[(&str, &[u8])]) {
    let root_lba = 18u32;
    let first_file_lba = 19u32;
    let total_sectors = first_file_lba as usize + files.len();
    let mut img = vec![0u8; total_sectors * ISO_SECTOR];

    // ---- Primary volume descriptor at sector 16 ----
    let pvd = 16 * ISO_SECTOR;
    img[pvd] = 1; // type: primary
    img[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
    img[pvd + 6] = 1; // version
    for b in &mut img[pvd + 8..pvd + 72] {
        *b = b' '; // system + volume identifiers
    }
    iso_both32(&mut img, pvd + 80, total_sectors as u32); // volume space size
    iso_both16(&mut img, pvd + 120, 1); // volume set size
    iso_both16(&mut img, pvd + 124, 1); // volume sequence number
    iso_both16(&mut img, pvd + 128, ISO_SECTOR as u16); // logical block size (must be 2048)
    // Path-table fields left zero — the reader traverses via the root record.
    iso_dir_record(
        &mut img,
        pvd + 156,
        &[0u8],
        root_lba,
        ISO_SECTOR as u32,
        true,
    );
    for b in &mut img[pvd + 190..pvd + 190 + 623] {
        *b = b' '; // volume-set/publisher/... string fields
    }
    // Four 17-byte ASCII date/time fields must be numeric (the reader parses
    // them with str::parse); all-zero digits parse fine.
    let dates_off = pvd + 190 + 623;
    for k in 0..4 {
        let o = dates_off + k * 17;
        img[o..o + 16].copy_from_slice(b"0000000000000000");
        img[o + 16] = 0;
    }
    img[dates_off + 68] = 1; // file structure version

    // ---- Volume descriptor set terminator at sector 17 ----
    let term = 17 * ISO_SECTOR;
    img[term] = 255;
    img[term + 1..term + 6].copy_from_slice(b"CD001");
    img[term + 6] = 1;

    // ---- Root directory extent at sector 18 ----
    let mut off = root_lba as usize * ISO_SECTOR;
    off += iso_dir_record(&mut img, off, &[0u8], root_lba, ISO_SECTOR as u32, true); // "."
    off += iso_dir_record(&mut img, off, &[1u8], root_lba, ISO_SECTOR as u32, true); // ".."
    for (i, (name, data)) in files.iter().enumerate() {
        let lba = first_file_lba + i as u32;
        off += iso_dir_record(
            &mut img,
            off,
            name.as_bytes(),
            lba,
            data.len() as u32,
            false,
        );
        let d = lba as usize * ISO_SECTOR;
        img[d..d + data.len()].copy_from_slice(data);
    }
    // The next byte after the last record is already zero (buffer is zeroed),
    // which signals end-of-directory to the reader.

    std::fs::write(path, &img).unwrap();
}

#[test]
fn test_iso_auto_detect() {
    let dir = tempdir().unwrap();
    let iso = dir.path().join("auto.iso");
    build_minimal_iso(
        &iso,
        &[
            ("vmlinuz", b"iso_kernel_bytes"),
            ("initrd", b"iso_initrd_bytes"),
        ],
    );
    let out_k = dir.path().join("k");
    let out_i = dir.path().join("i");
    bootycall_extractor::iso::extract_from_iso(&iso, None, None, &out_k, &out_i, None).unwrap();
    assert_eq!(fs::read(&out_k).unwrap(), b"iso_kernel_bytes");
    assert_eq!(fs::read(&out_i).unwrap(), b"iso_initrd_bytes");
}

#[test]
fn test_iso_explicit_overrides() {
    let dir = tempdir().unwrap();
    let iso = dir.path().join("override.iso");
    // Names deliberately do NOT match the auto-detect heuristics, so success
    // proves the explicit override paths were used.
    build_minimal_iso(
        &iso,
        &[("alpha", b"override_kernel"), ("beta", b"override_initrd")],
    );
    let out_k = dir.path().join("k");
    let out_i = dir.path().join("i");
    // Auto-detect must fail on these names.
    assert!(
        bootycall_extractor::iso::extract_from_iso(&iso, None, None, &out_k, &out_i, None).is_err()
    );
    // Explicit overrides resolve them.
    bootycall_extractor::iso::extract_from_iso(
        &iso,
        Some("alpha"),
        Some("beta"),
        &out_k,
        &out_i,
        None,
    )
    .unwrap();
    assert_eq!(fs::read(&out_k).unwrap(), b"override_kernel");
    assert_eq!(fs::read(&out_i).unwrap(), b"override_initrd");
}

#[test]
fn test_iso_missing_kernel_and_initrd() {
    let dir = tempdir().unwrap();
    let out_k = dir.path().join("k");
    let out_i = dir.path().join("i");

    // Only an initrd -> KernelNotFound.
    let no_kernel = dir.path().join("no_kernel.iso");
    build_minimal_iso(&no_kernel, &[("initrd", b"only_initrd")]);
    assert!(matches!(
        bootycall_extractor::iso::extract_from_iso(&no_kernel, None, None, &out_k, &out_i, None),
        Err(bootycall_extractor::error::ExtractorError::KernelNotFound)
    ));

    // Only a kernel -> InitrdNotFound.
    let no_initrd = dir.path().join("no_initrd.iso");
    build_minimal_iso(&no_initrd, &[("vmlinuz", b"only_kernel")]);
    assert!(matches!(
        bootycall_extractor::iso::extract_from_iso(&no_initrd, None, None, &out_k, &out_i, None),
        Err(bootycall_extractor::error::ExtractorError::InitrdNotFound)
    ));
}
