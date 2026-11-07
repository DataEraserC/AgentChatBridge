# NixOS module：可选启用静态 acbl（不默认启用）。
#
# acbl 独立于 payload/generation：package 提供 acb-launcher，版本切换只动
# current.json，不换这个 unit 的 ExecStart。
#
# 用法（flake / configuration.nix）：
#   imports = [ ./deploy/nix/nixos.nix ];
#   services.acb-launcher = {
#     enable = true;
#     package = self.packages.${system}.acb-launcher;  # 或 pkgs.acb-bridge
#     configFile = /etc/acb/supervisor.toml;
#   };
#
# 模块不依赖 NixOS generation：单元的 ExecStart 只是 acb-launcher 和配置文件路径，
# 版本由 state_dir/current.json 决定，回滚由 launcher/supervisor 执行。
{ config, lib, pkgs, ... }:
let
  cfg = config.services.acb-launcher;
in
{
  options.services.acb-launcher = {
    enable = lib.mkEnableOption "static acbl systemd front-end";
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
      default = "/var/lib/acb";
      description = "launcher 的工作目录（state_dir 的相对路径以它为准）";
    };
    user = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      description = "运行用户；null = systemd 默认（root）";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.acb-launcher = {
      description = "ACB launcher (static acbl; observer/restorer)";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];
      unitConfig = {
        StartLimitIntervalSec = 60;
        StartLimitBurst = 3;
        StartLimitAction = "none";
      };
      serviceConfig = {
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
      } // lib.optionalAttrs (cfg.user != null) {
        User = cfg.user;
      };
    };
  };
}
