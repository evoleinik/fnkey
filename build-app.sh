#!/bin/bash
set -e

APP_NAME="FnKey"
BUNDLE_DIR="$APP_NAME.app"

# Add Rust toolchain to PATH
export PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH"

echo "Building fnkey..."
cargo build --release

echo "Creating app bundle..."
rm -rf "$BUNDLE_DIR"
mkdir -p "$BUNDLE_DIR/Contents/MacOS"
mkdir -p "$BUNDLE_DIR/Contents/Resources"

cp target/release/fnkey "$BUNDLE_DIR/Contents/MacOS/"
cp Info.plist "$BUNDLE_DIR/Contents/"

echo "Signing app..."
codesign --force --deep --sign "FnKey Dev" "$BUNDLE_DIR"

echo "Done! App bundle created at: $BUNDLE_DIR"
echo ""
echo "To install:"
echo "  cp -r $BUNDLE_DIR /Applications/"
echo ""
echo "To run:"
echo "  open /Applications/$BUNDLE_DIR"
