{
  description = "Real-time web comparison project with SSE, WebSocket, WebRTC, and WebTransport";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
        };
        setup-certs = pkgs.writeShellScriptBin "setup-certs" ''
          set -e
          echo "Setting up locally trusted certificates..."

          REPO_ROOT="$(pwd)"
          CERT_DIR="$REPO_ROOT/certs"

          # Create certs directory if it doesn't exist
          mkdir -p "$CERT_DIR"

          echo "Installing local CA..."
          ${pkgs.mkcert}/bin/mkcert -install

          echo "Generating certificate for localhost in $CERT_DIR..."
          cd "$CERT_DIR"
          ${pkgs.mkcert}/bin/mkcert localhost
          cd "$REPO_ROOT"

          echo "ertificates created successfully!"
          echo "Certificate files are in: $CERT_DIR"
          ls -la "$CERT_DIR"/*.pem 2>/dev/null || echo "  (checking certificate files...)"
        '';
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = with pkgs; [
            rustc
            cargo
            rustfmt
            clippy
            rust-analyzer
            go
            openssl
            pkg-config
            git
            mkcert
            nss.tools
          ];

          OPENSSL_DIR = "${pkgs.openssl.dev}";
          OPENSSL_LIB_DIR = "${pkgs.openssl.out}/lib";
          OPENSSL_INCLUDE_DIR = "${pkgs.openssl.dev}/include";

          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
        };

        apps.default = {
          type = "app";
          program = "${setup-certs}/bin/setup-certs";
        };

        apps.setup-certs = {
          type = "app";
          program = "${setup-certs}/bin/setup-certs";
        };
      });
}

