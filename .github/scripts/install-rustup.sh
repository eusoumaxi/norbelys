#!/usr/bin/env bash
# Container fallback only: execute an archived rustup binary after comparing an immutable
# SHA-256 recorded from Rust's official 1.29.0 release. Toolchains are selected separately.
set -euo pipefail
case "$(uname -m)" in
  x86_64)
    target=x86_64-unknown-linux-gnu
    expected=4acc9acc76d5079515b46346a485974457b5a79893cfb01112423c89aeb5aa10
    ;;
  aarch64)
    target=aarch64-unknown-linux-gnu
    expected=9732d6c5e2a098d3521fca8145d826ae0aaa067ef2385ead08e6feac88fa5792
    ;;
  *) echo 'No reviewed rustup checksum for this container architecture.' >&2; exit 1 ;;
esac
temporary=$(mktemp -d)
trap 'rm -rf "$temporary"' EXIT
curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 --retry 3 \
  "https://static.rust-lang.org/rustup/archive/1.29.0/$target/rustup-init" \
  --output "$temporary/rustup-init"
printf '%s  %s\n' "$expected" "$temporary/rustup-init" | sha256sum --check --strict
chmod 700 "$temporary/rustup-init"
"$temporary/rustup-init" --no-modify-path --default-toolchain none --profile minimal -y
