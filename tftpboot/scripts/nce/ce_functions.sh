#!/bin/sh

##################################################
# Name: ce_functions.sh
# Description: A collection of custom functions for hacking on NCE.
##################################################

logger() {
	LOGGER_LEVEL="$1"
	LOGGER_MESSAGE="$2"
	LOGGER_TIME=$(date +"%Y-%m-%d %H:%M:%S")
	LOGGER_FILE=/tmp/nce_hacks.log

	echo "${LOGGER_TIME} ${LOGGER_LEVEL} ${LOGGER_MESSAGE}" >>"${LOGGER_FILE}" 2>&1
	echo "${LOGGER_TIME} ${LOGGER_LEVEL} ${LOGGER_MESSAGE}"
	return 0

}
logger "Loading Nutanix Community Edition shell functions..."

extract_boot_param() {
	PARAM_NAME="$1"
	VALUE=$(grep -oE "$PARAM_NAME=\S*" /proc/cmdline | head -n 1 | sed "s/^$PARAM_NAME=//")
	VALUE_TRIMMED=$(echo "$VALUE" | sed "s/^'//" | sed "s/'$//")
	echo "$VALUE_TRIMMED"
}

load_loop_module() {
	if grep -q loop /proc/modules; then
		logger INFO "Loop module already loaded."
		return 0
	fi
	insmod "/lib/modules/$(uname -r)/kernel/drivers/block/loop.ko" 2>>/tmp/nce_hacks.log || modprobe loop 2>>/tmp/nce_hacks.log || {
		logger WARN "insmod/modprobe loop.ko failed—no losetup."
		return 1
	}
	insmod "/lib/modules/$(uname -r)/kernel/fs/squashfs/squashfs.ko" 2>>/tmp/nce_hacks.log || modprobe squashfs 2>>/tmp/nce_hacks.log || {
		logger WARN "squashfs module load failed."
		return 1
	}
	logger INFO "Loaded loop/squashfs modules."
	return 0
}

download_file_into_ce() {
	SOURCE="$1"
	DEST="$2"
	FILENAME=$(basename "$SOURCE")

	TRY=1
	TOTAL_TRIES=10
	TIME_BETWEEN=30
	TIMEOUT=900

	logger INFO "Downloading ${SOURCE} to ${DEST}..."

	if ! command -v wget >/dev/null 2>&1; then
		logger ERROR "wget is not installed!"
		return 1
	fi

	while [ "$TRY" -le "$TOTAL_TRIES" ]; do
		logger INFO "Downloading ${FILENAME} attempt $TRY of $TOTAL_TRIES..."
		if wget --continue --quiet --tries=1 --timeout="$TIMEOUT" --output-document=- "$SOURCE" >"$DEST"; then
			if [ -s "$DEST" ]; then
				logger INFO "Successfully downloaded ${SOURCE} to ${DEST}"
				return 0
			else
				logger WARN "Downloaded file is empty, will retry download."
				rm -f "$DEST"
			fi
		fi
		if [ "$TRY" -lt "$TOTAL_TRIES" ]; then
			logger WARN "Download failed, retrying in ${TIME_BETWEEN} seconds..."
			sleep "$TIME_BETWEEN"
		fi
		TRY=$((TRY + 1))
	done

	logger ERROR "Failed to download ${SOURCE} after ${TOTAL_TRIES} attempts"
	rm -f "$DEST"
	return 1
}

download_squashfs_into_ce() {
	SQUASHFS_MD5=UPDATE_ME_PLZ

	if [ -z "${LIVEFS_URL}" ]; then
		logger ERROR "The variable LIVEFS_URL is not set!"
		return 1
	fi

	if [ -z "${IMG_FILE}" ]; then
		logger ERROR "The variable IMG_FILE is not set!"
		return 1
	fi

	logger INFO "Downloading squashfs.img from LIVEFS_URL: ${LIVEFS_URL}"

	if ! download_file_into_ce "${LIVEFS_URL}" "${IMG_FILE}"; then
		logger ERROR "Failed to download squashfs.img"
		rm -f "${IMG_FILE}"
		return 1
	fi

	if ! echo "${SQUASHFS_MD5}  ${IMG_FILE}" | md5sum -c >/dev/null 2>&1; then
		logger ERROR "Squashfs MD5 mismatch—corrupt download!"
		rm -f "${IMG_FILE}"
		return 1
	fi
	logger INFO "Squashfs MD5 verified OK."

	cp "${IMG_FILE}" /root/squashfs.img
	logger INFO "Staged squashfs to /root/squashfs.img."
	return 0
}

