#!/usr/bin/env bash
# Builds out/tpl-server.jar (a dex jar run by app_process on the phone). No Gradle: javac + d8.
# Needs JDK 17 and the Android SDK (platforms;android-36, build-tools;36.0.0).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SDK="${ANDROID_HOME:-${LOCALAPPDATA:-$HOME/AppData/Local}/Android/Sdk}"
PLATFORM="${TPL_PLATFORM:-android-36}"
BUILD_TOOLS="${TPL_BUILD_TOOLS:-36.0.0}"
ANDROID_JAR="$SDK/platforms/$PLATFORM/android.jar"
D8="$SDK/build-tools/$BUILD_TOOLS/d8"
[ -f "$D8.bat" ] && D8="$D8.bat"
JAVAC="${JAVA_HOME:+$JAVA_HOME/bin/}javac"

OUT="$HERE/out"
CLASSES="$OUT/classes"
rm -rf "$CLASSES"
mkdir -p "$CLASSES"

# Java 8 bytecode against the platform's android.jar; d8 then dexes it for API 31+ (Android 12+).
"$JAVAC" -encoding UTF-8 -source 8 -target 8 -Xlint:-options \
    -bootclasspath "$ANDROID_JAR" -d "$CLASSES" \
    $(find "$HERE/src" -name '*.java')

"$D8" --release --min-api 31 --lib "$ANDROID_JAR" --output "$OUT/tpl-server.jar" \
    $(find "$CLASSES" -name '*.class')

echo "built $OUT/tpl-server.jar ($(wc -c < "$OUT/tpl-server.jar") bytes)"
