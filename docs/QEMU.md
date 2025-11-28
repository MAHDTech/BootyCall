# QEMU for Testing iPXE on NixOS

## Pre-requisites

- Start a nix-shell with the required packages:

```bash
nix-shell -p qemu_full OVMFFull
```

- Create writable EFI vars file (NVRAM persistence; 64M for boot order):

```bash
echo "Preparing EFI vars file..."

OVMF_CODE=$(find /nix/store -name OVMF_CODE.fd | head -1)
OVMF_VARS=$(find /nix/store -name OVMF_VARS.fd | head -1)

sudo cp -f "$OVMF_VARS" /tmp/bios.bin
sudo chmod 666 /tmp/bios.bin

echo "EFI vars have been prepared!"
```

## Launch VM

Launch a VM using a q35 based machine for modern UEFI/PXE.

```bash
qemu-system-x86_64 \
	-name "Nutanix-Test-VM" \
	-machine q35 \
	-m 32G \
	-cpu host \
	-enable-kvm \
	-drive if=pflash,format=raw,readonly=on,file="${OVMF_CODE}" \
	-drive if=pflash,format=raw,file=/tmp/bios.bin \
	-vga qxl \
	-display gtk \
	-device virtio-net-pci,netdev=n1,mac=52:54:00:10:10:10 \
	-netdev bridge,id=n1,br=br0,helper=/run/wrappers/bin/qemu-bridge-helper \
	-boot n \
	-serial mon:stdio
```
