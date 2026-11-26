self:
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.tuyau;
  toml = pkgs.formats.toml { };
  configFile = toml.generate "tuyau-config.toml" cfg.settings;
in
{
  options.programs.tuyau = {
    enable = lib.mkEnableOption "tuyau, a declarative PipeWire patchbay";

    package = lib.mkOption {
      type = lib.types.package;
      default = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
      defaultText = lib.literalExpression "tuyau.packages.\${system}.default";
      description = "The tuyau package to use.";
    };

    settings = lib.mkOption {
      inherit (toml) type;
      default = { };
      description = "Written to {file}`$XDG_CONFIG_HOME/tuyau/config.toml`. See the README for the schema.";
      example = lib.literalExpression ''
        {
          node = [
            { match = "alsa_output.usb-Focusrite*"; alias = "Monitors"; volume = 0.8; }
            { match = { application = "spotify"; }; alias = "Music"; }
          ];
          link = [
            { from = "Music"; to = "Monitors"; exclusive = true; }
          ];
        }
      '';
    };

    service.enable = lib.mkEnableOption "a user service running `tuyau watch`, which recreates the configured links whenever their nodes appear";
  };

  config = lib.mkIf cfg.enable {
    home.packages = [ cfg.package ];

    xdg.configFile."tuyau/config.toml" = lib.mkIf (cfg.settings != { }) { source = configFile; };

    systemd.user.services.tuyau = lib.mkIf cfg.service.enable {
      Unit = {
        Description = "tuyau PipeWire patchbay";
        After = [
          "pipewire.service"
          "wireplumber.service"
        ];
        Requires = [ "pipewire.service" ];
        X-Restart-Triggers = [ "${configFile}" ];
      };
      Service = {
        ExecStart = "${lib.getExe cfg.package} watch";
        Restart = "on-failure";
        RestartSec = 2;
      };
      Install.WantedBy = [ "pipewire.service" ];
    };
  };
}
