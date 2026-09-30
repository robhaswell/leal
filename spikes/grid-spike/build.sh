#!/bin/bash
# Generate the Xcode project and build a Release GridSpike.app into ./build.
set -euo pipefail
cd "$(dirname "$0")"
xcodegen generate --quiet
xcodebuild -project GridSpike.xcodeproj -scheme GridSpike -configuration Release \
  -derivedDataPath build/dd -quiet build
rm -rf build/GridSpike.app
cp -R build/dd/Build/Products/Release/GridSpike.app build/
echo "built $(pwd)/build/GridSpike.app"
