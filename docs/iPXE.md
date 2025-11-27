# Readme

Notes on building custom ipxe files.

## Steps

- Clone the repo

```bash
git clone https://github.com/ipxe/ipxe.git --depth 1 --branch master
```

- Change into the right directory

```bash
cd ipxe/src
```

- Create a nix shell config

```bash
cat > shell.nix <<'EOF'
let
  pkgs = import <nixpkgs> { };

  # Cross toolchain for aarch64
  crossPkgs = pkgs.pkgsCross.aarch64-multiplatform;

  # Include the required perl dependencies
  perlDeps = with pkgs.perlPackages; [
    ExtUtilsMakeMaker
    IOCompress DigestSHA ArchiveZip
    CryptOpenSSLRSA CryptX509 CryptOpenSSLX509
  ];

in pkgs.mkShell {

  # Native build tools
  nativeBuildInputs = with pkgs; [
    git
    gnumake
    gcc
    binutils-unwrapped
    perl
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
  ] ++ perlDeps;

  # Cross-compiling build tools
  buildInputs = with crossPkgs; [
    stdenv.cc
    binutils
  ];

  shellHook = ''
    export PERL5LIB="${pkgs.perl.makePerlPath perlDeps}"
    export CROSS="aarch64-linux-gnu-"
    export PATH="${crossPkgs.stdenv.cc}/bin:${crossPkgs.binutils}/bin:$PATH"
    
    echo "≈ iPXE build environment ready ≈"
    echo "Native arch: $(uname -m)"
    echo "gcc → $(which gcc)"
    echo "aarch64-linux-gnu-gcc → $(which aarch64-linux-gnu-gcc || echo MISSING)"
    echo "CROSS = $CROSS"
  '';
}
EOF
```

- Launch the nix shell

```bash
nix-shell
```

- Make the embed script

```bash
cat > embed.ipxe <<'EOF'
#!ipxe

dhcp || exit

chain --autofree tftp://${next-server}/ipxe/config.ipxe ||
chain --autofree http://${next-server}/ipxe/config.ipxe ||
chain --autofree http://${next-server}:8080/ipxe/config.ipxe ||

echo Could not contact server, dropping to shell ...
shell
reboot
EOF
```

- Run the build for different architectures

```bash
# UEFI
make -j$(nproc) bin-x86_64-efi/ipxe.efi \
    EMBED=embed.ipxe \
    CONFIG=cloud \
    CONFIG=console \
    CONFIG=image \
    CONFIG=pci \
    CONFIG=usb

# Legacy BIOS
make -j$(nproc) bin/undionly.kpxe \
    EMBED=embed.ipxe \
    CONFIG=cloud \
    CONFIG=console \
    CONFIG=image

# ARM64 UEFI
make -j$(nproc) bin-arm64-efi/ipxe.efi \
    EMBED=embed.ipxe \
    CROSS=aarch64-linux-gnu- \
    CONFIG=cloud \
    CONFIG=console \
    CONFIG=image
```

- Transfer the files to the iPXE server

```bash
# Legacy BIOS must be in root.
scp bin/undionly.kpxe root@bootycall.saltlabs.cloud:/mnt/hdd/tftpboot/undionly.kpxe

# AMD64 UEFI
scp bin-x86_64-efi/ipxe.efi root@bootycall.saltlabs.cloud:/mnt/hdd/tftpboot/boot/x64/ipxe.efi

# ARM64
scp bin-arm64-efi/ipxe.efi root@bootycall.saltlabs.cloud:/mnt/hdd/tftpboot/boot/arm64/ipxe.efi
```

- Don't forget to change the permissions on the iPXE server!

```bash
chown -R tftp:tftp /mnt/hdd/tftpboot
```

