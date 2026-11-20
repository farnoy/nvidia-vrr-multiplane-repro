{
  description = "Minimal atomic KMS repro for NVIDIA VRR multi-plane commit stutter";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/d5cc6833999746cffdfe0598f7bc801a57b887a9";

  outputs =
    { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      drm-plane-bench = pkgs.pkgsStatic.rustPlatform.buildRustPackage {
        pname = "drm-plane-bench";
        version = "0.1.0";
        src = pkgs.lib.cleanSource ./.;
        cargoLock.lockFile = ./Cargo.lock;
        meta.mainProgram = "drm-plane-bench";
      };
    in
    {
      packages.${system}.default = drm-plane-bench;
      devShells.${system}.default = pkgs.mkShell {
        packages = [
          pkgs.cargo
          pkgs.rustc
          pkgs.clippy
        ];
      };
    };
}
