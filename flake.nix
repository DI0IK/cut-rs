{
  description = "cut-rs: Local Antigravity AI quota tracker, Waybar module, and on-demand cost estimator";

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
            version = "0.4.1";
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
            version = "0.4.1";
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
    };
}
