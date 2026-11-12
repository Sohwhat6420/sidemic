# Changelog

All notable changes to Sidemic. Dates are in Singapore time. The installer isn't code-signed yet; each release
lists its SHA-256.

## [Unreleased]

Planned (see the README's Roadmap): a per-session secret on the phone's mic and control sockets, a code-signed
installer, a home-screen control panel on the phone, an optional companion app, a Bluetooth-tethering fallback
and reverse audio.

## [1.0.0] - 2026-11-12

The first stable release: the 0.1.0 app with the first-run and failure paths fixed, built by CI from a clean
checkout, and documented without the pre-release caveats.

### Added
- **CI builds what the installer ships, from a clean checkout:** the phone server (javac + d8 against Android
  36), the phone's voice chain (arm64 `.so`, cargo-ndk) and the Windows NSIS installer with its SHA-256.
  The 0.1.0 installer was built by hand.
- **CI runs the voice chain's tests** on Linux, including the DeepFilterNet strength test that only runs in
  release. Smart auto and the desktop's unit tests still run on Windows.
- **Release drafts:** pushing a `v*` tag opens a draft GitHub release with the installer, its SHA-256 and this
  file's section for that version. A maintainer publishes it by hand; CI never publishes.
- A **versions check** fails CI when `Cargo.toml`, `tauri.conf.json` and this file's newest release disagree.
- **SECURITY.md:** how to report a problem privately, and what Sidemic can reach and trusts, including that the
  phone's two sockets aren't authenticated yet (see Known limits).
- CONTRIBUTING: how to run every test suite, and the release steps. The screenshot renderer runs on Linux
  and macOS too.

### Fixed
- **"adb isn't installed" can show.** adb discovery always "found" a bare `adb.exe`, so a PC without adb was
  told to tap Wireless debugging. adb installed while Sidemic runs is now found within 5 s, including winget's
  platform-tools package and Android Studio's SDK, which a tray app's start-up PATH doesn't reach.
- **A missing VB-CABLE is shown** in the flyout and on the Home page, instead of the mic silently staying off.
- **The laptop-mic fallback** uses Windows' default mic (not the first one listed, which could be Stereo Mix or
  a virtual mic), reopens it when it stops (a headset unplugged), and no longer re-scans devices every 5 ms on
  a PC with no mic.
- **A stalled phone stream** (Wi-Fi gone before adb notices) falls back to the laptop mic after 2 s instead of
  sending silence; a dropped stream is reconnected after 1 s rather than immediately.
- **The phone server survives a refused mic.** An exception while opening the mic used to end the whole server,
  which the laptop restarted into the same crash; now that client is closed and media, battery and volume keep
  working. A phone that won't hand over its wallpaper no longer stops those updates either.
- **Settings, Smart auto's memory and the phone list are saved whole or not at all**, so a crash mid-save can't
  reset learned rules or switch autostart back on.

### Changed
- The README, CONTRIBUTING and this file no longer call Sidemic pre-release. The Quick start points at the
  installer on the Releases page. The companion app moves to after 1.0; a code-signed installer stays on the
  roadmap.
- Crate descriptions say Sidemic, not TRUE Phone Link; `tpl-voice` and `tpl-desktop` can't be published to
  crates.io by accident.

### Known limits
- **The phone's mic and control sockets don't authenticate the other end.** adb forwards them to loopback ports
  on the PC, and the phone starts its mic for whoever connects, so any program on the PC can record from the
  phone while the server runs. SECURITY.md says what to do on a shared PC; a fix is planned (see Unreleased).
- Echo left after cancellation is above its target (README, Known limits). Tested on two phones and one laptop.
- The installer isn't code-signed: SmartScreen warns, and the release page gives the SHA-256 to check.

## [0.1.0] - 2026-11-11

The first tagged version: the app as it has run on the author's laptop since 27 Oct 2026, plus the fixes below.

### Added
- **Voice cleaning strength** setting (default Medium: DeepFilterNet at 18 dB) and a gentler leveller, so the
  default keeps the voice's highs.
- **A free-port fallback:** when ports 27183/27184 cannot be bound, the desktop asks adb for a free port and uses it.

### Changed
- `tpl-voice` is a normal dependency of the desktop app, not a dev-dependency.
- The licence file is plain MIT, so GitHub detects it. The two scrcpy-derived files stay Apache-2.0, as their
  headers and NOTICE say.

## Earlier work (untagged, Oct to Nov 2026)

- **11 Nov:** prepared for publishing. Device data scrubbed, screenshots re-rendered, CI added, personal
  names neutralised, MIT licence with the scrcpy files under Apache-2.0.
- **31 Oct:** echo simulation in harsher rooms; Bluetooth hand-off follow-ups.
- **26–27 Oct:** echo cancelling hardened (supervised AEC3 with backoff restarts), a tested Bluetooth state machine,
  tray phone switching, never treating an emulator as a phone, and the rename to Sidemic.
- **21 Oct:** echo cancelling for the phone mic (AEC3 against a loopback of the default speaker).
- **17 Oct:** works with more than one Android phone, mic queue sized to the link, watchdog for a stuck UI thread,
  the link protocol spec and a companion-app spike.
- **15–16 Oct:** Pause/Resume, one-click Undo, a week-long live run, and the efficiency pass from about 800 MB and
  a busy core down to about 9 MB idle.

[Unreleased]: https://github.com/Sohwhat6420/sidemic/compare/v1.0.0...HEAD
[1.0.0]: https://github.com/Sohwhat6420/sidemic/compare/v0.1.0...v1.0.0
[0.1.0]: https://github.com/Sohwhat6420/sidemic/releases/tag/v0.1.0
