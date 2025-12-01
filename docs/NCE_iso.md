# Nutanix Community Edition custom ISO

## Overview

Making the NCE ISO iPXE friendly.

## Steps

### Create a temporary directory for use as a workspace.

```bash
# Make sure its exported so its available int the nix-shell
export NCE_TEMP=$(mktemp -d)

cd ${NCE_TEMP}
```

### Create a custom nix-shell configuration file

```bash
cat > shell-amd64.nix <<'EOF'
let

  pkgs = import <nixpkgs> { };

in pkgs.mkShell {
  buildInputs = with pkgs; [
    bc
    binutils
    bison
    cdrkit
    cpio
    elfutils
    file
    flex
    gcc
    gnumake
    kmod
    libelf
    openssl
    p7zip
    perl
    pigz
    rpm
    rsync
    squashfsTools
  ];

  shellHook = ''
    echo "🛠️  Nutanix ISO Hacking Shell Loaded"
    echo ""
    echo "Ready to extract, inject, and repack."
  '';
}
EOF
```

- Launch the nix-shell

```bash
nix-shell shell-amd64.nix
```

### Copy the original ISO to the iPXE server

```bash
NCE_ISO=${HOME}/Downloads/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga.iso

rsync \
	-avz \
	--progress \
	--partial \
	--inplace \
	${NCE_ISO} \
	bootycall:/mnt/hdd/tftpboot/iso/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga.iso
```

## Extract the ISO

```bash
cd ${NCE_TEMP}

NCE_ISO=${HOME}/Downloads/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga.iso

mkdir -p ${NCE_TEMP}/iso-mounted
mkdir -p ${NCE_TEMP}/iso-extracted

sudo mount -o loop ${NCE_ISO} ${NCE_TEMP}/iso-mounted

sudo rsync -av --delete ${NCE_TEMP}/iso-mounted/* ${NCE_TEMP}/iso-extracted/

sudo umount ${NCE_TEMP}/iso-mounted
```

## Modify the ISO boot configuration

```bash
cd ${NCE_TEMP}

# File locations
CFG_GRUB=iso-extracted/EFI/BOOT/grub.cfg
CFG_ISOLINUX=iso-extracted/boot/isolinux/isolinux.cfg

# Update the grub.cfg for EFI boot.
sudo tee ${CFG_GRUB} > /dev/null << 'EOF'
set default="CEInstaller iPXE"

insmod part_gpt
insmod part_msdos

set timeout=30

search --no-floppy --set=root -l 'PHOENIX'

menuentry 'CEInstaller' {
	linuxefi /boot/kernel init=/ce_installer intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 IMG=squashfs
	initrdefi /boot/initrd
}

menuentry 'CEInstaller iPXE' {
	linuxefi /boot/kernel init=/ce_installer intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 IMG=squashfs mpt3sas.prot_mask=1 LIVEFS_URL=http://bootycall.saltlabs.cloud/iso-extracted/phoenix/squashfs.img PHOENIX_BASE=http://bootycall.saltlabs.cloud/iso-extracted/phoenix PHOENIX_ISO=http://bootycall.saltlabs.cloud/iso/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga_ipxe.iso UPDATES_CONFIG_URL=http://bootycall.saltlabs.cloud/iso-extracted/phoenix/updates_config.json rd.live.squashimg=/root/squashfs.img ip=dhcp rd.neednet=1 rd.debug CE_IPXE=1
	initrdefi /boot/initrd
}

menuentry 'Debug Shell for Stage 1 (initramfs)' {
	linuxefi /boot/kernel intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 mpt3sas.prot_mask=1 LIVEFS_URL=http://bootycall.saltlabs.cloud/iso-extracted/phoenix/squashfs.img PHOENIX_BASE=http://bootycall.saltlabs.cloud/iso-extracted/phoenix PHOENIX_ISO=http://bootycall.saltlabs.cloud/iso/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga_ipxe.iso rd.live.squashimg=/root/squashfs.img ip=dhcp rd.neednet=1 rd.shell=1 rd.break=pre-mount rd.debug
	initrdefi /boot/initrd
}

menuentry 'Debug Shell for Stage 2 (squashfs)' {
	linuxefi /boot/kernel init=/usr/bin/bash intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 IMG=squashfs mpt3sas.prot_mask=1 LIVEFS_URL=http://bootycall.saltlabs.cloud/iso-extracted/phoenix/squashfs.img PHOENIX_BASE=http://bootycall.saltlabs.cloud/iso-extracted/phoenix PHOENIX_ISO=http://bootycall.saltlabs.cloud/iso/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga_ipxe.iso rd.live.squashimg=/root/squashfs.img ip=dhcp rd.neednet=1 rd.shell=1 rd.break=mount rd.debug
	initrdefi /boot/initrd
}
EOF
```

