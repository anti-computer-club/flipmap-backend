# default.nix (in same directory as flake.nix)
{ lib, rustPlatform, fetchFromGitHub }:

rustPlatform.buildRustPackage rec {
  pname = "flipmap-backend";
  version = "0.1.0";  # or whatever version
  
  src = ./.;  # Use current directory as source
  
  cargoLock = {
    lockFile = ./Cargo.lock;
  };
  
  meta = with lib; {
    description = "Flipmap Backend";
    homepage = "https://github.com/pasimko/flipmap-backend";
    license = licenses.gpl3;  # or whatever license
    # maintainers = [ maintainers.paul ];  # optional
  };
}