download_images_into_ce() {
	PHOENIX_BASE=$(extract_boot_param "PHOENIX_BASE")

	if [ -z "${PHOENIX_BASE}" ]; then
		logger ERROR "The variable PHOENIX_BASE is not set"
		return 1
	fi

	logger INFO "Downloading images from PHOENIX_BASE: ${PHOENIX_BASE}"

	# Hypervisor Image
	HYP_ISO_SRC="${PHOENIX_BASE}/images/hypervisor/kvm/AHV-DVD-x86_64-el8.nutanix.20230302.101026.iso"
	HYP_ISO_DEST="/mnt/iso/images/hypervisor/kvm/AHV-DVD-x86_64-el8.nutanix.20230302.101026.iso"

	# AOS Image (part 0)
	AOS_P00_SRC="${PHOENIX_BASE}/images/svm/nutanix_installer_package.tar.p00"
	AOS_P00_DEST="/mnt/iso/images/svm/nutanix_installer_package.tar.p00"

	# AOS Image (part 1)
	AOS_P01_SRC="${PHOENIX_BASE}/images/svm/nutanix_installer_package.tar.p01"
	AOS_P01_DEST="/mnt/iso/images/svm/nutanix_installer_package.tar.p01"

	# Make ISO script
	MAKE_ISO_SRC="${PHOENIX_BASE}/make_iso.sh"
	MAKE_ISO_DEST="/mnt/iso/make_iso.sh"

	# Ensure directories exist
	mkdir -p /mnt/iso/images/svm /mnt/iso/images/hypervisor/kvm

	if [ -f "${HYP_ISO_DEST}" ]; then
		download_file_into_ce "$HYP_ISO_SRC" "$HYP_ISO_DEST" || {
			logger ERROR "Failed to download Hypervisor ISO!"
			return 1
		}
	else
		logger INFO "Hypervisor ISO image already exists at ${HYP_ISO_DEST}"
	fi

	if [ -f "${AOS_P00_DEST}" ]; then
		download_file_into_ce "$AOS_P00_SRC" "$AOS_P00_DEST" || {
			logger ERROR "Failed to download AOS P00 package!"
			return 1
		}
	else
		logger INFO "AOS P00 package already exists at ${AOS_P00_DEST}"
	fi

	if [ -f "${AOS_P01_DEST}" ]; then
		download_file_into_ce "$AOS_P01_SRC" "$AOS_P01_DEST" || {
			logger ERROR "Failed to download AOS P01 package!"
			return 1
		}
	else
		logger INFO "AOS P01 package already exists at ${AOS_P01_DEST}"
	fi

	if [ -f "${MAKE_ISO_DEST}" ]; then
		download_file_into_ce "$MAKE_ISO_SRC" "$MAKE_ISO_DEST" || {
			logger ERROR "Failed to download make_iso.sh script!"
			return 1
		}
	else
		logger INFO "make_iso.sh script already exists at ${MAKE_ISO_DEST}"
	fi

	return 0
}

