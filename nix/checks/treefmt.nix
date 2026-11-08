{
  pkgs,
  inputs,
  flake,
  ...
}:
(inputs.treefmt-nix.lib.evalModule pkgs ../treefmt.nix).config.build.check flake
