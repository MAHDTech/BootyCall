# Readme

Notes on building custom ipxe files.

## Part 0: Preparation

Prepare to build iPXE.

- Clone the repo

```bash
git clone https://github.com/ipxe/ipxe.git --depth 1 --branch master
```

- Change into the right directory

```bash
cd ipxe/src
```

- Patch the Makefiles for NixOS compatibility

```bash
sed -i 's|/bin/echo|echo|g' Makefile Makefile.housekeeping
sed -i 's|-mlittle-endian||g' arch/arm64/Makefile
```

- Enable options for additional feature support;

```bash
# Enable CONSOLE_CMD for console, colour, and cpair support
sed -i 's|//[[:space:]]*#define[[:space:]]\+CONSOLE_CMD|#define CONSOLE_CMD|' config/general.h

# Enable CONSOLE_FRAMEBUFFER for framebuffer support
sed -i 's|//[[:space:]]*#define[[:space:]]\+CONSOLE_FRAMEBUFFER|#define CONSOLE_FRAMEBUFFER|' config/console.h

# Enable IMAGE_PNG for PNG image support
sed -i 's|//[[:space:]]*#define[[:space:]]\+IMAGE_PNG|#define IMAGE_PNG|' config/general.h

# Enable reboot and poweroff support
sed -i 's|//[[:space:]]*#define[[:space:]]\+REBOOT_CMD|#define REBOOT_CMD|' config/general.h
sed -i 's|//[[:space:]]*#define[[:space:]]\+POWEROFF_CMD|#define POWEROFF_CMD|' config/general.h

# Enable ping command in the iPXE shell.
sed -i 's|//[[:space:]]*#define[[:space:]]\+PING_CMD|#define PING_CMD|' config/general.h

# Enable NTP
sed -i 's|//[[:space:]]*#define[[:space:]]\+NTP_CMD|#define NTP_CMD|' config/general.h

# Enable NSLOOKUP command
sed -i 's|//[[:space:]]*#define[[:space:]]\+NSLOOKUP_CMD|#define NSLOOKUP_CMD|' config/general.h

# Enable download protocols
sed -i 's|//[[:space:]]*#define[[:space:]]\+DOWNLOAD_PROTO_TFTP|#define DOWNLOAD_PROTO_TFTP|' config/general.h
sed -i 's|//[[:space:]]*#define[[:space:]]\+DOWNLOAD_PROTO_HTTP|#define DOWNLOAD_PROTO_HTTP|' config/general.h
```

- Make the embed script (amd64)

```bash
cat > embed-amd64.ipxe <<'EOF'
#!ipxe

#########################
# Initialisation
#########################

set timeout 10000
ifopen || goto fail
dhcp || goto fail

goto config

#########################
:config
#########################

echo Loading iPXE configuration ...
sleep 1

chain --autofree tftp://${next-server}/ipxe/config.ipxe ||
chain --autofree http://${next-server}/ipxe/config.ipxe ||
chain --autofree http://${next-server}:8080/ipxe/config.ipxe ||
goto fail

#########################
# Fail
#########################
:fail

echo iPXE boot has failed, dropping to shell...
shell
reboot
EOF
```

## Part 1: Native AMD64 Builds

Build the x86_64 EFI and legacy BIOS iPXE images using native tools.

- Create a nix shell config for native builds

```bash
cat > shell-amd64.nix <<'EOF'
let
  pkgs = import <nixpkgs> { };

  # Include the required perl dependencies
  perlEnv = with pkgs.perlPackages; perl.withPackages (ps: with ps; [
    ExtUtilsMakeMaker
    IOCompress DigestSHA ArchiveZip
    CryptOpenSSLRSA CryptX509 CryptOpenSSLX509
  ]);

in pkgs.mkShell {

  # Native build tools
  nativeBuildInputs = with pkgs; [
    git
    gnumake
    gcc
    binutils-unwrapped
    xz
    zlib
    mtools
    cdrtools
    syslinux
    gawk
    bison
    flex
    libusb1
    openssl
    bc
    cdrkit
    python3
    perlEnv
  ];

  shellHook = ''
    echo "≈≈≈≈≈ iPXE native build environment ready ≈≈≈≈≈"
    echo "Native arch: $(uname -m)"
    echo "gcc → $(which gcc)"
  '';
}
EOF
```

