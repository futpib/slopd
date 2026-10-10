#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ $# != 3 || ! "$1" =~ ^[0-9][0-9A-Za-z.+-]*$ || ! "$2" =~ ^[1-9][0-9]{0,8}$ ]]; then
    echo 'Usage: build-release.sh VERSION_NAME VERSION_CODE OUTPUT_DIRECTORY' >&2
    exit 2
fi
: "${KEYSTORE_PATH:?Set KEYSTORE_PATH to the persistent release keystore}"
: "${KEYSTORE_PASSWORD:?Set KEYSTORE_PASSWORD}"
if [[ ! -f "$KEYSTORE_PATH" ]]; then
    echo 'KEYSTORE_PATH does not point to a keystore file' >&2
    exit 2
fi
export KEYSTORE_PATH
KEYSTORE_PATH=$(realpath "$KEYSTORE_PATH")
release_output=$(realpath -m "$3")
mkdir -p "$release_output"

flutter_bin=${FLUTTER:-flutter}
"$flutter_bin" build apk --release --target lib/main.dart \
    --target-platform android-arm64,android-x64 --build-name="$1" --build-number="$2"
install -m644 build/app/outputs/flutter-apk/app-release.apk "$release_output/app-release.apk"
"$flutter_bin" build apk --release --target lib/main.dart --split-per-abi \
    --target-platform android-arm64,android-x64 --build-name="$1" --build-number="$2"
for abi in arm64-v8a x86_64; do
    install -m644 "build/app/outputs/flutter-apk/app-$abi-release.apk" "$release_output/app-$abi-release.apk"
done
(
    cd "$release_output"
    sha256sum app-release.apk app-arm64-v8a-release.apk app-x86_64-release.apk > SHA256SUMS
)
