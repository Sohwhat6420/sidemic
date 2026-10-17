#!/usr/bin/env bash
# Builds out/tplspike.apk, the throwaway Phase 0 spike. No Gradle: aapt2 + javac + d8 + zipalign + apksigner,
# the same toolchain as phone-server/build.sh. Needs JDK 17 and the Android SDK (platforms;android-36, build-tools;36.0.0).
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SDK="${ANDROID_HOME:-${LOCALAPPDATA:-$HOME/AppData/Local}/Android/Sdk}"
BT="$SDK/build-tools/36.0.0"
ANDROID_JAR="$SDK/platforms/android-36/android.jar"
D8="$BT/d8"; [ -f "$D8.bat" ] && D8="$D8.bat"
APKSIGNER="$BT/apksigner"; [ -f "$APKSIGNER.bat" ] && APKSIGNER="$APKSIGNER.bat"
OUT="$HERE/out"
rm -rf "$OUT/classes" "$OUT/dex" "$OUT"/*.apk
mkdir -p "$OUT/classes" "$OUT/dex"

"$BT/aapt2" link -o "$OUT/base.apk" -I "$ANDROID_JAR" --manifest "$HERE/AndroidManifest.xml" \
    --min-sdk-version 29 --target-sdk-version 36 --debug-mode

javac -encoding UTF-8 -source 8 -target 8 -Xlint:-options -bootclasspath "$ANDROID_JAR" \
    -classpath "$BT/core-lambda-stubs.jar" -d "$OUT/classes" $(find "$HERE/src" -name '*.java')

"$D8" --debug --min-api 29 --lib "$ANDROID_JAR" --output "$OUT/dex" $(find "$OUT/classes" -name '*.class')

(cd "$OUT/dex" && "$BT/aapt" add "$OUT/base.apk" classes.dex >/dev/null)
"$BT/zipalign" -f -p 4 "$OUT/base.apk" "$OUT/aligned.apk"

# Throwaway debug key, made on first build and never committed.
KS="$OUT/spike.keystore"
[ -f "$KS" ] || keytool -genkeypair -keystore "$KS" -storepass spikespike -keypass spikespike -alias spike \
    -keyalg RSA -keysize 2048 -validity 365 -dname "CN=TPL spike throwaway" >/dev/null 2>&1
"$APKSIGNER" sign --ks "$KS" --ks-pass pass:spikespike --out "$OUT/tplspike.apk" "$OUT/aligned.apk"
rm -f "$OUT/base.apk" "$OUT/aligned.apk"
echo "built $OUT/tplspike.apk ($(wc -c < "$OUT/tplspike.apk") bytes)"