- Launch the nix shell for native builds

```bash
nix-shell shell-amd64.nix
```

- Build the native images

```bash
# x86_64 UEFI
make -j$(nproc) bin-x86_64-efi/ipxe.efi \
    EMBED=embed-amd64.ipxe \
    CONFIG=console \
    CONFIG=image \
    CONFIG=pci \
    CONFIG=usb \
    VERSION_MAJOR=1 \
    VERSION_MINOR=0 \
    VERSION_PATCH=0

# Legacy BIOS
make -j$(nproc) bin/undionly.kpxe \
    EMBED=embed-amd64.ipxe \
    CONFIG=console \
    CONFIG=image \
    CONFIG=pci \
    CONFIG=usb \
    VERSION_MAJOR=1 \
    VERSION_MINOR=0 \
    VERSION_PATCH=0

# SNPOnly version
make bin-x86_64-efi/snponly.efi \
    EMBED=embed-amd64.ipxe \
    CONFIG=console \
    CONFIG=image \
    CONFIG=pci \
    CONFIG=usb \
    VERSION_MAJOR=1 \
    VERSION_MINOR=0 \
    VERSION_PATCH=0
```

- Exit the shell

```bash
exit
```

## Part 2: Cross-compiled ARM64 Builds

Build the ARM64 UEFI iPXE image using cross-compilation tools.

- Create a nix shell config for cross-compilation

```bash
cat > shell-arm64.nix <<'EOF'
let

  pkgs = import <nixpkgs> { };
  # Cross toolchain for aarch64

  crossPkgs = pkgs.pkgsCross.aarch64-multiplatform;
  # Include the required perl dependencies

  perlEnv = with pkgs.perlPackages; perl.withPackages (ps: with ps; [
    ExtUtilsMakeMaker
    IOCompress DigestSHA ArchiveZip
    CryptOpenSSLRSA CryptX509 CryptOpenSSLX509
  ]);

in pkgs.mkShell {

  # Native build tools
  nativeBuildInputs = with pkgs; [
    git
    gnumake
    xz
    zlib
    gawk
    bison
    flex
    libusb1
    bc
    python3
    perlEnv
  ];

  # Cross-compiling build tools
  buildInputs = with crossPkgs; [
    stdenv.cc
    binutils
  ];

  shellHook = ''
    # Create a temporary bin directory for symlinks
    IPXE_BIN="/tmp/ipxe-bin"
    mkdir -p "$IPXE_BIN"
    TOOLS=(
      gcc
      g++
      as
      ld
      objcopy
      objdump
      ar
      strip
      ranlib
    )

    # Add native build tools to temp bin
    for tool in ''${TOOLS[@]};
    do
      if command -v "$tool" >/dev/null 2>&1;
      then
        ln -sf "$(which "$tool")" "$IPXE_BIN/$tool"
      fi
    done

    # Symlink unprefixed cross tools to prefixed versions (Nix uses 'unknown')
    for tool in ''${TOOLS[@]};
    do
      full_prefixed="aarch64-unknown-linux-gnu-$tool"
      if command -v "$full_prefixed" >/dev/null 2>&1;
      then
        ln -sf "$(which "$full_prefixed")" "$IPXE_BIN/$tool"
      fi
    done

    # Also create symlinks for the shorter prefix expected by iPXE
    for tool in ''${TOOLS[@]};
    do
      prefixed="aarch64-linux-gnu-$tool"
      full_prefixed="aarch64-unknown-linux-gnu-$tool"
      if command -v "$full_prefixed" >/dev/null 2>&1;
      then
        ln -sf "$(which "$full_prefixed")" "$IPXE_BIN/$prefixed"
      fi
    done

    export PATH="$IPXE_BIN:$PATH:${crossPkgs.stdenv.cc}/bin:${crossPkgs.binutils}/bin"
    # Use the shorter prefix for iPXE compatibility
    export CROSS="aarch64-linux-gnu-"

    echo "≈≈≈≈≈ iPXE cross-compilation build environment ready ≈≈≈≈≈"
    echo "Native arch: $(uname -m)"
    echo "aarch64-linux-gnu-gcc → $(which aarch64-linux-gnu-gcc || echo MISSING)"
    echo "aarch64-unknown-linux-gnu-gcc → $(which aarch64-unknown-linux-gnu-gcc || echo MISSING)"
    echo "as → $(which as)"
    echo "CROSS = $CROSS"
  '';
}
EOF
```

