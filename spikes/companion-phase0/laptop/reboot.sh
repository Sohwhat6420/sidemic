#!/usr/bin/env bash
# Reboots the POCO and times what comes back with no taps: adb (via the test rig), the spike's mDNS advert and its TCP
# port, then prints the spike's own boot log.   bash reboot.sh <n>
set -uo pipefail
export MSYS_NO_PATHCONV=1
A="$LOCALAPPDATA/Microsoft/WinGet/Packages/Genymobile.scrcpy_Microsoft.Winget.Source_8wekyb3d8bbwe/scrcpy-win64-v4.1/adb.exe"
S="${POCO:-adb-X6PROFAKESN00001-abc123._adb-tls-connect._tcp}"
IP="${POCO_IP:-192.0.2.78}"
here="$(cygpath -m "$(cd "$(dirname "$0")" && pwd)")"
n="${1:-?}"
t0=$(date +%s); echo "REBOOT $n at $(date -u +%T)Z"
"$A" -s "$S" reboot
adb_t=""; tcp_t=""; mdns_t=""
while [ $(( $(date +%s) - t0 )) -lt 300 ]; do
  sleep 5; el=$(( $(date +%s) - t0 ))
  [ -z "$tcp_t" ] && timeout 4 node "$here/probe.mjs" cmd "$IP" 1 "ping 1" 2>/dev/null | grep -q pong && { tcp_t=$el; echo "spike TCP answers at +${el}s"; }
  [ -z "$mdns_t" ] && timeout 6 node "$here/probe.mjs" browse 4 2>/dev/null | grep -q FOUND && { mdns_t=$el; echo "spike mDNS found at +${el}s"; }
  if [ -z "$adb_t" ]; then
    "$A" mdns services 2>/dev/null | grep -q X6PROFAKESN00001 && "$A" -s "$S" shell true 2>/dev/null && { adb_t=$el; echo "adb back at +${el}s"; }
  fi
  [ -n "$adb_t" ] && [ -n "$tcp_t" ] && [ -n "$mdns_t" ] && break
done
echo "RESULT reboot $n: adb=${adb_t:-NEVER} tcp=${tcp_t:-NEVER} mdns=${mdns_t:-NEVER} (seconds after the reboot command)"
[ -n "$adb_t" ] && "$A" -s "$S" shell run-as com.example.tplspike cat files/spike.log | awk '/BOOT receiver/{b=""} {b=b $0 "\n"} END{printf "%s", b}' | grep -E 'BOOT|rig:|onStartCommand|startForeground|boot-auto s=3|nsd REG' | cut -c1-150
