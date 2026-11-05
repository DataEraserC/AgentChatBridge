# NixOS module：可选启用独立 issue 服务（默认不启用）。
#
# 用法：
#   imports = [ ./deploy/nix/issue-service.nix ];
#   services.acb-issue-service = {
#     enable = true;
#     package = self.packages.${system}.acb-issue-service;
#     environmentFile = /etc/acb/issue.env;  # ACB_ISSUE_TOKEN 等，0600
#   };
#
# 这个模块只负责起进程和建库目录；issue 库与 ACB 的 state_dir 分开，
# 备份/恢复见 docs/guide/issue-service.md。
{ config, lib, pkgs, ... }:
let
  cfg = config.services.acb-issue-service;
in
{
  options.services.acb-issue-service = {
    enable = lib.mkEnableOption "ACB issue service (SQLite + HTTP API)";
    package = lib.mkOption {
      type = lib.types.package;
      description = "包含 acb-issue-service 的包";
    };
    listen = lib.mkOption {
      type = lib.types.str;
      default = "127.0.0.1:7800";
      description = "HTTP 监听地址；默认只监听本机";
    };
    adminPrincipal = lib.mkOption {
      type = lib.types.str;
      default = "admin";
      description = "超时升级/自动关闭时的管理员 principal";
    };
    environmentFile = lib.mkOption {
      type = lib.types.nullOr lib.types.path;
      default = null;
      description = "可选 EnvironmentFile（ACB_ISSUE_TOKEN 等），由 systemd 以 root 读取";
    };
  };

  config = lib.mkIf cfg.enable {
    systemd.services.acb-issue-service = {
      description = "ACB issue service (SQLite + HTTP API)";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];
      unitConfig = {
        StartLimitIntervalSec = 60;
        StartLimitBurst = 5;
      };
      environment = {
        ACB_ISSUE_DB = "/var/lib/acb-issue/issues.db";
        ACB_ISSUE_LISTEN = cfg.listen;
        ACB_ISSUE_ADMIN_PRINCIPAL = cfg.adminPrincipal;
      };
      serviceConfig = {
        Type = "simple";
        ExecStart = "${cfg.package}/bin/acb-issue-service serve";
        WorkingDirectory = "/var/lib/acb-issue";
        StateDirectory = "acb-issue";
        StateDirectoryMode = "0750";
        DynamicUser = true;
        Restart = "on-failure";
        RestartSec = 5;
        TimeoutStopSec = 15;
      } // lib.optionalAttrs (cfg.environmentFile != null) {
        EnvironmentFile = cfg.environmentFile;
      };
    };
  };
}