- Launch the nix shell for cross-compilation

```bash
nix-shell shell-arm64.nix
```

- Make the embed script (arm64)

```bash
cat > embed-arm64.ipxe <<'EOF'
#!ipxe

#########################
# Initialisation
#########################

set timeout 10000
ifopen net0 || goto snponly
dhcp net0 || goto snponly
goto ipxe

#########################
:ipxe
#########################

echo Loading iPXE configuration (ipxe.efi) ...
sleep 1

goto config_ipxe

#########################
:snponly
#########################

echo Failed to configure network using ipxe.efi, falling back to snponly.efi ...
sleep 1

chain --autofree tftp://${next-server}/boot/x64/snponly.efi ||
chain --autofree http://${next-server}/boot/x64/snponly.efi ||
chain --autofree http://${next-server}:8080/boot/x64/snponly.efi ||
goto fail

goto config_snponly

#########################
:config_ipxe
#########################

echo Loading iPXE configuration (ipxe.efi) ...
sleep 1

chain --autofree tftp://${next-server}/ipxe/config.ipxe ||
chain --autofree http://${next-server}/ipxe/config.ipxe ||
chain --autofree http://${next-server}:8080/ipxe/config.ipxe ||
goto fail

#########################
:config_snponly
#########################

echo Loading iPXE configuration (snmponly.efi) ...
sleep 1

# TODO: Create a non-menu failback for snponly.

chain --autofree tftp://${next-server}/ipxe/config.ipxe ||
chain --autofree http://${next-server}/ipxe/config.ipxe ||
chain --autofree http://${next-server}:8080/ipxe/config.ipxe ||
goto fail

#########################
# Fail
#########################
:fail

echo iPXE boot has failed, dropping to shell...
shell
reboot
EOF
```

- Build the ARM64 image

```bash
make -j$(nproc) bin-arm64-efi/ipxe.efi \
    EMBED=embed-arm64.ipxe \
    CROSS=aarch64-linux-gnu- \
    CONFIG=console \
    CONFIG=image \
    CONFIG=pci \
    CONFIG=usb \
    VERSION_MAJOR=1 \
    VERSION_MINOR=0 \
    VERSION_PATCH=0
```

- Exit the shell

```bash
exit
```

## Part 3: Transfer the files

- Transfer the files to the iPXE server

```bash
# BootyCall project
BOOTYCALL_HOME="${HOME}/Projects/syncthing/GitHub/MAHDTech/BootyCall"

# Legacy BIOS must be in root.
cp -f bin/undionly.kpxe "${BOOTYCALL_HOME}/tftpboot/undionly.kpxe"

# AMD64 UEFI ipxe.efi
cp -f bin-x86_64-efi/ipxe.efi "${BOOTYCALL_HOME}/tftpboot/boot/x64/ipxe.efi"

# AMD64 UEFI snponly.efi
cp -f bin-x86_64-efi/snponly.efi "${BOOTYCALL_HOME}/tftpboot/boot/x64/snponly.efi"

# ARM64
cp -f bin-arm64-efi/ipxe.efi "${BOOTYCALL_HOME}/tftpboot/boot/arm64/ipxe.efi"
```

- Don't forget to change the permissions on the iPXE server!

```bash
chown -R tftp:tftp /mnt/hdd/tftpboot
```

## Part 4: iPXE Wallpaper fun

See [iPXE Wallpapers](./iPXE_wallpapers.md)
