# Home Manager module：可选启用静态 acbl（不默认启用）。
#
# acbl 独立于 payload/generation：package 提供 acb-launcher，版本切换只动
# current.json，不换这个 unit 的 ExecStart。
#
# 用法：
#   imports = [ ./deploy/nix/home-manager.nix ];
#   services.acb-launcher = {
#     enable = true;
#     package = self.packages.${system}.acb-launcher;  # 或 pkgs.acb-bridge
#     configFile = "${config.home.homeDirectory}/.config/acb/supervisor.toml";
#   };
#
# 与 NixOS module 一样不依赖 generation：ExecStart 只是 launcher + 配置路径。
{ config, lib, pkgs, ... }:
let
  cfg = config.services.acb-launcher;
in
{
  options.services.acb-launcher = {
    enable = lib.mkEnableOption "static acbl user systemd front-end";
    package = lib.mkOption {
      type = lib.types.package;
      description = "包含 acb-launcher 的包";
    };
    configFile = lib.mkOption {
      type = lib.types.path;
      description = "supervisor.toml 路径";
    };
    workingDirectory = lib.mkOption {
      type = lib.types.str;
      description = "launcher 的工作目录（state_dir 的相对路径以它为准）";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.user.services.acb-launcher = {
      Unit = {
        Description = "ACB launcher (static acbl; observer/restorer)";
        After = [ "network.target" ];
        StartLimitIntervalSec = 60;
        StartLimitBurst = 3;
        StartLimitAction = "none";
      };
      Service = {
        Type = "notify";
        NotifyAccess = "main";
        WatchdogSec = 120;
        ExecStart = "${cfg.package}/bin/acb-launcher --config ${cfg.configFile}";
        WorkingDirectory = cfg.workingDirectory;
        Restart = "on-failure";
        RestartSec = 1;
        RestartForceExitStatus = [ 75 ];
        RestartPreventExitStatus = [ 78 ];
        TimeoutStopSec = 45;
        # acbl 只观察/恢复 bridge，不是它的所有者：停/重启 acbl 不能连带杀 bridge。
        KillMode = "process";
      };
      Install.WantedBy = [ "default.target" ];
    };
  };
}
