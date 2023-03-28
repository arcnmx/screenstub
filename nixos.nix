{ pkgs, config, lib, ... }: with lib; let
  cfg = config.programs.screenstub;
in {
  imports = [ ./module.nix ];

  config = mkIf cfg.enable {
    environment.systemPackages = [ cfg.finalPackage ];
  };
}
