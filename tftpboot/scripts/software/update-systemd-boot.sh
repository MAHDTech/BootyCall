#!/usr/bin/env bash

clear

export DIR="tftpboot/tools/systemd-boot"
export IMG_LOCAL="tftpboot/iso/systemd-boot.img"
export IMG_REMOTE="bootycall:/mnt/hdd/tftpboot/iso/systemd-boot.img"
export IMG_SIZE="64"

nix-shell -p dosfstools mtools --pure --run "
set -euo pipefail

rm -f ${IMG_LOCAL} || true

echo 'Creating empty disk image...'
dd if=/dev/zero of=${IMG_LOCAL} bs=1M count=${IMG_SIZE} status=none

echo 'Formatting as FAT32...'
mkfs.vfat -n BOOTYCALL ${IMG_LOCAL} || {
	echo 'Failed to format image'
	exit 1
}

echo 'Copying files into image...'
mcopy -s -i ${IMG_LOCAL} ${DIR}/* ::/ || {
	echo 'Failed to copy files into image'
	exit 1
}

echo 'Image created -> ${IMG_LOCAL}'
"

read -rp "ISO creation has finished, do you want to upload it? " RESPONSE
if [[ $RESPONSE =~ ^[Yy]$ ]]; then
	rsync -av "${IMG_LOCAL}" "${IMG_REMOTE}" || {
		echo "Failed to upload ISO to remote server"
		exit 1
	}
fi

echo "Finished."
