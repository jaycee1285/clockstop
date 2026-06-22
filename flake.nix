{
  description = "clockstop focus-session tray daemon";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      mkPackage = pkgs:
        pkgs.rustPlatform.buildRustPackage {
          pname = "clockstop";
          version = "0.1.0";
          src = self;

          cargoLock.lockFile = ./Cargo.lock;

          postInstall = ''
            install -Dm644 ${./packaging/linux/clockstop.desktop} \
              $out/share/applications/clockstop.desktop
          '';
        };
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = import nixpkgs { inherit system; };
          clockstop = mkPackage pkgs;
        in
        {
          inherit clockstop;
          default = clockstop;
        });

      apps = forAllSystems (system: {
        clockstop = {
          type = "app";
          program = "${self.packages.${system}.clockstop}/bin/clockstop";
        };
        default = self.apps.${system}.clockstop;
      });

      devShells = forAllSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.mkShell {
            packages = [
              pkgs.cargo
              pkgs.clippy
              pkgs.rustc
              pkgs.rustfmt
            ];
          };
        });
    };
}
