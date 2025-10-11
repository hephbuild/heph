# default.nix
# This file describes a build environment with multiple tools.
{ pkgs ? import <nixpkgs> {} }:

let
  go-minimal = pkgs.runCommand "go-minimal" { buildInputs = [ pkgs.go ]; } ''
    mkdir -p $out
    cp -r ${pkgs.go}/* $out/
    rm -rf $out/pkg/tool
  '';
in
pkgs.buildEnv {
  name = "basic-build-tools";
  paths = [
    pkgs.cowsay
    go-minimal
  ];
}
