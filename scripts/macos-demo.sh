#!/bin/sh
# Builds the demo into a signed .app and runs it. UNUserNotificationCenter refuses to work
# outside an app bundle.
#
# macOS refuses notifications for an ad-hoc signed app without prompting, and the demo then
# reports permission denied. So pass a real identity, as listed by
# `security find-identity -v -p codesigning`:
#
#     CODESIGN_IDENTITY="Developer ID Application: ..." scripts/macos-demo.sh
#
# Arguments are passed to the demo, for example `--probe`.
set -eu

cd "$(dirname "$0")/.."
cargo build --example demo

app=target/debug/ZephyrNotifyDemo.app
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
cp target/debug/examples/demo "$app/Contents/MacOS/ZephyrNotifyDemo"
cat > "$app/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>com.zephyr.notify.demo</string>
    <key>CFBundleName</key>
    <string>Zephyr Notify Demo</string>
    <key>CFBundleExecutable</key>
    <string>ZephyrNotifyDemo</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>0.1.0</string>
    <key>CFBundleVersion</key>
    <string>1</string>
</dict>
</plist>
PLIST
codesign --force --sign "${CODESIGN_IDENTITY:--}" "$app"

exec "$app/Contents/MacOS/ZephyrNotifyDemo" "$@"
