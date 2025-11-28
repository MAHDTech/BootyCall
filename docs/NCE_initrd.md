# Nutanix Community Edition custom initrd

## Overview

In order to boot the NCE installer over iPXE we need to create a custom initrd that includes NFS support and all required network drivers. The default initrd included in the ISO often lacks the kernel modules required to mount the root filesystem via NFS, as it is designed to boot from local media (USB/CD).

There are different attempts to create a custom initrd.

## Common Steps

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
  	cpio
    pigz
    squashfsTools
    file
    kmod
  ];

  shellHook = ''
    echo "🛠️  Nutanix Initrd Hacking Shell Loaded"
    echo "Ready to extract, inject, and repack."
  '';
}
EOF
```

### Launch the nix-shell

```bash
nix-shell shell-amd64.nix
```

## Option 1: Create a custom initrd and embed missing modules

### Setup and unpack the NCE ISO

```bash
# Where is the NCE ISO?
export NCE_ISO_PATH="$HOME/Downloads/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga.iso"

# 1. Create a workspace inside the temp directory
mkdir -p ${NCE_TEMP}/workspace/extracted_initrd
mkdir -p ${NCE_TEMP}/workspace/extracted_squash
mkdir -p ${NCE_TEMP}/workspace/mount_iso

# 2. Mount the NCE ISO
sudo mount -o loop ${NCE_ISO_PATH} ${NCE_TEMP}/workspace/mount_iso

# 3. Extract the Initrd from the ISO
cd ${NCE_TEMP}/workspace/extracted_initrd
zcat ${NCE_TEMP}/workspace/mount_iso/boot/initrd | cpio -idmv
```

### Prepare the drivers and modules

Include any required drivers and NFS kernel modules from the main system image (SquashFS) into the initrd.

```bash
# 1. Unsquash the main OS image
cd ${NCE_TEMP}
unsquashfs -d workspace/extracted_squash workspace/mount_iso/squashfs.img

# 2. Find the Kernel Version
ls workspace/extracted_initrd/lib/modules/

# Set the kernel version from the output.
export KERNEL_VER="5.10.2"

# TODO: Add NFS support for root=/dev/nfs
mkdir -p workspace/extracted_initrd/lib/modules/$KERNEL_VER/kernel/fs/nfs
mkdir -p workspace/extracted_initrd/lib/modules/$KERNEL_VER/kernel/fs/nfs_common
mkdir -p workspace/extracted_initrd/lib/modules/$KERNEL_VER/kernel/fs/lockd
mkdir -p workspace/extracted_initrd/lib/modules/$KERNEL_VER/kernel/net/sunrpc

