#!/bin/bash
set -e

APP_NAME="FnKey"
BUNDLE_DIR="$APP_NAME.app"

# Add Rust toolchain to PATH
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

echo "Building fnkey..."
cargo build --release

INSTALL_DIR="/Applications/$BUNDLE_DIR"

echo "Installing to $INSTALL_DIR..."
mkdir -p "$INSTALL_DIR/Contents/MacOS"
mkdir -p "$INSTALL_DIR/Contents/Resources"

cp target/release/fnkey "$INSTALL_DIR/Contents/MacOS/"
cp Info.plist "$INSTALL_DIR/Contents/"

echo "Signing app..."
codesign --force --deep --sign "FnKey Dev" "$INSTALL_DIR"

echo "Done!"
echo ""
echo "To run:"
echo "  open $INSTALL_DIR"
