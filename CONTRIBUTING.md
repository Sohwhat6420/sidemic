# Contributing

Thanks for looking. Sidemic is pre-release and tested on two phones and one laptop, so reports from other
phones are the most useful thing you can send.

## Reporting a phone

Open an issue with:
- the phone model, Android version and skin (One UI, HyperOS, …);
- whether the mic, media and Smart auto worked, and what the status line said when they didn't;
- anything in the Known limits section of the README that you hit.

**Never paste a pairing code, an adb serial, an IP address or a log with your device names in it.** Replace them
with placeholders first.

## Running the tests

These need no phone and no Android SDK (Windows, Rust stable):

```bash
mkdir -p phone-server/out && touch phone-server/out/tpl-server.jar phone-server/out/libtpl_voice_jni.so
cargo test -p tpl-policy -p tpl-desktop     # what CI runs on every push
```

The first line puts empty stand-ins where the Tauri build script expects the phone files; the tests don't use
them. Building the real app needs the full toolchain under "Build from source" in the README.

## Pull requests

- Keep them small and focused, with a one-line reason in the title.
- Smart auto changes need a scenario test in `crates/policy/` (events in, actions out).
- Console tools (adb, reg) start with `CREATE_NO_WINDOW`, so nothing flashes on screen; please do the same for
  any new one.
- The two scrcpy-derived files in `phone-server/` stay under Apache-2.0; everything else is MIT.
- British English in docs and UI copy.

## Good first issues

Look for the [`good first issue`](https://github.com/Sohwhat6420/sidemic/labels/good%20first%20issue) label, and
ask in the issue if anything is unclear.
