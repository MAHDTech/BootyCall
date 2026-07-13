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
    bashInteractive
    bootycall
    cargo-audit
    cargo-deny # RustSec advisories + license/ban/source policy
    cargo-edit # Adds 'cargo upgrade'
    cargo-machete # Detects unused workspace dependencies
    figlet
    gh
    hello
    nil
    toml-cli
    trivy
  ];

  enterShell = ''
    if [[ "''${CI:-false}" == "true" ]]; then
      echo "devenv running in CI"
    else
      figlet -f slant -w 180 "$(echo "$PROJECT" | tr '[:lower:]-' '[:upper:] ')"

      hello --greeting="Hello ''${USER:-user}, welcome to the $PROJECT project."

      ${lib.optionalString (config.scripts != { }) ''
        echo ""
        echo "#########################"
        echo "#### Helper scripts #####"
        echo "#########################"
        echo "🦾"
        ${lib.concatStrings (
          lib.mapAttrsToList (
            name: value: "printf '🦾 %-20s  %s\\n' '${name}' '${value.description}'\n"
          ) config.scripts
        )}
        echo "🦾"
        echo "#########################"
      ''}
    fi
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
      ".*\\.drawio$"
      ".devenv/"
      "\\.git(/.*)?$"
      "^.vscode/"
      "^\\.cache(/.*)?$"
      "^\\.devenv(/.*)?$"
      "^\\.direnv(/.*)?$"
      "^\\.git(/.*)?$"
      "^scratch(/.*)?$"
      "target/"
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
      convco.enable = true;
      check-symlinks.enable = true;
      check-yaml.enable = true;
      commitizen.enable = true;
      cspell = {
        enable = true;
        args = [
          "--no-must-find-files"
        ];
      };
      cargo-check = {
        enable = true;
        package = config.languages.rust.toolchainPackage;
        args = [
          "--all-features"
        ];
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

  scripts = {
    version = {
      package = pkgs.bash;
      description = "Bump workspace version using conventional commits or explicit bump level (MAJOR/MINOR/PATCH)";
      exec = ''
        ./scripts/version.sh "$@"
      '';
    };
    build-ipxe-local = {
      package = pkgs.bash;
      description = "Build and populate local tftpboot/boot/ with compiled iPXE binaries";
      exec = ''
        echo "Building BootyCall assets (including iPXE binaries)..."
        nix build .#assets --out-link result-assets
        mkdir -p tftpboot/boot
        cp -fvR result-assets/tftpboot/boot/* tftpboot/boot/
        rm -f result-assets
        echo "Done! Populated local tftpboot/boot/ directory with compiled iPXE binaries."
      '';
    };
  };

  enterTest = ''
    if [[ "''${CI:-false}" == "true" ]]; then
      echo "Skipping workspace tests in CI (handled by cargo-test job)"
    else
      echo "Running workspace tests..."
      # Actually run the suite so `devenv test` (locally and the ci-devenv-test
      # job) is meaningful. --all-features matches ci-cargo-test and the clippy
      # hook so feature-gated tests run too.
      cargo test --workspace --all-features
    fi
  '';
}
