#!/usr/bin/env bash
# Собирает macOS-приложение "Preset Launcher.app" и zip для распространения.
#
#   bash scripts/bundle-macos.sh
#
# Результат: dist/Preset Launcher.app и dist/PresetLauncher-<версия>-macos.zip
#
# Universal-сборка (arm64 + x86_64 через lipo): собираются все macOS-цели, для которых в
# rustup установлена std (скрипт пробует добавить их сам: `rustup target add`). Если доступна
# только одна — получится сборка под неё. Нужны Xcode Command Line Tools (sips, iconutil,
# lipo, codesign, ditto). Приложение подписывается ad-hoc (без сертификата разработчика).
#
# Режим релиза: `RELEASE=1 bash scripts/bundle-macos.sh` (или флаг `--release`). Обе архитектуры
# обязательны, ошибки `rustup target add` не подавляются, в конце `lipo -verify_arch arm64 x86_64`.
# Используется в release.yml; в обычном режиме (CI, локально) доступна и сборка под одну цель.
set -euo pipefail

cd "$(dirname "$0")/.."

RELEASE="${RELEASE:-0}"
[[ "${1:-}" == "--release" ]] && RELEASE=1

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "Скрипт запускается только на macOS." >&2
  exit 1
fi

APP_NAME="Preset Launcher"
BIN_NAME="preset-launcher"
BUNDLE_ID="io.github.eg0rs.presetlauncher"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -n 1)"
if [[ -z "$VERSION" ]]; then
  echo "Не удалось прочитать версию из Cargo.toml" >&2
  exit 1
fi

DIST="dist"
APP="$DIST/$APP_NAME.app"
ZIP="$DIST/PresetLauncher-$VERSION-macos.zip"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# --- 1. Сборка под доступные архитектуры ------------------------------------------------
BINS=()
for target in aarch64-apple-darwin x86_64-apple-darwin; do
  if command -v rustup >/dev/null 2>&1; then
    if [[ "$RELEASE" == "1" ]]; then
      rustup target add "$target"
    else
      rustup target add "$target" >/dev/null 2>&1 || true
    fi
  elif [[ "$RELEASE" == "1" ]]; then
    echo "RELEASE=1 требует rustup (нужны обе цели)." >&2
    exit 1
  fi
  if [[ -d "$(rustc --print sysroot)/lib/rustlib/$target" ]]; then
    echo "==> cargo build --release --target $target"
    cargo build --release --target "$target"
    BINS+=("target/$target/release/$BIN_NAME")
  elif [[ "$RELEASE" == "1" ]]; then
    echo "RELEASE=1: нет std для $target." >&2
    exit 1
  else
    echo "==> пропускаю $target (нет std для этой цели)"
  fi
done
if [[ ${#BINS[@]} -eq 0 ]]; then
  echo "Нет ни одной подходящей цели для сборки." >&2
  exit 1
fi

rm -rf "$APP" "$ZIP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

if [[ ${#BINS[@]} -gt 1 ]]; then
  echo "==> lipo: universal binary"
  lipo -create "${BINS[@]}" -output "$APP/Contents/MacOS/$BIN_NAME"
  lipo "$APP/Contents/MacOS/$BIN_NAME" -verify_arch arm64 x86_64
else
  if [[ "$RELEASE" == "1" ]]; then
    echo "RELEASE=1: получена сборка только под одну архитектуру." >&2
    exit 1
  fi
  cp "${BINS[0]}" "$APP/Contents/MacOS/$BIN_NAME"
fi
chmod +x "$APP/Contents/MacOS/$BIN_NAME"

# --- 2. Иконка .icns ----------------------------------------------------------------------
# PNG рисует само приложение (служебная команда icon-png); берём бинарник под архитектуру хоста.
HOST_TARGET="x86_64-apple-darwin"
[[ "$(uname -m)" == "arm64" ]] && HOST_TARGET="aarch64-apple-darwin"
ICON_BIN="target/$HOST_TARGET/release/$BIN_NAME"
[[ -x "$ICON_BIN" ]] || ICON_BIN="${BINS[0]}"

echo "==> иконка"
"$ICON_BIN" icon-png 1024 "$WORK/icon_1024.png"
ICONSET="$WORK/AppIcon.iconset"
mkdir -p "$ICONSET"
make_icon() { # размер_в_пикселях имя_файла
  sips -z "$1" "$1" "$WORK/icon_1024.png" --out "$ICONSET/$2" >/dev/null
}
make_icon 16   icon_16x16.png
make_icon 32   icon_16x16@2x.png
make_icon 32   icon_32x32.png
make_icon 64   icon_32x32@2x.png
make_icon 128  icon_128x128.png
make_icon 256  icon_128x128@2x.png
make_icon 256  icon_256x256.png
make_icon 512  icon_256x256@2x.png
make_icon 512  icon_512x512.png
cp "$WORK/icon_1024.png" "$ICONSET/icon_512x512@2x.png"
iconutil -c icns "$ICONSET" -o "$APP/Contents/Resources/AppIcon.icns"

# --- 3. Info.plist --------------------------------------------------------------------------
# LSMinimumSystemVersion 12.0: определение имени процесса читает Info.plist через `plutil -extract` (macOS 12+).
# LSUIElement=false: у приложения есть значок в Dock (окно настроек + строка меню). Winit/eframe
# всё равно переключают политику активации в «обычную», поэтому скрытие из Dock не поддерживается.
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key>
  <string>$APP_NAME</string>
  <key>CFBundleDisplayName</key>
  <string>$APP_NAME</string>
  <key>CFBundleIdentifier</key>
  <string>$BUNDLE_ID</string>
  <key>CFBundleExecutable</key>
  <string>$BIN_NAME</string>
  <key>CFBundleIconFile</key>
  <string>AppIcon</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$VERSION</string>
  <key>CFBundleVersion</key>
  <string>$VERSION</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>LSUIElement</key>
  <false/>
  <key>NSHighResolutionCapable</key>
  <true/>
  <key>NSPrincipalClass</key>
  <string>NSApplication</string>
  <key>NSAppleEventsUsageDescription</key>
  <string>Preset Launcher управляет программами и системой (закрытие приложений, громкость, питание, блокировка экрана).</string>
</dict>
</plist>
PLIST

# --- 4. Подпись (ad-hoc) и zip ----------------------------------------------------------
echo "==> codesign (ad-hoc)"
codesign --force --deep --sign - "$APP"

echo "==> zip"
ditto -c -k --keepParent "$APP" "$ZIP"

echo
echo "Готово:"
echo "  $APP"
echo "  $ZIP"
