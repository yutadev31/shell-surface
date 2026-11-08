{
  inputs.rustdev-nix.url = "github:yutadev31/rustdev-nix";

  outputs =
    inputs:
    inputs.rustdev-nix.lib.mkFlake {
      inherit inputs;
      src = ./.;

      packageAttrs = { pkgs, ... }: {
        nativeBuildInputs = [
          pkgs.pkg-config
        ];

        buildInputs = [
          pkgs.libxkbcommon
          pkgs.wayland
        ];
      };
    };
}
