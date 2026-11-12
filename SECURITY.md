# Security

## Reporting a problem

Please report security problems privately through GitHub:
[**Report a vulnerability**](https://github.com/Sohwhat6420/sidemic/security/advisories/new) (the repository's
Security tab). Don't open a public issue for them.

Say what you found, which version (the About page in the app shows it), and how to reproduce it. As with any
report, **leave out pairing codes, adb serials, IP addresses and logs that name your devices**.

This is a one-person project, so there is no fixed response time; expect an acknowledgement within a week or so.

## Supported versions

Only the newest release gets fixes. Sidemic doesn't update itself yet, so install the new version over the old
one; your settings carry over.

## What Sidemic can reach, and what it trusts

Sidemic works by turning on Android's **wireless debugging**, which is a powerful thing to leave on. Knowing what
that means is the best protection.

- **Wireless debugging gives any paired PC a shell on the phone.** Pair only PCs you own, use it only on networks
  you trust (home Wi-Fi, not a café or a school network), and remove old pairings in Developer options →
  Wireless debugging → Paired devices. This is how Android's own debugging works; Sidemic adds nothing to it.
- **The phone server runs as the shell user**, started with `app_process` the way scrcpy is. It lives in
  `/data/local/tmp/tpl` and stops when the phone restarts. It has the permissions the shell user has, which
  include recording audio and reading what's playing; it has no root.
- **The phone server's two sockets (`tpl_mic`, `tpl_ctl`) don't authenticate the other end yet.** adb forwards
  them to two ports on the PC's loopback interface (27183 and 27184 by default). The phone starts its mic for
  whoever connects to the mic socket, so while the phone server is running, **any program on the PC, run by any
  user of that PC, can connect to the mic port and record from the phone**, and can send the control socket
  play/pause, ducking and volume commands. A program that can do that is already running on your PC, where it
  could use the laptop's own mic, so this matters most on PCs other people also use. Whether other **apps on the
  phone** can reach these sockets depends on the phone's SELinux policy and hasn't been tested. A per-session
  secret on both sockets is planned. Until then, on a PC other people use, turn off wireless debugging on the
  phone when you aren't using Sidemic: that drops the adb link, so the two ports no longer reach the phone. (Pause
  doesn't: it only switches the mic and audio back to the laptop.)
- **Nothing leaves your network.** Sidemic has no account, no telemetry and no update check. Audio and control
  messages go between the PC and the phone over the adb connection only. The logs in
  `%APPDATA%\com.truephonelink.desktop\logs\` stay on the PC and contain your phone's name and adb serial, so
  check them before you share one.
- **The installer is not code-signed yet.** Check the download against the SHA-256 on the release page before
  you run it (in PowerShell: `Get-FileHash .\Sidemic_<version>_x64-setup.exe`).

The optional companion app on the roadmap is designed to replace adb with a paired, end-to-end encrypted link;
its wire format is in [docs/link-protocol.md](docs/link-protocol.md). It isn't implemented yet.
