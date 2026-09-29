# schorl の開発環境 (NixOS 向け)。
#
# nixpkgs はホストの /etc/nixos/flake.lock が zeno 経由で掴んでいる rev に揃える。
# ホストの Mesa / WiVRn / monado と同じ世代の loader を使うため。
#
# README の apt 依存との対応:
#   glslang-tools -> glslang      (glslangValidator。build.rs が必須とする)
#   spirv-tools   -> spirv-tools  (spirv-val。任意)
# README に書かれていないが実際に要る物:
#   libxkbcommon  -> smithay の wayland_frontend が xkbcommon crate 経由で link する
#   vulkan-loader / openxr-loader -> ash / openxr crate が実行時に dlopen する
#   monado        -> scripts/schorl-hmdless-check.sh が monado-service を起こす
{
  description = "schorl development shell";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/ffb3c9b700e759be2ef13237c9d8f953b32a1e46";

  outputs =
    { nixpkgs, ... }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      # 実行時に dlopen される物。link 時には要らないので LD_LIBRARY_PATH で渡す。
      runtimeLibs = with pkgs; [
        vulkan-loader
        openxr-loader
        libxkbcommon
        wayland
      ];
    in
    {
      devShells.${system}.default = pkgs.mkShell {
        packages = with pkgs; [
          rustc
          cargo
          clippy
          rustfmt
          pkg-config
          glslang
          spirv-tools
          libxkbcommon
          monado
          vulkan-tools
        ];
        # Mesa の ICD はホストの /run/opengl-driver から読まれる (nixpkgs の vulkan-loader の既定)。
        LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
      };
    };
}
