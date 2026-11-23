{
  description = "acb-bridge: 多 harness × 多聊天平台的桥";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs";
  };

  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
      # 构建身份：git 提交短哈希优先，脏树/无 git 回落到时间戳。控制台头部
      # 显示它（/api/status 的 version 字段，来源 ACB_BUILD_VERSION），
      # 用来分辨「这台桥跑的是哪次构建」。
      buildRev = if self ? shortRev then self.shortRev
        else if self ? dirtyShortRev then self.dirtyShortRev
        else self.lastModifiedDate or "unknown";
      # 提交日期（YYYYMMDD）：跟随 git 提交，**不是构建时间**，所以同一个 commit
      # 永远得到同一个版本号，不破坏可复现性。
      #
      # 只用纯 Nix 算术：`self.lastModified` 是提交时间戳（脏树时退化成当天）。
      # 不走 `runCommand`/IFD——那会拖慢每次求值，而这里只想要一个日期串。
      buildDate =
        let
          stamp = self.lastModified or 0;
          days = stamp / 86400;
          # 民用历换算（Howard Hinnant 的 days_from_civil 逆运算）。
          z = days + 719468;
          era = z / 146097;
          doe = z - era * 146097;
          yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
          y = yoe + era * 400;
          doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
          mp = (5 * doy + 2) / 153;
          d = doy - (153 * mp + 2) / 5 + 1;
          m = if mp < 10 then mp + 3 else mp - 9;
          year = if m <= 2 then y + 1 else y;
          pad = n: if n < 10 then "0" + toString n else toString n;
          # 时分：同样按 UTC（与上面的日期换算同一时区，避免跨日错位）。
          hours = (stamp - days * 86400) / 3600;
          minutes = (stamp - days * 86400 - hours * 3600) / 60;
        in
        if stamp == 0 then
          "unknown"
        else
          "${toString year}${pad m}${pad d}-${pad hours}${pad minutes}";
      # 版本号只带提交哈希：它要短、要稳定、要能一眼和人说的对上。
      # 提交时间单独走 `ACB_COMMIT_TIME`，由 `/api/meta` 暴露成结构化字段——
      # 拼进版本串里只会让版本号变长且难比对（2026-11-20 用户裁定）。
      buildVersion = "${version}+${builtins.toString buildRev}";

      # 整个 workspace 的 Rust 构建：产出全部二进制。它本身不再是部署产物，
      # 只作为 acbl / payload 两个分发包的取材来源。
      workspace = pkgs.rustPlatform.buildRustPackage {
          pname = "acb-workspace";
          inherit version;
          src = ./.;

          # libgit2 通过 git2 crate 的 vendored 实现编译，需要这些系统依赖。
          # nixpkgs 26.11 的 cargo-setup-hook 强制 LIBGIT2_NO_VENDOR=1，而系统
          # libgit2 已升到 1.9.7、超出 git2 0.19 的 <1.9.0 约束 —— 显式恢复
          # vendored 编译，并补上 vendored libgit2 1.8 的 cmake 构建依赖。
          env.LIBGIT2_NO_VENDOR = "0";
          nativeBuildInputs = with pkgs; [
            pkg-config
            cmake
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

          # checkPhase 会以 release profile 跑 `cargo check --workspace`，某个
          # dev-dependency 会打开 kernel 的 `test-fixtures` feature，命中它内置的
          # compile_error! 守卫。该守卫是刻意为之；nix 包只负责产出二进制，
          # 测试由 `cargo test` 单独执行。
          doCheck = false;
        };

      # acbl：静态入口分发包。只含 acbl 侧内容；bridge / harnesses / drivers
      # 属于 payload，不在这里。unit 模板随包分发，由 `acbctl unit install` 渲染。
      acbl = pkgs.runCommand "acbl-${version}" { } ''
        mkdir -p $out/bin $out/share/acb/units $out/share/acb/presets
        install -m755 ${workspace}/bin/acb-launcher $out/bin/acb-launcher
        install -m755 ${workspace}/bin/acb-supervisor $out/bin/acb-supervisor
        install -m755 ${workspace}/bin/acbctl $out/bin/acbctl
        install -m644 ${./share/acb/units}/*.tmpl $out/share/acb/units/
        install -m644 ${./deploy/systemd/acb-launcher.service} $out/share/acb/units/acb-launcher.service
        # 预设清单与脚本随 acbl 分发（不随 payload）。安装后由
        # `bin/<launcher|acbctl>` 同级推导 ../share/acb/presets/presets.toml，
        # 不需要任何环境变量。脚本按安装布局（$HERE/../../../.. = prefix）
        # 找同包的 bin/acbctl，不依赖仓库相对路径。
        cp -r ${./deploy/presets}/. $out/share/acb/presets/
        chmod -R u+w $out/share/acb/presets
        patchShebangs $out/share/acb/presets
        find $out/share/acb/presets -name '*.sh' -exec chmod 755 {} +
      '';

      # payload：会随迭代换代的内容。口径与 B-12 的 validate_payload 对齐：
      # bin/acb-bridge、harnesses/、deploy/components/drivers/start-drivers.sh、acb-contract.json。
      # examples/lua 也属于 payload：unit 的 ACB_LUA_DIR 指向 profile 的
      # examples/lua，缺了这一代就启动失败（2026-11-09 升级事故）。
      acb-payload = pkgs.runCommand "acb-payload-${version}" {
        nativeBuildInputs = [ pkgs.makeWrapper ];
      } ''
        mkdir -p $out/bin $out/harnesses $out/deploy/components/drivers $out/examples
        install -m755 ${workspace}/bin/acb-bridge $out/bin/acb-bridge
        install -m755 ${workspace}/bin/acb-notifier $out/bin/acb-notifier
        cp -r ${./harnesses}/. $out/harnesses/
        cp -r ${./examples/lua} $out/examples/lua
        install -m755 ${./deploy/components/drivers/start-drivers.sh} \
          $out/deploy/components/drivers/start-drivers.sh
        install -m644 ${./deploy/ops/acb-contract.json} $out/acb-contract.json
        # 构建身份随 payload 走：桥在 /api/status 里回这个值，控制台头部显示。
        wrapProgram $out/bin/acb-bridge --set ACB_BUILD_VERSION ${buildVersion} \
          --set ACB_COMMIT_TIME ${buildDate}
      '';
    in
    {
      packages.${system} = rec {
        default = acb-bridge;
        # 完整 workspace 构建；部署请取 acbl / acb-payload。
        acb-bridge = workspace;
        # acb-launcher 与 acb-bridge 是同一个 workspace 构建产物；这个别名方便
        # deploy/nix/*.nix 的 package 选项直接引用。
        acb-launcher = workspace;
        inherit acbl acb-payload;
        # `nix build .#payload` 的简写。
        payload = acb-payload;

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
