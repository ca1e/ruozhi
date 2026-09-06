#!/bin/sh
# Build target/release/ruozhi.app — a minimal macOS app bundle.
# The bundle gives ruozhi its own TCC identity (NSMicrophoneUsageDescription),
# so the microphone permission prompt names ruozhi instead of the terminal.
set -e
cd "$(dirname "$0")/.."
cargo build --release
APP="target/release/ruozhi.app"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/ruozhi "$APP/Contents/MacOS/ruozhi"
cp assets/ruozhi.icns "$APP/Contents/Resources/ruozhi.icns"
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>ruozhi</string>
    <key>CFBundleDisplayName</key><string>ruozhi</string>
    <key>CFBundleIdentifier</key><string>local.ruozhi.app</string>
    <key>CFBundleExecutable</key><string>ruozhi</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleInfoDictionaryVersion</key><string>6.0</string>
    <key>CFBundleShortVersionString</key><string>0.1.0</string>
    <key>CFBundleVersion</key><string>0.1.0</string>
    <key>LSMinimumSystemVersion</key><string>12.0</string>
    <key>CFBundleIconFile</key><string>ruozhi</string>
    <!-- menu-bar app: no Dock icon, the tray icon is the resident entry -->
    <key>LSUIElement</key><true/>
    <key>NSMicrophoneUsageDescription</key><string>ruozhi 需要使用麦克风与 小智 进行语音对话</string>
</dict>
</plist>
PLIST
codesign --force --sign - "$APP" 2>/dev/null || true
echo "built $APP"
echo "run:  open $APP          (GUI, 首次运行会弹出麦克风授权)"
echo "或者: $APP/Contents/MacOS/ruozhi   (终端里跑，日志可见)"
