{
  pkgs,
  inputs,
  system,
  ...
}:
let
  rustPkgs = import inputs.nixpkgs {
    inherit system;
    overlays = [ inputs.rust-overlay.overlays.default ];
  };

  rustToolchain = rustPkgs.rust-bin.stable.latest.default.override {
    extensions = [
      "rust-src"
      "rust-analyzer"
    ];
  };
in
pkgs.mkShell {
  packages = with pkgs; [
    nixd
    nixfmt
    typos-lsp
    tombi

    rustToolchain

    libxkbcommon
    wayland
  ];
}
