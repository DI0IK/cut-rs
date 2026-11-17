{
  description = "cut-rs: Crowdsourced AI quota telemetry & statistical estimator";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f:
        nixpkgs.lib.genAttrs supportedSystems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs:
        let
          lib = pkgs.lib;
          src = lib.cleanSourceWith {
            src = ./.;
            filter = path: type:
              let
                base = baseNameOf (toString path);
              in
              !(
                base == "target"
                || base == "node_modules"
                || base == "dist"
                || base == ".devenv"
                || base == ".direnv"
                || lib.hasPrefix "cut_rs.db" base
              ) && lib.cleanSourceFilter path type;
          };

          web = pkgs.buildNpmPackage {
            pname = "cut-rs-web";
            version = "0.1.0";
            src = lib.cleanSourceWith {
              src = ./web;
              filter = path: type:
                let
                  base = baseNameOf (toString path);
                in
                !(base == "node_modules" || base == "dist") && lib.cleanSourceFilter path type;
            };
            npmDeps = pkgs.importNpmLock { npmRoot = ./web; };
            npmConfigHook = pkgs.importNpmLock.npmConfigHook;
            installPhase = ''
              runHook preInstall
              cp -r dist $out
              runHook postInstall
            '';
          };
        in
        {
          inherit web;
          default = pkgs.rustPlatform.buildRustPackage {
            pname = "cut-rs";
            version = "0.1.0";
            inherit src;

            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [ pkgs.sqlite ];

            preBuild = ''
              rm -rf web/dist
              cp -r ${web} web/dist
            '';
          };
        });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            rustc
            cargo
            clippy
            rustfmt
            pkg-config
            sqlite
          ];
        };
      });

      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.cut-rs;
        in
        {
          options.services.cut-rs = {
            enable = lib.mkEnableOption "cut-rs AI quota telemetry server";

            package = lib.mkOption {
              type = lib.types.package;
              default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
              description = "The cut-rs package to use.";
            };

            host = lib.mkOption {
              type = lib.types.str;
              default = "127.0.0.1";
              description = "Address to bind the HTTP server to.";
            };

            port = lib.mkOption {
              type = lib.types.port;
              default = 8080;
              description = "Port to bind the HTTP server to.";
            };

            databaseUrl = lib.mkOption {
              type = lib.types.str;
              default = "sqlite:///var/lib/cut-rs/cut_rs.db?mode=rwc";
              description = "SQLite database URL.";
            };

            trustProxyHeaders = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Trust X-Forwarded-For / X-Real-IP headers from a reverse proxy.";
            };

            environmentFile = lib.mkOption {
              type = lib.types.nullOr lib.types.path;
              default = null;
              description = "Optional environment file (e.g. for HMAC_SECRET).";
            };

            extraEnv = lib.mkOption {
              type = lib.types.attrsOf lib.types.str;
              default = { };
              description = "Additional environment variables passed to cut-rs.";
            };

            openFirewall = lib.mkOption {
              type = lib.types.bool;
              default = false;
              description = "Whether to open the configured port in the firewall.";
            };
          };

          config = lib.mkIf cfg.enable {
            users.users.cut-rs = {
              isSystemUser = true;
              group = "cut-rs";
              home = "/var/lib/cut-rs";
            };
            users.groups.cut-rs = { };

            networking.firewall.allowedTCPPorts = lib.mkIf cfg.openFirewall [ cfg.port ];

            systemd.services.cut-rs = {
              description = "cut-rs AI Quota Telemetry Server";
              wantedBy = [ "multi-user.target" ];
              after = [ "network.target" ];

              environment = {
                HOST = cfg.host;
                PORT = toString cfg.port;
                DATABASE_URL = cfg.databaseUrl;
                TRUST_PROXY_HEADERS = if cfg.trustProxyHeaders then "true" else "false";
              } // cfg.extraEnv;

              serviceConfig = {
                ExecStart = "${cfg.package}/bin/cut-rs serve";
                Restart = "on-failure";
                RestartSec = "5s";
                User = "cut-rs";
                Group = "cut-rs";
                StateDirectory = "cut-rs";
                StateDirectoryMode = "0750";
                EnvironmentFile = lib.mkIf (cfg.environmentFile != null) cfg.environmentFile;

                # Hardening
                NoNewPrivileges = true;
                PrivateTmp = true;
                ProtectSystem = "strict";
                ProtectHome = true;
                ProtectKernelTunables = true;
                ProtectKernelModules = true;
                ProtectControlGroups = true;
                RestrictAddressFamilies = [ "AF_INET" "AF_INET6" "AF_UNIX" ];
                CapabilityBoundingSet = "";
                LockPersonality = true;
              };
            };
          };
        };
    };
}
