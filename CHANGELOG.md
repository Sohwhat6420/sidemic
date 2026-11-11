# Changelog

All notable changes to Sidemic. Dates are in Singapore time. Sidemic is pre-release: no signed installer yet.

## [Unreleased]

Planned (see the README's Roadmap): a home-screen control panel on the phone, an optional companion app, a
Bluetooth-tethering fallback, reverse audio, and a signed installer.

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

[Unreleased]: https://github.com/Sohwhat6420/sidemic/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/Sohwhat6420/sidemic/releases/tag/v0.1.0
