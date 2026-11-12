// Renders the README screenshots from the real UI files (apps/desktop/ui) with a stubbed backend, so they can be
// rebuilt after any UI change and contain no real device names, serials or addresses.
//
//   node docs/screenshots/render.mjs        writes docs/assets/{hero,flyout,page-*}.png
//
// Uses Playwright's bundled Chromium, headless (npx playwright install chromium), never an installed browser.
// CHROME=<path to chrome> picks a specific one; SHOTS_OUT=<dir> writes the images there instead of docs/assets.
// The committed images were rendered on Windows; elsewhere Segoe UI is missing, so the text looks different.
import { cpSync, readFileSync, writeFileSync, mkdtempSync, readdirSync, existsSync, rmSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { dirname, join } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath, pathToFileURL } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const ui = join(here, "../../apps/desktop/ui");
const assets = join(here, "../assets");
const out = process.env.SHOTS_OUT || assets;
const work = mkdtempSync(join(tmpdir(), "sidemic-shots-"));
cpSync(ui, work, { recursive: true });

function findChromium() {
  if (process.env.CHROME) return process.env.CHROME;
  const home = process.env.HOME || "";
  const roots = [
    process.env.PLAYWRIGHT_BROWSERS_PATH,
    join(process.env.LOCALAPPDATA || "", "ms-playwright"), // Windows
    join(home, ".cache", "ms-playwright"), // Linux
    join(home, "Library", "Caches", "ms-playwright"), // macOS
  ].filter(Boolean);
  const exes = [
    ["chrome-win64", "chrome.exe"],
    ["chrome-linux64", "chrome"],
    ["chrome-linux", "chrome"],
    ["chrome-mac", "Chromium.app", "Contents", "MacOS", "Chromium"],
  ];
  for (const root of roots) {
    if (!existsSync(root)) continue;
    for (const d of readdirSync(root).filter((d) => /^chromium-\d+$/.test(d)).sort().reverse()) {
      for (const parts of exes) {
        const exe = join(root, d, ...parts);
        if (existsSync(exe)) return exe;
      }
    }
  }
  throw new Error("Playwright Chromium not found: run `npx playwright install chromium`, or set CHROME");
}
const chrome = findChromium();

// Windows' default blue accent (AccentPalette: Light3, Light2, Light1, Base, Dark1, Dark2, Dark3), dark theme.
const accent = { dark: true, palette: ["#99ebff", "#4cc2ff", "#0091f8", "#0078d4", "#0067c0", "#003e92", "#001a68"] };
// A made-up lock screen (dusk over hills) standing in for the phone's own wallpaper, which the app shows.
const WALL = "data:image/svg+xml;base64," + Buffer.from(`<svg xmlns="http://www.w3.org/2000/svg" width="90" height="190" viewBox="0 0 90 190">
<defs><linearGradient id="s" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#1b2a4a"/><stop offset=".55" stop-color="#6a5a8e"/><stop offset="1" stop-color="#e59a6b"/></linearGradient></defs>
<rect width="90" height="190" fill="url(#s)"/><circle cx="62" cy="96" r="9" fill="#ffd6a0" opacity=".9"/>
<path d="M0 128 Q22 108 45 122 T90 114 V190 H0Z" fill="#2c2f4f"/><path d="M0 150 Q30 132 58 146 T90 140 V190 H0Z" fill="#171a30"/></svg>`).toString("base64");
const now = new Date("2026-11-11T20:42:00+08:00").getTime();
const min = 60 * 1000;
const state = {
  connected: true, link_kind: "wifi", link_text: "Wi-Fi, wireless debugging", hint: "", hint_long: "", phone_warning: null,
  other_phones: [], phones: [{ id: "PHONE1", label: "Galaxy S24 Ultra", ready: true, active: true, chosen: true }],
  auto_switch: false, aec_delay_ms: 0, aec_fallback_silence: false,
  device: { name: "Galaxy S24 Ultra" }, battery: 82, charging: false, volume: 0.55, wallpaper: WALL, ping_ms: 6, version: "1.0.0",
  autostart: true, smart_auto: true, paused: false,
  mic: { mode: "auto", live: "phone", app: "Discord", level_db: -21, delay_ms: 118, why: "" },
  audio: { mode: "auto", on_speaker: false, available: true, playing: null, why: "", speaker: "PC speakers", speaker_short: "PC speakers" },
  banner: null,
  decisions: [
    { id: 1, at: now - 52 * min, icon: "speaker", text: "Moved YouTube Music to PC speakers", reason: "The laptop was quiet", undoable: false, corrected: false },
    { id: 2, at: now - 31 * min, icon: "speaker", text: "Handed the speakers back to the laptop", reason: "A video started playing on the laptop", undoable: false, corrected: false },
    { id: 3, at: now - 9 * min, icon: "mic", text: "Phone mic on for Discord", reason: "Discord started listening", undoable: true, corrected: false },
  ],
  rules: [
    { icon: "game", text: "Valorant: phone audio stays on the phone", key: "game:valorant" },
    { icon: "flow", text: "TikTok: phone audio stays on the phone", key: "app:tiktok" },
  ],
};
const stub = `<script>Date.now=()=>${now};window.__TAURI__={core:{invoke:async(c)=>c==="state"?${JSON.stringify(state)}:c==="accent"?${JSON.stringify(accent)}:null},event:{listen:()=>{}}};</script>`;
for (const f of ["window.html", "flyout.html"]) {
  const p = join(work, f);
  writeFileSync(p, readFileSync(p, "utf8").replace('<script src="common.js">', stub + '<script src="common.js">'));
}

