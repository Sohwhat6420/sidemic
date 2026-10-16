# Efficiency: how TRUE Phone Link went from ~800 MB and a busy core to ~8 MB idle

TRUE Phone Link sits in the tray all day, so it has to cost close to nothing while nothing is happening. A day
of real use showed it didn't, so I measured where the cost came from and fixed each cause. All figures are from
the same laptop (Windows 11, 20 logical cores), 16 Oct 2026.

## Results

| | Before | After |
| --- | --- | --- |
| App process, private memory | 487 MB after ~20 h, and still growing | ~8–11 MB, flat |
| App + WebView2, popup closed | ~800 MB, 7 processes | **~8 MB, 1 process** (WebView2 fully exits) |
| App + WebView2, popup open | ~800 MB | ~107 MB, 7 processes |
| WebView2 GPU process | 76 MB fresh, ~200 MB after a day | 12 MB (`--disable-gpu`) |
| Threads in the app process | 81 | 21–24 |
| CPU | ~1 core, all day (70,000 CPU-seconds in ~20 h) | ~0.03–0.05% of the machine |
| Opening the popup | instant | ~0.5 s on the first click after 2 min idle, instant otherwise |

Automatic switching is unchanged: it lives in the Rust core, which never unloads. Only the popup's web view does.

## How it was measured
- **Whole process tree:** private bytes, working set and CPU over a 30 s window, for the app and every WebView2
  child it starts.
- **One part at a time:** `true-phone-link.exe --probe <part> [times]` (`src-tauri/src/probe.rs`) calls a single
  piece of the 250 ms watcher loop thousands of times, after 20 warm-up calls, and prints wall time, CPU time and
  private memory left behind **per call**. That turns "it leaks slowly" into a number per API call.
- **Thread names** (`GetThreadDescription`) and stacks, to see which threads were burning CPU. They were Windows
  thread-pool threads, not ours, which pointed at WinRT callbacks rather than our own loops.

## What was wrong, and the fix

1. **The media check leaked and piled up subscriptions.** Each second the watcher asked Windows for
   `GlobalSystemMediaTransportControlsSessionManager` afresh (`RequestAsync`). Every call leaked ~2.8 KB *and*
   left a live manager subscribed to media events, so after a day thousands of them were firing callbacks on the
   thread pool. That was the busy core. **Fix:** request the manager once and keep it (`winrt::MediaWatch`),
   re-requesting only on error; read app ids only, not titles.
2. **The Bluetooth check was a full device scan.** `DeviceInformation::FindAllAsync` every 2 s cost 63–69 ms of
   CPU and left 2.6–7 KB behind per call. **Fix:** one `DeviceWatcher` (`winrt::BtWatch`) keeps the set of paired A2DP
   sources up to date from Added/Removed events, so checking is a set lookup at ~0 cost. A full scan remains only
   as a once-a-minute fallback if the watcher can't start.
3. **Audio devices were enumerated four times a second.** **Fix:** `wasapi::Watch` keeps the session managers for
   the virtual cable and the speaker and looks the devices up again only every 5 s, or straight away on an error.
4. **A slow Windows service could stall the whole watcher.** The media query now runs on its own thread and the
   watcher reads its latest answer, so if Windows' media service stops answering the mic and speaker logic keeps
   running, and the log says so.
5. **WebView2 was always loaded.** The popup's web view stayed alive all day for a window that is open a few
   seconds at a time. **Fix:** build the popup on the first click and destroy it after 2 minutes unused, so every
   WebView2 process exits. It is shown only once its first frame has drawn, so it never flashes blank.
6. **GPU process.** The UI is a few flat panels; `--disable-gpu` cuts the GPU process from 76 MB (growing to
   ~200 MB) to 12 MB and renders identically.
7. **Thread count.** The async runtime is capped at 2 worker threads instead of one per core (20).

## Per-call costs after the fixes

`--probe`, release build, after 20 warm-up calls:

| Part | Calls | Wall per call | CPU per call | Memory left per call |
| --- | --- | --- | --- | --- |
| `find-cable` (look the cable up afresh; now every 5 s, not every tick) | 2000 | 1.2 ms | 1.2 ms | 84 B (allocator noise) |
| `mic-users` (look-up + session list) | 2000 | 1.5 ms | 1.4 ms | 156 B |
| `sounds` (speaker sessions + peak meters) | 2000 | 2.7 ms | 0.5 ms | 129 B |
| `foreground` (foreground exe + full-screen state) | 2000 | 0.09 ms | 0.05 ms | 0 |
| `bt-scan` (the old full Bluetooth scan) | 100 | 64 ms | 69 ms | 7.2 KB |
| `bt-watch` (the new watcher lookup) | 5000 | ~0 | ~0 | 0 |

## Lessons
- Measure per call, not per hour. "Grows 20 MB an hour" is hard to act on; "2.8 KB per `RequestAsync`" names the
  line to change.
- Keep WinRT managers and watchers; don't re-request them in a loop. Each request can be a live subscription, not
  just an allocation.
- A tray app's UI should cost nothing when nobody is looking at it.