mount_iso_for_ce() {
	logger INFO "Trying to mount Phoenix ISO..."

	if mountpoint -q /mnt/iso; then
		logger INFO "/mnt/iso already mounted—skipping."
		return 0
	fi
	ISO_DEV=""

	# Attempt 1: If the system has blkid, use it.
	if [ -z "$ISO_DEV" ]; then
		logger INFO "Attempt 1: Looking for ISO via blkid..."
		if command -v blkid >/dev/null 2>&1; then
			ISO_DEV=$(blkid -L "PHOENIX" 2>/dev/null | head -1)
		fi
	fi

	# Attempt 2: If blkid failed, try probing /proc/partitions for sr/loop devices.
	if [ -z "$ISO_DEV" ]; then
		logger INFO "Attempt 2: Probing /proc/partitions for sr/loop devices..."
		ISO_DEV=$(grep -E 'sr|loop' /proc/partitions | tail -1 | awk '{print "/dev/" $4}')
		if [ -n "$ISO_DEV" ] && mount -t iso9660 "$ISO_DEV" /mnt/iso -o ro >/dev/null 2>&1 && [ -f /mnt/iso/make_iso.sh ]; then
			umount /mnt/iso >/dev/null 2>&1
			logger INFO "ISO probed on $ISO_DEV via /proc."
		else
			ISO_DEV=""
		fi
	fi

	# Attempt 3: If probing failed, try probing /dev/sr* and /dev/loop* devices.
	if [ -z "$ISO_DEV" ]; then
		logger INFO "Attempt 3: Probing /dev/sr* and /dev/loop* devices..."
		for dev in /dev/sr* /dev/loop*; do
			[ -b "$dev" ] || continue
			if mount -t iso9660 -o ro "$dev" /mnt/iso >/dev/null 2>&1; then
				logger INFO "ISO mount successful, testing for make_iso.sh"
				if [ -f /mnt/iso/make_iso.sh ]; then
					logger INFO "make_iso.sh found, ISO is OK!"
					ISO_DEV="$dev"
					logger INFO "ISO test-mounted on $ISO_DEV."
				else
					logger WARN "make_iso.sh not found, incorrect ISO mounted!"
				fi
				umount /mnt/iso >/dev/null 2>&1
			else
				logger WARN "ISO mount failed on $dev"
			fi
		done
	fi

	# If we have an ISO device, mount it and copy the files.
	if [ -n "$ISO_DEV" ]; then
		logger INFO "Mounting ISO on $ISO_DEV."
		if mount -t iso9660 -o ro "$ISO_DEV" /mnt/iso; then
			if [ -f /mnt/iso/make_iso.sh ]; then
				logger INFO "Full Phoenix ISO mounted at /mnt/iso via sanboot/probe."
				mkdir -p /overlay/mnt/iso /overlay/root/images
				cp -r /mnt/iso/* /overlay/mnt/iso/ 2>/dev/null || true
				cp /mnt/iso/squashfs.img /overlay/root/ 2>/dev/null || true
				cp -r /mnt/iso/images/* /overlay/root/images/ 2>/dev/null || true
				return 0
			fi
			umount /mnt/iso 2>/dev/null || true
		else
			logger ERROR "Mount failed on $ISO_DEV."
		fi
	fi

	logger WARN "All attempts to mount the Phoenix ISO have failed!"
	return 1

}

download_and_mount_iso_for_ce() {
	logger INFO "Downloading full Phoenix ISO to ramdisk..."

	PHOENIX_BASE=$(extract_boot_param "PHOENIX_BASE")
	PHOENIX_ISO_REMOTE=$(extract_boot_param "PHOENIX_ISO")
	PHOENIX_ISO_LOCAL="/tmp/phoenix.iso"

	load_loop_module || {
		logger ERROR "Failed to load loop kernel module, aborting ISO download."
		return 1
	}

	if command -v losetup >/dev/null 2>&1; then
		logger ERROR "Losetup is not installed, aborting ISO download."
		return 1
	fi

	if ! download_file_into_ce "$PHOENIX_ISO_REMOTE" "$PHOENIX_ISO_LOCAL"; then
		logger ERROR "Failed to download the Phoenix ISO!"
		return 1
	fi

	LOOP_DEV=$(losetup -f -s --show "$PHOENIX_ISO_LOCAL" 2>/dev/null)

	if [ -z "$LOOP_DEV" ]; then
		logger ERROR "Losetup failed—check loop kernel module?"
		rm -f "$PHOENIX_ISO_LOCAL"
		return 1
	fi

	if ! mount -t iso9660 -o loop,ro "$LOOP_DEV" /mnt/iso; then
		logger ERROR "ISO mount failed on $LOOP_DEV."
		losetup -d "$LOOP_DEV" 2>/dev/null
		rm -f "$PHOENIX_ISO_LOCAL"
		return 1
	fi

	logger INFO "The full Phoenix ISO has been loop-mounted at /mnt/iso."

	logger INFO "Staging files for overlay..."
	mkdir -p /overlay/mnt/iso /overlay/root/images
	cp -r /mnt/iso/* /overlay/mnt/iso/ 2>/dev/null || true
	cp /mnt/iso/squashfs.img /overlay/root/ 2>/dev/null || true
	cp -r /mnt/iso/images/* /overlay/root/images/ 2>/dev/null || true
	umount /mnt/iso 2>/dev/null
	losetup -d "$LOOP_DEV" 2>/dev/null
	rm -f "$PHOENIX_ISO_LOCAL"

	return 0
}
