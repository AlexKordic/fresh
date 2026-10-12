#!/bin/sh
# Regenerates fresh.scene.v1.rs, the Rust types the scene tests decode the
# scene channel's protobuf with, from src/webui/scene.proto. Needs protoc and
# protoc-gen-prost 0.5 (`cargo install protoc-gen-prost --version 0.5.0`).
set -eu
here=$(cd "$(dirname "$0")" && pwd)
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
protoc --prost_out="$out" --proto_path="$here/../../src/webui" "$here/../../src/webui/scene.proto"
cp "$out/fresh/scene/v1/fresh.scene.v1.rs" "$here/fresh.scene.v1.rs"