# TODO: Find where to copy the NFS modules from.
cp -r workspace/extracted_squash/usr/lib/modules/$KERNEL_VER/kernel/fs/nfs/* \
      workspace/extracted_initrd/lib/modules/$KERNEL_VER/kernel/fs/nfs/
cp -r workspace/extracted_squash/usr/lib/modules/$KERNEL_VER/kernel/fs/nfs_common/* \
      workspace/extracted_initrd/lib/modules/$KERNEL_VER/kernel/fs/nfs_common/
cp -r workspace/extracted_squash/usr/lib/modules/$KERNEL_VER/kernel/fs/lockd/* \
      workspace/extracted_initrd/lib/modules/$KERNEL_VER/kernel/fs/lockd/
cp -r workspace/extracted_squash/usr/lib/modules/$KERNEL_VER/kernel/net/sunrpc/* \
      workspace/extracted_initrd/lib/modules/$KERNEL_VER/kernel/net/sunrpc/

# Copy the module dependency map to avoid the lack of 'depmod'
cp workspace/extracted_squash/usr/lib/modules/$KERNEL_VER/modules.dep \
   workspace/extracted_initrd/lib/modules/$KERNEL_VER/
cp workspace/extracted_squash/usr/lib/modules/$KERNEL_VER/modules.alias \
   workspace/extracted_initrd/lib/modules/$KERNEL_VER/
```

### Force loading drivers

```bash
# Create a force-load config to tell Dracut to "Load these drivers before you do ANYTHING else"
mkdir -p workspace/extracted_initrd/usr/lib/dracut/dracut.conf.d/
echo 'drivers+=" nfs sunrpc lockd "' > workspace/extracted_initrd/usr/lib/dracut/dracut.conf.d/01-force-drivers.conf
```

### Repack

```bash
cd workspace/extracted_initrd

# Repack into a new file named 'initrd-custom'
find . -print0 | cpio --null -o --format=newc | gzip -9 > ../../initrd-custom

# Transfer the initrd-custom over to the iPXE server
# Example:
# scp ../../initrd-custom bootycall:/mnt/hdd/tftpboot/iso-extracted/phoenix/boot/initrd-custom
```

### iPXE Configuration Reference

Update the `menu.ipxe` file to use `initrd-custom`.

```ipxe
#########################
# Example iPXE snippet
#########################

<snip>

# Load the Monolithic Nutanix initrd
initrd --name initrd ${phoenix-base}/initrd-monolithic || goto failed

kernel ${phoenix-base}/boot/kernel initrd=initrd init=/ce_installer ramdisk_size=4000000 root=/dev/ram0 intel_iommu=on iommu=pt kvm-intel.nested=1 kvm-intel.ept=1 vga=791 net.ifnames=0 mpt3sas.prot_mask=1 IMG=squashfs || goto failed

</snip>
```

## Option 2: Monolithic Initrd

This approach will bake the squashfs.img into the initrd.

````bash
# OPTION 1: Create monolithic initrd inserting the squashfs.
cat <<- 'EOF' > "${NCE_TEMP}/nce-initrd-monolithic.sh"
#!/usr/bin/env bash

set -euo pipefail

# ---------------------
NCE_ISO_PATH="${HOME}/Downloads/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga.iso"
# ---------------------

function show_header() {
	local message=$1
	echo "====================================="
	echo "✅ $message"
	echo "====================================="
}

function log() {
	echo "[$(date +%H:%M:%S)] $1"
}

# 1. Setup
show_header "Setting up workspace..."
mkdir -p "${NCE_TEMP}/workspace/extracted_initrd" "${NCE_TEMP}/workspace/mount_iso" || {
	log "Failed to create workspace directory"
	exit 1
}
log "Workspace setup complete"

# 2. Mount ISO
show_header "Mounting ISO..."
sudo mount -o loop "${NCE_ISO_PATH}" "${NCE_TEMP}/workspace/mount_iso" || {
	log "Failed to mount ISO"
	exit 1
}
log "ISO mounted successfully"

# 3. Extract Small Initrd
show_header "Extracting Initrd..."
cd "${NCE_TEMP}/workspace/extracted_initrd" || {
	log "Failed to change directory"
	exit 1
}
zcat "${NCE_TEMP}/workspace/mount_iso/boot/initrd" | cpio -idm --quiet || {
	log "Failed to extract initrd"
	exit 1
}
log "Initrd extracted successfully"

# 4. Insert the SquashFS
show_header "Embedding SquashFS into Initrd..."
cp "${NCE_TEMP}/workspace/mount_iso/squashfs.img" . || {
	log "Failed to copy SquashFS image"
	exit 1
}
log "SquashFS image copied successfully"

# 5. Repack into one Giant File
show_header "Repacking Monolithic Initrd (This will take a moment)..."
find . -print0 | cpio --null -o --format=newc | gzip -1 > "${NCE_TEMP}/initrd-monolithic" || {
	log "Failed to repack initrd"
	exit 1
}
log "Monolithic initrd repacked successfully"

# Show a summary
echo -e "\n"
echo -e "#########################"
echo -e "✅ DONE! File is at: ${NCE_TEMP}/initrd-monolithic"
echo -e "\tinitrd size (original): $(du -h "${NCE_TEMP}/workspace/mount_iso/boot/initrd" | cut -f1)"
echo -e "\tinitrd size (monolithic): $(du -h "${NCE_TEMP}/initrd-monolithic" | cut -f1)"
echo -e "\n"
echo -e "REMINDER: SCP this file to the iPXE server!"
echo -e "#########################"
EOF

chmod +x ${NCE_TEMP}/nce-initrd-monolithic.sh
${NCE_TEMP}/nce-initrd-monolithic.sh

# Option: Concat the files together
cat <<- 'EOF' > "${NCE_TEMP}/nce-initrd-concat.sh"
#!/usr/bin/env bash

set -euo pipefail

NCE_ISO_PATH="${HOME}/Downloads/phoenix.x86_64-fnd_5.6.1_patch-aos_6.8.1_ga.iso"

# 1. Setup Workspace
mkdir -p "${NCE_TEMP}/workspace/mount_iso"
sudo mount -o loop "${NCE_ISO_PATH}" "${NCE_TEMP}/workspace/mount_iso"

# 2. Define Inputs/Outputs
ORIG_INITRD="${NCE_TEMP}/workspace/mount_iso/boot/initrd"
SQUASHFS="${NCE_TEMP}/workspace/mount_iso/squashfs.img"
OUTPUT="${NCE_TEMP}/initrd-concat"

echo "Processing..."

# 3. Create a CPIO wrapper for the squashfs (No compression = Instant)
# Enter a subshell to switch dirs so the path in the CPIO is clean
(
  cd "${NCE_TEMP}/workspace/mount_iso"
  echo "squashfs.img" | cpio -H newc -o > "${NCE_TEMP}/squashfs-wrapper.cpio"
)

# 4. Concatenate: Original Initrd + Wrapped Squashfs = Monolithic
cat "${ORIG_INITRD}" "${NCE_TEMP}/squashfs-wrapper.cpio" > "${OUTPUT}"

echo "✅ DONE! Created: ${OUTPUT}"
echo "   Size: $(du -h ${OUTPUT} | cut -f1)"

# Cleanup
sudo umount "${NCE_TEMP}/workspace/mount_iso"
EOF

chmod +x ${NCE_TEMP}/nce-initrd-concat.sh
${NCE_TEMP}/nce-initrd-concat.sh
``

## Clean up

This is common to both methods.

```bash
cd ${HOME}
sudo umount ${NCE_TEMP}/workspace/mount_iso
sudo rm -rf ${NCE_TEMP}
````
