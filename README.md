# TRUE Phone Link

Your Android phone as your PC's microphone, and your phone's media on your PC speakers, wirelessly, with
nothing installed on the phone. A Rust + Tauri tray app drives a small server over wireless adb.

Status: early development (private until v1).

## Layout
- `phone-server/`: the on-phone server (Java, dexed, run by `app_process` as the shell user). It captures the
  mic with `AudioRecord` and runs the voice chain **on the phone** (high-pass, presence, speech leveller with
  expander, look-ahead limiter; DeepFilterNet planned), so the PC does no audio work while you game.
  - `build.sh`: javac + d8, no Gradle. Needs JDK 17 and Android SDK platform 36 / build-tools 36.0.0.
  - Modes: `probe`, `record` (A/B WAVs), `stream` (warm server on `localabstract:tpl_mic`).

## Run the phone server by hand
```bash
bash phone-server/build.sh
adb push phone-server/out/tpl-server.jar /data/local/tmp/tpl/
adb shell CLASSPATH=/data/local/tmp/tpl/tpl-server.jar app_process / com.truephonelink.server.Main mode=probe
```

## Efficiency
Idle in the tray it is one process using about 9 MB and 0.03% CPU; the popup's web view loads on click and unloads
after 2 minutes. How that was measured and fixed (it started at ~800 MB and a busy core): `docs/efficiency.md`.

## Licence
Apache-2.0. Parts of `phone-server` are derived from scrcpy (Apache-2.0); see `NOTICE`.
