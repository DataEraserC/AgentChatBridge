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

      devShells.${system} =
        let
          # inputsFrom 只合并 buildInputs/nativeBuildInputs/propagated* 并拼接
          # shellHook，不会合并任意 env；公共变量必须写进这个共享 hook，
          # 并在每个需要它的 shell 里引用。
          commonHook = ''
            export CARGO_HOME="''${CARGO_HOME:-$HOME/.cargo}"
          '';
        in
        rec {
          # 公共工具层：脚本、调试和部署都会用到的基础命令。
          base = pkgs.mkShell {
            name = "acb-base";
            nativeBuildInputs = with pkgs; [
              bash
              coreutils
              curl
              git
              jq
            ];
            shellHook = commonHook;
          };

          # Rust 组件：与旧 default 一致。
          acb = pkgs.mkShell {
            name = "acb-rust";
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
            shellHook = commonHook;
          };

          # Node 组件：仓库实际用 Node 24 跑 `node --test`；Node 22 在受限
          # 环境里 node:http 走 undici 会失败，所以固定到已验证的 24。
          harness = pkgs.mkShell {
            name = "acb-harness";
            nativeBuildInputs = with pkgs; [
              nodejs_24
              pnpm
            ];
            shellHook = commonHook;
          };

          # `nix develop` 一次只接受一个 installable；多组件组合走这个聚合层。
          all = pkgs.mkShell {
            name = "acb-all";
            inputsFrom = [
              base
              acb
              harness
            ];
          };

          # 旧入口保持只含 Rust 工具链，不额外引入 Node；需要全部组件用 .#all。
          default = acb;
        };
    };
}
