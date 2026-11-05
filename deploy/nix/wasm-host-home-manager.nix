# Home Manager module：可选安装 acb-wasm-host（不默认启用）。
#
# 与 NixOS module 相同：宿主由桥按配置启动，模块只把二进制放进用户 PATH。
#
# 用法：
#   imports = [ ./deploy/nix/wasm-host-home-manager.nix ];
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
    home.packages = [ cfg.package ];
  };
}
