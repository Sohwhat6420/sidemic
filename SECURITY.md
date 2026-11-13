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
- **The phone server's two sockets (`tpl_mic`, `tpl_ctl`) are locked with a per-session key** (since 1.0.1). adb
  forwards them to two ports on the PC's loopback interface (27183 and 27184 by default), which any program on
  the PC can connect to, so a connection has to prove itself before it is served:
  - Each time Sidemic starts the phone server it makes a fresh 32-byte random key and writes it to the server's
    standard input, through the `adb shell` session that starts it. The key is never on a command line, in an
    environment variable, in a file or in a log, and it is gone when the server stops.
  - On every connection the phone sends a random challenge. The PC answers with its own and an HMAC-SHA-256 of
    both under the key; the phone checks it in constant time and then proves the key the same way back. The key
    itself never crosses the socket, and a proof is good for one connection on one socket.
  - A connection that sends nothing, something else or a wrong proof is closed (within 3 seconds if it stays
    silent). **The phone's mic is never opened for it**, and no control command is read from it.

  What this stops: a program on the PC that just connects to those ports (run by you or by another user of the
  PC) no longer hears the phone's mic, reads what's playing or changes the volume, and it can't pass itself off
  as the phone to feed Sidemic fake audio. The same goes for an app on the phone that reaches the two sockets.

  What it doesn't stop, because it can't:
  - **A program that can use adb on your PC.** adb's own server (port 5037 on loopback) asks its callers for
    nothing, so such a program already has a shell on the phone as the shell user. From there it can record with
    its own copy of a tool like this one, and it may be able to read the key out of the running server (that
    hasn't been tested). That is adb's trust model, not something a key on Sidemic's sockets can change; the
    companion app below is the way out of it.
  - **A program running as you that reads Sidemic's memory, or one running as an administrator.** It can take
    the key from there, or record from the virtual cable or the laptop's own mic instead.
  - **Getting in the way.** A program can still open many connections to the two ports, or take the ports before
    Sidemic does (Sidemic then asks adb for free ones). Either can cost you the phone mic for a while, and
    Sidemic falls back to the laptop mic; neither gets it any audio.
  - The audio itself is not encrypted on the loopback interface. Reading other programs' loopback traffic
    normally needs administrator rights on Windows. Between the PC and the phone it travels inside wireless
    debugging's TLS connection.

  On a PC other people use, it is still sensible to turn off wireless debugging on the phone when you aren't
  using Sidemic. (Pause doesn't do that: it only switches the mic and audio back to the laptop.)
- **Nothing leaves your network.** Sidemic has no account, no telemetry and no update check. Audio and control
  messages go between the PC and the phone over the adb connection only. The logs in
  `%APPDATA%\com.truephonelink.desktop\logs\` stay on the PC and contain your phone's name and adb serial, so
  check them before you share one.
- **The installer is not code-signed yet.** Check the download against the SHA-256 on the release page before
  you run it (in PowerShell: `Get-FileHash .\Sidemic_<version>_x64-setup.exe`).

The optional companion app on the roadmap is designed to replace adb with a paired, end-to-end encrypted link;
its wire format is in [docs/link-protocol.md](docs/link-protocol.md). It isn't implemented yet.
