use bootycall_core::config::HostConfig;
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
    bootycall_extractor::sync_host_cache(&host, &cache_dir).unwrap();

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
    bootycall_extractor::sync_host_cache(&host, &cache_dir).unwrap();

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

    let result = bootycall_extractor::sync_host_cache(&host, &cache_dir);
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
    bootycall_extractor::sync_host_cache(&host, &cache_dir).unwrap();

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

    bootycall_extractor::sync_host_cache(&host_with_override, &cache_dir).unwrap();

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
    bootycall_extractor::sync_host_cache(&host_with_override, &cache_dir).unwrap();
    let after = fs::metadata(&cached_kernel).unwrap().modified().unwrap();
    assert_eq!(before, after, "second override sync must not re-extract");
}