## Unpack initrd

```bash
cd ${NCE_TEMP}

mkdir ${NCE_TEMP}/initrd-extracted
cd ${NCE_TEMP}/initrd-extracted

zcat ${NCE_TEMP}/iso-extracted/boot/initrd | cpio -idmv
```

### Update the scripts inside the initrd

```bash
# Define the locations of the modified scripts.
SCRIPTS_SOURCE="/home/mahdtech/Sync/Projects/GitHub/MAHDTech/BootyCall/tftpboot/scripts/nce"
SCRIPTS_DEST="${NCE_TEMP}/initrd-extracted"

echo "Copying modified livecd.sh script..."
cp -f "${SCRIPTS_SOURCE}/livecd.sh" "${SCRIPTS_DEST}/livecd.sh"

echo "Copying modified do_ce_installer.sh script..."
cp -f "${SCRIPTS_SOURCE}/do_ce_installer.sh" "${SCRIPTS_DEST}/do_ce_installer.sh"

echo "Copying new ce_functions.sh script..."
cp -f "${SCRIPTS_SOURCE}/ce_functions.sh" "${SCRIPTS_DEST}/ce_functions.sh"
```

### Repack initrd

```bash
cd ${NCE_TEMP}/initrd-extracted

# REMINDER: If you have modified the squashfs, you need to update the checksum.
#SQUASHFS_MD5=$(md5sum ${NCE_TEMP}/iso-extracted/squashfs.img | awk '{print $1}')
#sed -i "s/^\([[:space:]]*\)SQUASHFS_MD5=.*/\1SQUASHFS_MD5=${SQUASHFS_MD5}/" ${NCE_TEMP}/initrd-extracted/ce_functions.sh
#sed -i "s/^SQUASHFS_DIGEST_x86_64=.*/SQUASHFS_DIGEST_x86_64=${SQUASHFS_MD5}/" ${NCE_TEMP}/initrd-extracted/livecd.sh
#grep "SQUASHFS_MD5=" ${NCE_TEMP}/initrd-extracted/ce_functions.sh
#grep "SQUASHFS_DIGEST_x86_64=" ${NCE_TEMP}/initrd-extracted/livecd.sh

# Repack the initrd
OLD_INITRD=$(sha256sum ${NCE_TEMP}/iso-extracted/boot/initrd | awk '{print $1}')
find . -print0 | cpio --null -o --format=newc | gzip -9 | sudo tee ${NCE_TEMP}/iso-extracted/boot/initrd > /dev/null
NEW_INITRD=$(sha256sum ${NCE_TEMP}/iso-extracted/boot/initrd | awk '{print $1}')

echo "Old initrd hash: ${OLD_INITRD}"
echo "New initrd hash: ${NEW_INITRD}"
```

## Rebuild the ISO

```bash
cd ${NCE_TEMP}/iso-extracted

# Delete any old ISO.
rm -f ../nutanix_ipxe.iso || true

# Run the ISO script to create the new ISO in the current directory
sudo chmod +x make_iso.sh
sudo ./make_iso.sh nutanix_ipxe
```

## Transfer the ISO

```bash
cd ${NCE_TEMP}

file nutanix_ipxe.iso

rsync \
	-avz \
	--progress \
	--partial \
	--inplace \
	nutanix_ipxe.iso \
	bootycall:/mnt/hdd/tftpboot/iso/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga_ipxe.iso
```

## Cleanup

When you are done with the ISO creation, you can clean up the temporary directory.

```bash
cd $HOME

sudo rm -rf ${NCE_TEMP}
```

## Extract the ISO

SSH to the iPXE server and extract the ISO to the correct location.

```bash
# Define variables
ISO_SOURCE="/mnt/hdd/tftpboot/iso/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga_ipxe.iso"
EXTRACT_DEST="/mnt/hdd/tftpboot/iso-extracted/phoenix"
MOUNT_POINT="/mnt/iso"

echo "Ensuring directories exist..."
mkdir -p ${EXTRACT_DEST}
mkdir -p ${MOUNT_POINT}

echo "Mounting ISO..."
sudo mount -o loop ${ISO_SOURCE} ${MOUNT_POINT}

echo "Syncing contents..."
sudo rsync -av --delete ${MOUNT_POINT}/ ${EXTRACT_DEST}/

echo "Unmounting ISO..."
sudo umount ${MOUNT_POINT}

echo "Changing file ownership..."
sudo chown -R tftp:tftp ${ISO_SOURCE}
sudo chown -R tftp:tftp ${EXTRACT_DEST}
```
