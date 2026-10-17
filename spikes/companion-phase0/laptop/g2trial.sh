#!/usr/bin/env bash
# One G2 trial on the POCO: respawn the service from the background (exact alarm, fresh process), confirm the mic is
# NOT available from there, then tap the notification's "Mic on" and read whether the audio is REAL.
#   bash g2trial.sh <n>          prints one line: TRIAL n bg=<...> notif=<REAL|SILENT|NONE>
set -uo pipefail
export MSYS_NO_PATHCONV=1
A="$LOCALAPPDATA/Microsoft/WinGet/Packages/Genymobile.scrcpy_Microsoft.Winget.Source_8wekyb3d8bbwe/scrcpy-win64-v4.1/adb.exe"
S="${POCO:-adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp}"
IP="${POCO_IP:-192.0.2.78}"
n="${1:-?}"
here="$(cygpath -m "$(cd "$(dirname "$0")" && pwd)")"   # node.exe needs C:/..., not /c/...
logtail() { "$A" -s "$S" shell run-as com.example.tplspike tail -n "$1" files/spike.log; }
dump() { "$A" -s "$S" shell 'uiautomator dump /sdcard/tplspike-ui.xml >/dev/null 2>&1; cat /sdcard/tplspike-ui.xml; rm -f /sdcard/tplspike-ui.xml' | tr '>' '\n'; }
centre() { grep -oE 'bounds="\[[0-9]+,[0-9]+\]\[[0-9]+,[0-9]+\]"' | head -1 | grep -oE '[0-9]+' | tr '\n' ' '; }

"$A" -s "$S" shell input keyevent SLEEP
timeout 10 node "$here/probe.mjs" cmd "$IP" 1 "respawn 4000" >/dev/null 2>&1
sleep 16
bg=$(logtail 40 | sed -n '/respawn: alarm/,$p' | grep -E 'respawn-auto s=3|startForeground (OK|REFUSED).*boot-direct-mic' | sed -E 's/.*(startForeground [A-Z]+).*/\1/; s/.*(silenced=[a-z]+ [A-Z()a-z-]+).*/\1/' | tr '\n' ';')

"$A" -s "$S" shell input keyevent WAKEUP
"$A" -s "$S" shell cmd statusbar expand-notifications
sleep 1.5
# HyperOS 2's shade: uiautomator bounds sit ~300 px above the real screen, so the taps use positions measured from a
# screencap (spike card second, under the Artemis helper). A wrong position shows up as notif=NONE, never as a pass.
ui=$(dump)
echo "$ui" | grep -q 'text="TPL spike (test app)"' || { echo "TRIAL $n bg=[$bg] notif=NONE (notification not found)"; exit 1; }
if ! echo "$ui" | grep -q 'text="Mic on"'; then "$A" -s "$S" shell input tap 1096 790; sleep 1.5; fi
"$A" -s "$S" shell input tap 265 1046
sleep 5
res=$(logtail 15 | grep -E 'mic stats src=notif s=3' | grep -oE 'silenced=[a-z]+ [A-Z()a-z-]+' | head -1)
"$A" -s "$S" shell cmd statusbar collapse
timeout 10 node "$here/probe.mjs" cmd "$IP" 1 "mic off" >/dev/null 2>&1
echo "TRIAL $n $(date -u +%T)Z bg=[$bg] notif=[${res:-NONE}]"
