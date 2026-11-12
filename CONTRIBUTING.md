# Contributing

Thanks for looking. Sidemic has been tested on two phones and one laptop, so reports from other phones are the
most useful thing you can send.

## Reporting a phone

Open an issue with:
- the phone model, Android version and skin (One UI, HyperOS, …);
- whether the mic, media and Smart auto worked, and what the status line said when they didn't;
- anything in the Known limits section of the README that you hit.

**Never paste a pairing code, an adb serial, an IP address or a log with your device names in it.** Replace them
with placeholders first.

## Running the tests

These need no phone and no Android SDK (Rust stable). On Windows:

```bash
mkdir -p phone-server/out && touch phone-server/out/tpl-server.jar phone-server/out/libtpl_voice_jni.so
cargo test -p tpl-policy -p tpl-desktop     # Smart auto and the desktop's unit tests
```

The first line puts empty stand-ins where the Tauri build script expects the phone files; the tests don't use
them. On Linux too (the desktop crate is Windows-only; these two aren't):

```bash
cargo test -p tpl-policy -p tpl-voice
cargo test --release -p tpl-voice -- --include-ignored   # DeepFilterNet only builds its graph in release
```

CI runs all of these on every push and pull request (`.github/workflows/test.yml`), and `build.yml` builds the
phone server, the phone's voice chain and the Windows installer from a clean checkout. Building the real app
yourself needs the full toolchain under "Build from source" in the README.

## Releasing

1. Bump `version` in `apps/desktop/src-tauri/Cargo.toml` and `apps/desktop/src-tauri/tauri.conf.json`, and move
   the CHANGELOG's Unreleased notes under a new `## [x.y.z] - date` heading. CI fails if the three disagree.
2. Merge, then tag the merge commit `vx.y.z` and push the tag.
3. `build.yml` opens a **draft** release with the installer, its SHA-256 and that version's CHANGELOG section.
   Read it, try the installer, and publish it by hand.

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
