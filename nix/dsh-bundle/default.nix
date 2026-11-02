{
  lib,
  pkgs,
  # 每个 ACB 会话生成一个 dsh agent preset：preset 名 + 本地 MCP 端口。
  # nix-config 用 `.override { sessions = ...; }` 注入部署侧列表。
  sessions ? [ ],
}:
let
  rowTemplate = builtins.readFile ./preset-row.yml.tmpl;
  rows = lib.concatMapStringsSep "\n" (
    session:
    lib.replaceStrings
      [ "@PRESET@" "@PORT@" ]
      [ session.preset (toString session.port) ]
      rowTemplate
  ) sessions;
  patch = lib.replaceStrings
    [ "@ROWS@" ]
    [ rows ]
    (builtins.readFile ./presets.patch.yml.tmpl);
in
pkgs.runCommand "acb-presets-0.1.0" { } ''
  pkg=$out/lib/node_modules/acb-presets
  mkdir -p $out/nix-support "$pkg/presets"
  cp ${pkgs.writeText "acb-presets.patch.yml" patch} "$pkg/presets/acb.patch.yml"
  cp ${./package.json} "$pkg/package.json"
  sed "s|@PKG@|$pkg|" ${./dsh-bundles.json.tmpl} > $out/nix-support/dsh-bundles.json
''
// {
  passthru = {
    dshBundle = true;
    dshBundleHelper = "buildDshBundle";
    runtimeDeps = [ ];
  };
  meta = {
    description = "ACB agent presets: one dsh preset + MCP client per ACB session";
    license = lib.licenses.mit;
    platforms = lib.platforms.unix;
  };
}
