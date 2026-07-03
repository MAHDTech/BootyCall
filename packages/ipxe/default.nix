{
  lib,
  stdenv,
  writeText,
  ipxe,
}:

{
  pname,
  targets ? [ ],
  embedScript ? null,
  serverAddress ? null,
  httpPort ? 8080,
  additionalConfig ? [ ],
  disabledConfig ? [ ],
}:

let
  # Create a dummy syslinux package that supports all platforms (including arm64 and darwin).
  # We do this because we only compile UEFI targets and don't need syslinux, but standard
  # nixpkgs ipxe depends on it, which causes evaluation failures on those architectures.
  dummySyslinux = stdenv.mkDerivation {
    name = "syslinux";
    pname = "syslinux";
    src = writeText "dummy" "";
    dontUnpack = true;
    installPhase = "mkdir -p $out/share/syslinux";
    meta.platforms = lib.platforms.all;
  };

  # Override the base ipxe derivation to use our dummy syslinux
  ipxeOverridden = ipxe.override { syslinux = dummySyslinux; };

  # Generate the embed script if not provided
  defaultEmbedScript =
    let
      server = if serverAddress != null then serverAddress else "\${next-server}";
      portStr = toString httpPort;
    in
    ''
      #!ipxe

      # Wait for network link and run DHCP
      ifopen || goto fail
      dhcp || goto fail

      echo "Booting from BootyCall server: ${server}"

      # Try loading config via TFTP, then standard HTTP, then custom HTTP port
      chain --autofree tftp://${server}/ipxe/config.ipxe || \
      chain --autofree http://${server}/ipxe/config.ipxe || \
      chain --autofree http://${server}:${portStr}/ipxe/config.ipxe || \
      goto fail

      :fail
      echo "BootyCall boot failed. Dropping to interactive iPXE shell..."
      shell
      reboot
    '';

  embedScriptFile =
    if embedScript != null then
      (if lib.isString embedScript then writeText "${pname}-embed.ipxe" embedScript else embedScript)
    else
      writeText "${pname}-embed.ipxe" defaultEmbedScript;

  # Determine what build flags to pass based on targets.
  # If targets is empty, we decide based on the architecture.
  resolvedTargets =
    if targets != [ ] then
      targets
    else
      (
        if stdenv.hostPlatform.isAarch64 then
          [
            "bin-arm64-efi/ipxe.efi"
            "bin-arm64-efi/snp.efi"
          ]
        else
          [
            "bin-x86_64-efi/ipxe.efi"
            "bin-x86_64-efi/snp.efi"
          ]
      );

in
ipxeOverridden.overrideAttrs (oldAttrs: {
  inherit pname;

  # Override target binaries to build
  buildFlags = resolvedTargets;

  # Override configurePhase to inject our customization macros
  configurePhase =
    (oldAttrs.configurePhase or "")
    + "\n"
    + (lib.concatStringsSep "\n" (
      (map (opt: "echo \"#define ${opt}\" >> src/config/general.h") additionalConfig)
      ++ (map (opt: "echo \"#undef ${opt}\" >> src/config/general.h") disabledConfig)
    ));

  # Append embed script to make flags
  makeFlags = (oldAttrs.makeFlags or [ ]) ++ [
    "EMBED=${embedScriptFile}"
  ];

  # Override install phase to copy only the custom target files
  installPhase = ''
    runHook preInstall
    mkdir -p $out
    ${lib.concatMapStringsSep "\n" (target: ''
      filename=$(basename "${target}")
      # Translate snp.efi to snponly.efi for consistency with BootyCall expectations
      outname=$filename
      if [ "$filename" = "snp.efi" ]; then
        outname="snponly.efi"
      fi

      if [ -f "${target}" ]; then
        cp -v "${target}" "$out/$outname"
      elif [ -f "src/${target}" ]; then
        cp -v "src/${target}" "$out/$outname"
      else
        echo "Error: Target file ${target} not found" >&2
        exit 1
      fi
    '') resolvedTargets}
    runHook postInstall
  '';
})
