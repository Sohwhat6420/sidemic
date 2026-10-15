// Shared by the flyout and the main window: talks to the Rust side and keeps the live accent and wallpaper.
const tauri = window.__TAURI__;
const invoke = (cmd, args) => tauri.core.invoke(cmd, args);
const listen = (ev, f) => tauri.event.listen(ev, (e) => f(e.payload));
const $ = (id) => document.getElementById(id);

// Windows accent palette, as WinUI maps it in dark theme: fills use Light2, accent text and links use Light3.
function applyAccent(a) {
  if (!a || !a.palette) return;
  const [light3, light2, light1, base] = a.palette;
  const r = document.documentElement.style;
  r.setProperty("--accent", a.dark ? light2 : base);
  r.setProperty("--accent-text", a.dark ? light3 : a.palette[5]);
  r.setProperty("--on-accent", a.dark ? "#000" : "#fff");
}

function applyWallpaper(url) {
  document.documentElement.style.setProperty("--wallpaper", url ? `url("${url}")` : "none");
}

// Clicks: data-act="command" runs a backend command; data-open="page" opens the main window on that page.
document.addEventListener("click", (e) => {
  const act = e.target.closest("[data-act]");
  if (act) invoke("act", { action: act.dataset.act, arg: act.dataset.arg ?? null });
  const open = e.target.closest("[data-open]");
  if (open) invoke("open_window", { page: open.dataset.open });
});

const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c]);
const icon = (name, cls = "") => `<i class="i ${cls}" style="--i:url(icons/${name}.svg)"></i>`;
const clock = (ms) => new Date(ms).toLocaleTimeString("en-GB", { hour: "2-digit", minute: "2-digit" });

// The page script defines render(state); this wires it to the backend.
async function start(render) {
  applyAccent(await invoke("accent"));
  listen("accent", applyAccent);
  let wall = null;
  const draw = (s) => {
    if (s.wallpaper !== wall) {
      wall = s.wallpaper;
      applyWallpaper(wall);
    }
    render(s);
  };
  draw(await invoke("state"));
  listen("state", draw);
}
