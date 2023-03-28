{ config, pkgs, lib, ... }: with pkgs; with lib; let
  screenstub = import ./. { inherit pkgs; };
  inherit (screenstub) checks packages;
  screenstub-checked = (packages.screenstub.override {
    buildType = "debug";
  }).overrideAttrs (_: {
    doCheck = true;
  });
in {
  config = {
    name = "screenstub";
    ci.gh-actions.enable = true;
    cache.cachix.arc.enable = true;
    channels = {
      nixpkgs = "22.11";
    };
    tasks = {
      build.inputs = singleton screenstub-checked;
      check-config.inputs = singleton (checks.check-config.override {
        screenstub = screenstub-checked;
      });
    };
    jobs = {
      nixos = {
        system = "x86_64-linux";
      };
      #macos.system = "x86_64-darwin";
    };
  };
}
