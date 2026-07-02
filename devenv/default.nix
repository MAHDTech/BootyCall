{
  pkgs,
  config,
  lib,
  ...
}:
let
  mkCrate = import ../packages/crate.nix { inherit pkgs; };
  bootycall = mkCrate "bootycall-rs";
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

  dotenv = {
    enable = true;
    disableHint = false;
  };

  difftastic = {
    enable = true;
  };

  packages = with pkgs; [
    figlet
    hello
    gh
    bashInteractive
    nil
    bootycall
  ];

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
      toolchainFile = ../rust-toolchain.toml;
      lsp.enable = true;
    };
    shell = {
      enable = true;
    };
  };

  git-hooks = {
    excludes = [
      ".devenv/"
      "\\.git(/.*)?$"
      "^.vscode/"
      "target/"
      "^scratch(/.*)?$"
    ];
    hooks = {
      actionlint.enable = true;
      action-validator.enable = true;
      check-version-bump = {
        enable = true;
        name = "Check Cargo.toml Version Bump";
        entry = "./scripts/check-version-bump.sh";
        files = "^Cargo\\.toml$";
        pass_filenames = false;
      };
      check-json.enable = true;
      check-merge-conflicts.enable = true;
      check-shebang-scripts-are-executable = {
        enable = true;
        excludes = [
          ".*\\.ipxe"
        ];
      };
      convco.enable = true;
      check-symlinks.enable = true;
      check-yaml.enable = true;
      commitizen.enable = true;
      cspell.enable = true;
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
      deadnix = {
        enable = true;
        settings = {
          noUnderscore = true;
        };
      };
      editorconfig-checker.enable = true;
      lychee.enable = true;
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
      nixfmt.enable = true;
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
      ripsecrets.enable = true;
      rustfmt = {
        enable = true;
        package = config.languages.rust.toolchainPackage;
      };
      shellcheck = {
        enable = true;
        args = [
          "--external-sources"
        ];
        excludes = [
          ".env"
        ];
      };
      shfmt.enable = true;
      statix.enable = true;
      trim-trailing-whitespace.enable = true;
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
            "streetsidesoftware.code-spell-checker"
            "tamasfe.even-better-toml"
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

  scripts = { };

  enterTest = ''
    echo "Running devenv tests..."
  '';
}
