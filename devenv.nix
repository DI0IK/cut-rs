{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:

{
  packages = [ pkgs.git pkgs.nodejs ];
  languages.rust.enable = true;
}
