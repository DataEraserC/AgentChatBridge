{
  description = "acb-bridge: 多 harness × 多聊天平台的桥";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs";
  };

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
    in
    {
      packages.${system} = rec {
        default = acb-bridge;
        # acb-launcher 与 acb-bridge 是同一个 workspace 构建产物；这个别名方便
        # deploy/nix/*.nix 的 package 选项直接引用。
        acb-launcher = acb-bridge;
        acb-bridge = pkgs.rustPlatform.buildRustPackage {
          pname = "acb-bridge";
          version = "0.1.0";
          src = ./.;

          # libgit2 通过 git2 crate 的 vendored 实现编译，需要这些系统依赖。
          nativeBuildInputs = with pkgs; [
            pkg-config
          ];
          # memory-steward 的测试直接 spawn `git` CLI（git init 等），沙箱里
          # 没有就不会出现在 PATH；这是 checkPhase 的依赖，不进运行时闭包。
          nativeCheckInputs = with pkgs; [
            git
          ];
          buildInputs = with pkgs; [
            openssl
            zlib
            libgit2
          ];

          cargoLock = {
            lockFile = ./Cargo.lock;
          };
        };

        # ACB 的 dsh preset bundle：默认没有会话，部署侧用
        # `.override { sessions = [ { preset = "..."; port = 3101; } ]; }` 注入。
        dsh-bundle = pkgs.callPackage ./nix/dsh-bundle { };

        # 参考 WASM 宿主：可选包，不进默认环境，也不链接进 acb-bridge。
        # 只有显式装配 + 配置 [[process_providers]] 才会用。
        acb-wasm-host = pkgs.rustPlatform.buildRustPackage {
          pname = "acb-wasm-host";
          version = "0.1.0";
          src = ./.;
          cargoLock = {
            lockFile = ./Cargo.lock;
          };
          buildFeatures = [ "wasm-host" ];
          cargoBuildFlags = [ "--package" "acb-wasm-host" ];
          doCheck = false;
          installPhase = ''
            mkdir -p $out/bin
            cp target/*/release/acb-wasm-host $out/bin/
          '';
        };

        # 仅供当前测试环境使用：把 Lua 目录固化在 store 路径里，
        # 不依赖手工 drop-in 或宿主环境变量。实际部署应另行决定用户、权限和配置来源。
        acb-bridge-test = pkgs.writeShellApplication {
          name = "acb-bridge-test";
          runtimeInputs = [ acb-bridge ];
          text = ''
            export ACB_LUA_DIR="${self}/examples/lua"
            exec acb-bridge "$@"
          '';
        };
      };

      apps.${system}.acb-bridge-test = {
        type = "app";
        program = "${self.packages.${system}.acb-bridge-test}/bin/acb-bridge-test";
      };

      devShells.${system}.default = pkgs.mkShell {
        nativeBuildInputs = with pkgs; [
          cargo
          rustc
          pkg-config
        ];
        buildInputs = with pkgs; [
          openssl
          zlib
          libgit2
        ];
        # 让 cargo 在这个 dev shell 里能正常解包（nix 提供干净的文件系统）。
        shellHook = ''
          export CARGO_HOME="''${CARGO_HOME:-$HOME/.cargo}"
        '';
      };
    };
}
