{ pkgs, config, lib, inputs'screenstub, ... }: with lib; let
  cfg = config.programs.screenstub;
  settingsFormat = pkgs.formats.yaml { };
  settingsModule = { config, ... }: {
    freeformType = settingsFormat.type;
  };
  screenstubPackage = pkgs.callPackage (inputs'screenstub + "/derivation.nix") (optionalAttrs (inputs'screenstub ? lib.crate) {
    inherit (inputs'screenstub.lib) crate;
  });
in {
  options.programs.screenstub = with types; {
    enable = mkEnableOption "screenstub";

    package = mkOption {
      type = package;
      default = screenstubPackage;
    };

    finalPackage = mkOption {
      type = package;
      default = pkgs.writeShellScriptBin "screenstub" ''
        exec ${getExe cfg.package} -c ${cfg.configFile} "$@"
      '';
    };

    settings = mkOption {
      type = submodule settingsModule;
      default = { };
    };

    configFile = mkOption {
      type = path;
      default = settingsFormat.generate "screenstub.yml" cfg.settings;
      defaultText = literalExpression "config.programs.screenstub.settings";
    };
  };

  config = {
    _module.args.inputs'screenstub = mkOptionDefault {
      outPath = ./.;
    };
  };
}
