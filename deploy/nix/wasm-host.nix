# NixOS module：可选安装 acb-wasm-host（不默认启用）。
#
# WASM 宿主由桥按配置 [[process_providers]] 启动，本身不是常驻服务；模块只负责
# 把二进制放进系统 PATH。不启用 / 不安装就是最彻底的禁用。
#
# 用法：
#   imports = [ ./deploy/nix/wasm-host.nix ];
#   services.acb-wasm-host = {
#     enable = true;
#     package = self.packages.${system}.acb-wasm-host;
#   };
{ config, lib, ... }:
let
  cfg = config.services.acb-wasm-host;
in
{
  options.services.acb-wasm-host = {
    enable = lib.mkEnableOption "acb-wasm-host reference WASM provider host";
    package = lib.mkOption {
      type = lib.types.package;
      description = "包含 acb-wasm-host 的包（flake 的 acb-wasm-host 或自建）";
    };
  };

  config = lib.mkIf cfg.enable {
    environment.systemPackages = [ cfg.package ];
  };
}
