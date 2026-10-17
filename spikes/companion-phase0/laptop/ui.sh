#!/usr/bin/env bash
# Spike helper: tap a visible UI element on the POCO by its text or content-desc, via uiautomator + input tap.
#   bash ui.sh notif "Mic on"     open the notification shade, tap the button
#   bash ui.sh qs "Spike mic"     open Quick Settings, tap the tile
#   bash ui.sh dump               list visible texts
set -euo pipefail
export MSYS_NO_PATHCONV=1
A="$LOCALAPPDATA/Microsoft/WinGet/Packages/Genymobile.scrcpy_Microsoft.Winget.Source_8wekyb3d8bbwe/scrcpy-win64-v4.1/adb.exe"
S="${POCO:-adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp}"
where="$1"; label="${2:-}"
"$A" -s "$S" shell input keyevent WAKEUP
case "$where" in
  notif) "$A" -s "$S" shell cmd statusbar expand-notifications ;;
  qs) "$A" -s "$S" shell cmd statusbar expand-settings ;;
esac
sleep 1.5
xml=$("$A" -s "$S" shell 'uiautomator dump /sdcard/tplspike-ui.xml >/dev/null 2>&1; cat /sdcard/tplspike-ui.xml; rm -f /sdcard/tplspike-ui.xml')
if [ "$where" = dump ] || [ -z "$label" ]; then
  echo "$xml" | tr '>' '\n' | grep -oE '(text|content-desc)="[^"]+"' | sort -u
  exit 0
fi
node_line=$(echo "$xml" | tr '>' '\n' | grep -E "(text|content-desc)=\"$label\"" | head -1 || true)
if [ -z "$node_line" ]; then echo "NOT FOUND: $label"; echo "$xml" | tr '>' '\n' | grep -oE '(text|content-desc)="[^"]+"' | sort -u | head -40; exit 1; fi
b=$(echo "$node_line" | grep -oE 'bounds="\[[0-9]+,[0-9]+\]\[[0-9]+,[0-9]+\]"' | grep -oE '[0-9]+' | tr '\n' ' ')
read -r x1 y1 x2 y2 <<< "$b"
x=$(( (x1 + x2) / 2 )); y=$(( (y1 + y2) / 2 ))
"$A" -s "$S" shell input tap "$x" "$y"
echo "tapped '$label' at $x,$y $(date +%T)"
