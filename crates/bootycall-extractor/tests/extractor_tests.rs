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

fn server_config(cache_dir: &std::path::Path) -> ServerConfig {
    ServerConfig {
        http_bind: "0.0.0.0:8080".to_string(),
        tftp_bind: "0.0.0.0:69".to_string(),
        tftp_root: std::path::PathBuf::from("./tftpboot"),
        proxy_dhcp_bind: "0.0.0.0:4011".to_string(),
        cache_dir: cache_dir.to_path_buf(),
        default_bootloader_amd64: "boot/x64/ipxe.efi".to_string(),
        default_bootloader_arm64: "boot/arm64/ipxe.efi".to_string(),
        oled_enabled: false,
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
