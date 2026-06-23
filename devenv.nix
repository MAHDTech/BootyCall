{
  pkgs,
  config,
  lib,
  ...
}:
let

  packages = with pkgs; [
    bashInteractive
    pre-commit
  ];

  devPackages = with pkgs; [
    bc
    binutils
    bison
    cdrkit
    cpio
    elfutils
    figlet
    file
    flex
    gcc
    git
    gnumake
    gnutar
    hello
    jq
    kmod
    libelf
    openssl
    p7zip
    perl
    pigz
    rpm
    rsync
    squashfsTools
    wget
  ];

in
{
  name = "BootyCall";

  env = {
    PROJECT = config.name;
  };

  cachix = {
    pull = [
      "mahdtech"
    ];
    push = "mahdtech";
  };

  devenv = {
    warnOnNewVersion = true;
  };

  dotenv = {
    enable = true;
    disableHint = false;
  };

  difftastic = {
    enable = true;
  };

  packages =
    packages ++ lib.optionals (!config.container.isBuilding || config.name == "devenv") devPackages;

  enterShell = ''
    figlet -f starwars -w 180 $PROJECT

    hello --greeting="Hello ''${USER:-user}, welcome to the $PROJECT project!"

    echo ""
    echo "#########################"
    echo "#### Helper scripts #####"
    echo "#########################"
    echo "🦾"
    ${pkgs.gnused}/bin/sed -e 's| |••|g' -e 's|=| |' <<EOF | ${pkgs.util-linuxMinimal}/bin/column -t | ${pkgs.gnused}/bin/sed -e 's|^|🦾 |' -e 's|••| |g'
    ${lib.generators.toKeyValue { } (lib.mapAttrs (_name: value: value.description) config.scripts)}
    EOF
    echo "🦾"
    echo "#########################"
  '';

  languages = {
    nix.enable = true;
    rust = {
      enable = true;
      toolchainFile = ./rust-toolchain.toml;
      lsp.enable = true;
    };
    shell = {
      enable = true;
    };
  };

  git-hooks = {
    excludes = [
      ".devenv/"
      ".git/"
      "^.vscode/"
      "target/"
      "scratch/"
    ];
    hooks = {
      actionlint.enable = true;
      action-validator.enable = true;
      check-json.enable = true;
      check-merge-conflicts.enable = true;
      check-shebang-scripts-are-executable = {
        enable = true;
        excludes = [
          ".*\\.ipxe"
        ];
      };
      check-symlinks.enable = true;
      check-yaml.enable = true;
      commitizen.enable = true;
      cargo-check = {
        enable = true;
        package = config.languages.rust.toolchainPackage;
      };
      clippy = {
        enable = true;
        package = config.languages.rust.toolchainPackage;
        settings = {
          denyWarnings = true;
          offline = true;
          allFeatures = true;
        };
      };
      convco.enable = true;
      deadnix.enable = true;
      editorconfig-checker.enable = true;
      gptcommit.enable = true;
      markdownlint = {
        enable = true;
        settings = {
          configuration = {
            MD013 = {
              line_length = 500;
            };
            MD033 = {
              allowed_elements = [
                "a"
                "br"
                "nobr"
                "pre"
                "sup"
              ];
            };
          };
        };
      };
      mixed-line-endings.enable = true;
      nixfmt-rfc-style.enable = true;
      pre-commit-hook-ensure-sops.enable = true;
      prettier = {
        enable = true;
        excludes = [
          ".devcontainer.json"
        ];
        settings = {
          configPath = ".prettierrc.yaml";
          plugins = [
          ];
        };
      };
      pretty-format-json.enable = false; # using prettier
      ripsecrets.enable = true;
      rustfmt = {
        enable = true;
        package = config.languages.rust.toolchainPackage;
      };
      shellcheck = {
        enable = true;
        excludes = [
          ".env"
        ];
      };
      shfmt.enable = true;
      statix.enable = true;
      tflint.enable = true;
      trim-trailing-whitespace.enable = true;
      trufflehog.enable = false;
      typos.enable = true;
      yamllint = {
        enable = true;
        settings = {
          configuration = ''
            extends: relaxed
            rules:
              line-length: disable
              indentation: enable
          '';
        };
      };
    };
  };

  starship = {
    enable = true;
    config = {
      enable = false;
    };
  };

  devcontainer = {
    enable = true;
    settings = {
      customizations = {
        vscode = {
          extensions = [
            "arrterian.nix-env-selector"
            "brettm12345.nixfmt-vscode"
            "dotenv.dotenv-vscode"
            "esbenp.prettier-vscode"
            "github.vscode-github-actions"
            "github.vscode-pull-request-github"
            "gruntfuggly.todo-tree"
            "hediet.vscode-drawio"
            "jnoortheen.nix-ide"
            "mkhl.direnv"
            "nhoizey.gremlins"
            "pinage404.nix-extension-pack"
            "redhat.vscode-yaml"
            "skellock.just"
            "streetsidesoftware.code-spell-checker"
            "tamasfe.even-better-toml"
            "tekumura.typos-vscode"
            "timonwong.shellcheck"
            "tuxtina.json2yaml"
            "vscodevim.vim"
            "waderyan.gitblame"
            "wakatime.vscode-wakatime"
            "yzhang.markdown-all-in-one"
          ];
        };
      };
    };
  };

  scripts = {
    create-iso-nce = {
      package = pkgs.bash;
      description = "Create ISO for NCE";
      exec = ''
        echo "This will create/update a custom Nutanix Community Edition ISO."
        echo ""
        read -rp "Ready to start? " ANSWER
        ANSWER=$(echo "''${ANSWER:-}" | tr '[:upper:]' '[:lower:]')
        if [[ "$ANSWER" == "y" || "$ANSWER" == "yes" ]];
        then
          ./tftpboot/scripts/nce/nce_create_iso.sh
        else
          echo "Ok then, goodbye!"
        fi
      '';
    };
    create-iso-systemd-boot = {
      package = pkgs.bash;
      description = "Create ISO for Systemd Boot";
      exec = ''
        echo "This will create/update a custom Systemd Boot ISO."
        echo ""
        read -rp "Ready to start? " ANSWER
        ANSWER=$(echo "''${ANSWER:-}" | tr '[:upper:]' '[:lower:]')
        if [[ "$ANSWER" == "y" || "$ANSWER" == "yes" ]];
        then
          ./tftpboot/scripts/software/update-systemd-boot.sh
        else
          echo "Ok then, goodbye!"
        fi
      '';
    };
    create-iso-refind = {
      package = pkgs.bash;
      description = "Create ISO for Refind";
      exec = ''
        echo "This will create/update a custom Refind ISO."
        echo ""
        read -rp "Ready to start? " ANSWER
        ANSWER=$(echo "''${ANSWER:-}" | tr '[:upper:]' '[:lower:]')
        if [[ "$ANSWER" == "y" || "$ANSWER" == "yes" ]];
        then
          ./tftpboot/scripts/software/update-refind.sh
        else
          echo "Ok then, goodbye!"
        fi
      '';
    };
  };

  enterTest = ''
    echo "Running devenv tests..."
  '';

}