// A Windows 11-style frame around the page, so the README images look like the real window.
const frame = (src, w, h, title) => `<!doctype html><html><head><style>
  html,body{margin:0;background:transparent}
  .win{width:${w}px;border-radius:8px;overflow:hidden;border:1px solid #3a3a3a;box-shadow:0 8px 32px rgba(0,0,0,.45);background:#202020}
  .bar{height:32px;display:flex;align-items:center;gap:8px;padding-left:12px;color:#fff;font:12px "Segoe UI Variable Text","Segoe UI",sans-serif;background:#202020}
  .bar img{width:16px;height:16px}.sp{flex:1}.b{width:46px;text-align:center;color:#ccc;font:10px "Segoe MDL2 Assets","Segoe Fluent Icons"}
  iframe{display:block;border:0;width:${w}px;height:${h}px}
</style></head><body><div class="win"><div class="bar"><img src="${pathToFileURL(join(assets, "icon.png"))}">${title}<span class="sp"></span><span class="b">&#xE921;</span><span class="b">&#xE922;</span><span class="b">&#xE8BB;</span></div>
<iframe src="${src}"></iframe></div></body></html>`;

function shoot(url, out, w, h) {
  // Chromium refuses to run as root (containers, CI) without --no-sandbox; it only renders local files here.
  const root = process.getuid?.() === 0 ? ["--no-sandbox"] : [];
  execFileSync(chrome, [...root, "--headless=new", "--disable-gpu", "--hide-scrollbars", "--allow-file-access-from-files",
    "--default-background-color=00000000", `--window-size=${w},${h}`, `--screenshot=${out}`, "--virtual-time-budget=4000", url],
  { windowsHide: true, stdio: "ignore" });
  console.log(out);
}

const W = 1080, H = 760;
for (const page of ["home", "microphone", "audio", "auto", "connection"]) {
  const f = join(work, `frame-${page}.html`);
  writeFileSync(f, frame(pathToFileURL(join(work, "window.html")) + `?page=${page}`, W, H, "Sidemic"));
  shoot(pathToFileURL(f).href, join(out, `page-${page}.png`), W + 2, H + 34);
}
// The flyout is 360 px wide in the app (FLYOUT_WIDTH in main.rs); headless Chromium won't make a window that narrow,
// so it is framed at its real width, top-left, and the capture is cropped to it.
const fly = join(work, "frame-flyout.html");
writeFileSync(fly, `<!doctype html><html><head><style>html,body{margin:0;background:transparent}
  iframe{display:block;border:0;width:360px;height:492px;border-radius:8px;border:1px solid #3a3a3a;background:#202020}</style></head>
  <body><iframe src="${pathToFileURL(join(work, "flyout.html"))}"></iframe></body></html>`);
shoot(pathToFileURL(fly).href, join(out, "flyout.png"), 362, 494);

// Hero: the window with the flyout over its corner, on a dark gradient.
const hero = join(work, "hero.html");
writeFileSync(hero, `<!doctype html><html><head><style>
  html,body{margin:0;width:1600px;height:900px;overflow:hidden;background:radial-gradient(120% 120% at 20% 10%,#1d3b66 0%,#0e1a2e 55%,#070c16 100%)}
  img{position:absolute}
</style></head><body>
<img src="${pathToFileURL(join(out, "page-home.png"))}" style="left:80px;top:60px">
<img src="${pathToFileURL(join(out, "flyout.png"))}" style="left:1150px;top:340px;border-radius:8px;box-shadow:0 12px 40px rgba(0,0,0,.6)">
</body></html>`);
shoot(pathToFileURL(hero).href, join(out, "hero.png"), 1600, 900);
rmSync(work, { recursive: true, force: true });
