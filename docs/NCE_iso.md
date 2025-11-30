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

sudo rsync -av ${NCE_TEMP}/iso-mounted/* ${NCE_TEMP}/iso-extracted/

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
	linuxefi /boot/kernel init=/ce_installer intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 IMG=squashfs mpt3sas.prot_mask=1 LIVEFS_URL=http://bootycall.saltlabs.cloud/iso-extracted/phoenix/squashfs.img PHOENIX_BASE=http://bootycall.saltlabs.cloud/iso-extracted/phoenix PHOENIX_ISO=http://bootycall.saltlabs.cloud/iso/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga_ipxe.iso rd.live.squashimg=/root/squashfs.img ip=dhcp rd.neednet=1 rd.debug CE_IPXE=1
	initrdefi /boot/initrd
}

menuentry 'Rescue Shell Stage 1 (initramfs)' {
	linuxefi /boot/kernel intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 mpt3sas.prot_mask=1 LIVEFS_URL=http://bootycall.saltlabs.cloud/iso-extracted/phoenix/squashfs.img PHOENIX_BASE=http://bootycall.saltlabs.cloud/iso-extracted/phoenix PHOENIX_ISO=http://bootycall.saltlabs.cloud/iso/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga_ipxe.iso rd.live.squashimg=/root/squashfs.img ip=dhcp rd.neednet=1 rd.shell=1 rd.break=pre-mount rd.debug
	initrdefi /boot/initrd
}

menuentry 'Rescue Shell Stage 2 (squashfs)' {
	linuxefi /boot/kernel init=/ce_installer intel_iommu=on iommu=pt kvm-intel.nested=1 kvm.ignore_msrs=1 kvm-intel.ept=1 vga=791 net.ifnames=0 IMG=squashfs mpt3sas.prot_mask=1 LIVEFS_URL=http://bootycall.saltlabs.cloud/iso-extracted/phoenix/squashfs.img PHOENIX_BASE=http://bootycall.saltlabs.cloud/iso-extracted/phoenix PHOENIX_ISO=http://bootycall.saltlabs.cloud/iso/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga_ipxe.iso rd.live.squashimg=/root/squashfs.img ip=dhcp rd.neednet=1 rd.shell=1 rd.break=mount rd.debug
	initrdefi /boot/initrd
}
EOF
```

## Modify the initrd

```bash
cd ${NCE_TEMP}

mkdir ${NCE_TEMP}/initrd-extracted
cd ${NCE_TEMP}/initrd-extracted

zcat ${NCE_TEMP}/iso-extracted/boot/initrd | cpio -idmv
```

### Add kernel modules into the initrd

If needed, customise the installer kernel...

```bash
cd ${NCE_TEMP}

NCE_PHOENIX_KERNEL="5.10.2"

mkdir ${NCE_TEMP}/kernel-${NCE_PHOENIX_KERNEL}
cd ${NCE_TEMP}/kernel-${NCE_PHOENIX_KERNEL}

# Download the correct version for the Phoenix installer.
wget https://cdn.kernel.org/pub/linux/kernel/v5.x/linux-${NCE_PHOENIX_KERNEL}.tar.xz

tar -xJf linux-${NCE_PHOENIX_KERNEL}.tar.xz

cd linux-${NCE_PHOENIX_KERNEL}

# Extract the kernel specific configuration from the nutanix kernel into a file.
scripts/extract-ikconfig ${NCE_TEMP}/iso-extracted/boot/kernel > .config-phoenix

# N/A the needed settings are already enabled...
grep -E "(CONFIG_BLK_DEV_LOOP|SQUASHFS)" .config-phoenix

# TODO: Any customisations needed? maybe can try a newer kernel?
```

### Update the scripts inside the initrd

```bash
# Define the locations of the modified scripts.
SCRIPTS_SOURCE="/home/mahdtech/Sync/Projects/GitHub/MAHDTech/BootyCall/tftpboot/scripts/nce"
SCRIPTS_DEST="${NCE_TEMP}/initrd-extracted"

echo "Copying modified livecd.sh script..."
cp -f "${SCRIPTS_SOURCE}/livecd.sh" "${SCRIPTS_DEST}/livecd.sh"

#echo "Copying modified do_ce_installer.sh script..."
#cp -f "${SCRIPTS_SOURCE}/do_ce_installer.sh" "${SCRIPTS_DEST}/do_ce_installer.sh"

#echo "Copying new ce_functions.sh script..."
#cp -f "${SCRIPTS_SOURCE}/ce_functions.sh" "${SCRIPTS_DEST}/ce_functions.sh"
```

### Create a dracut loop module for the initrd

```bash
cd ${NCE_TEMP}/initrd-extracted

mkdir -p usr/lib/dracut/modules.d/05loop

cat > usr/lib/dracut/modules.d/05loop/module-setup.sh << 'EOF'
#!/bin/bash

# Minimal Dracut module: 05loop - Basic loop device support for Phoenix initrd
# Provides: loopdev (helper for losetup -f)
# Requires: Built-in kernel loop (CONFIG_BLK_DEV_LOOP=y)

check() {
    # Always include for initramfs with remote/block images
    return 0
}

depends() {
    # Depends on udev for device nodes (already in Phoenix)
    echo "udev"
}

installkernel() {
    # No kernel modules (built-in)
    :
}

install() {
    # Install pre-udev hook to create /dev/loop* nodes
    # shellcheck disable=SC2154
    inst_hook pre-udev 90 "${moddir}/loop.sh"
    # Install helper binary
    # shellcheck disable=SC2154
    inst_simple "${moddir}/loopdev" /bin/loopdev
    # Copy losetup if missing (from host/CVM; ensures availability)
    # shellcheck disable=SC2154
    if [ ! -e "${initdir}/sbin/losetup" ]; then
        dracut_inst_exec /sbin/losetup /sbin/losetup
    fi
    dracut_inst_rules 60-persistent-storage.rules 2>/dev/null || true
}
EOF
chmod +x usr/lib/dracut/modules.d/05loop/module-setup.sh

cat > usr/lib/dracut/modules.d/05loop/loop.sh << 'EOF'
#!/bin/sh
# Pre-udev: Ensure /dev/loop-control and /dev/loop0-7 exist
# (Matches kernel CONFIG_BLK_DEV_LOOP_MIN_COUNT=8)

[ -e /dev/loop-control ] || mknod /dev/loop-control c 10 237

for i in $(seq 0 7); do
    [ -e "/dev/loop${i}" ] || mknod "/dev/loop${i}" b 7 "${i}"  # Quote $i
done
EOF
chmod +x usr/lib/dracut/modules.d/05loop/loop.sh

cat > usr/lib/dracut/modules.d/05loop/loopdev << 'EOF'
#!/bin/sh
# loopdev: Attach image to free loop device and return path
# Usage: LOOPDEV=$(loopdev /path/to/img)

if [ $# -eq 0 ]; then
    losetup -f  # Just find free device
    exit 0
fi

IMG="$1"
losetup -f --show "$IMG"
EOF
chmod +x usr/lib/dracut/modules.d/05loop/loopdev

# Ensure losetup is included inside initrd
cd ${NCE_TEMP}
mkdir ${NCE_TEMP}/temp-losetup && cd $_

# Download RPM (CentOS 8 / Oracle Linux mirror)
UTILS_LINUX_RPM=util-linux-2.32.1-28.el8.x86_64.rpm
wget https://vault.centos.org/8.5.2111/BaseOS/x86_64/os/Packages/${UTILS_LINUX_RPM}

# Extract rhe rpm.
rpm2cpio ${UTILS_LINUX_RPM} | cpio -idmv

sudo cp -f ./usr/sbin/losetup ${NCE_TEMP}/initrd-extracted/sbin/

cd ${NCE_TEMP}/initrd-extracted/
sudo ln -sf sbin/losetup bin/losetup
ls -la bin/losetup

# Remove the temp directory
rm -rf ${NCE_TEMP}/temp-losetup
```

### Rebuild the initrd and override the original

```bash
cd ${NCE_TEMP}/initrd-extracted

# REMINDER: If you have modified the squashfs, you need to update the checksum.
SQUASHFS_MD5=$(md5sum ${NCE_TEMP}/iso-extracted/squashfs.img | awk '{print $1}')
sed -i "s/^\([[:space:]]*\)SQUASHFS_MD5=.*/\1SQUASHFS_MD5=${SQUASHFS_MD5}/" ${NCE_TEMP}/initrd-extracted/ce_functions.sh
sed -i "s/^SQUASHFS_DIGEST_x86_64=.*/SQUASHFS_DIGEST_x86_64=${SQUASHFS_MD5}/" ${NCE_TEMP}/initrd-extracted/livecd.sh

grep "SQUASHFS_MD5=" ${NCE_TEMP}/initrd-extracted/ce_functions.sh
grep "SQUASHFS_DIGEST_x86_64=" ${NCE_TEMP}/initrd-extracted/livecd.sh

# Repack the initrd
find . -print0 | cpio --null -o --format=newc | gzip -9 | sudo tee ${NCE_TEMP}/iso-extracted/boot/initrd > /dev/null
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

echo "Cleaning destination directory..."
rm -rf ${EXTRACT_DEST}/*

echo "Mounting ISO..."
sudo mount -o loop ${ISO_SOURCE} ${MOUNT_POINT}

echo "Copying contents..."
sudo rsync -av ${MOUNT_POINT}/ ${EXTRACT_DEST}/

echo "Unmounting ISO..."
sudo umount ${MOUNT_POINT}

echo "Changing file ownership..."
sudo chown -R tftp:tftp ${ISO_SOURCE}
sudo chown -R tftp:tftp ${EXTRACT_DEST}
```
