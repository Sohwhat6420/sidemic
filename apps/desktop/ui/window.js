// The main window: a Settings-style page per area.
let page = new URLSearchParams(location.search).get("page") || "home";

function show(p) {
  page = p;
  document.querySelectorAll(".item").forEach((el) => el.classList.toggle("sel", el.dataset.page === p));
  document.querySelectorAll(".page").forEach((el) => el.classList.toggle("on", el.id === "p-" + p));
}
document.addEventListener("click", (e) => {
  if (e.target.closest("select")) return;
  const nav = e.target.closest("[data-page]");
  if (nav) show(nav.dataset.page);
});
document.addEventListener("change", (e) => {
  const s = e.target.closest("[data-setting]");
  if (s) invoke("act", { action: s.dataset.setting, arg: s.value });
});
listen("page", show);
show(page);

const KIND_ICON = { mic: "mic_20_regular", speaker: "speaker_2_20_regular", phone: "phone_20_regular", game: "games_20_regular", pause: "person_voice_20_regular" };

start((s) => {
  $("win").classList.toggle("off", !s.connected);
  $("name").textContent = s.device.name || "Your phone";
  $("meta").innerHTML = s.connected
    ? `${icon("checkmark_circle_16_filled", "i16 ok")}Connected ${icon(s.link_kind === "bluetooth" ? "bluetooth_20_regular" : "wifi_1_20_regular", "i16")}` +
      (s.battery != null ? `${icon("battery_7_20_regular", "i16")}<span class="num">${s.battery}%</span>` : "")
    : esc(s.hint);

  $("pause-icon").style.setProperty("--i", `url(icons/${s.paused ? "play" : "pause"}_20_regular.svg)`);
  $("pause-title").firstChild.textContent = s.paused ? "Paused" : "Working automatically";
  $("pause-sub").textContent = s.paused
    ? "Your laptop mic is used and phone audio stays on the phone. Resume puts your settings back."
    : "Pause uses your laptop mic, keeps phone audio on the phone and stops anything moving by itself";
  $("pause-btn").textContent = s.paused ? "Resume" : "Pause";

  document.querySelectorAll('[data-setting="mic_mode"]').forEach((el) => (el.value = s.mic.mode));
  document.querySelectorAll('[data-setting="audio_mode"]').forEach((el) => (el.value = s.audio.mode));
  document.querySelectorAll('[data-setting="voice_cleaning"]').forEach((el) => (el.value = s.voice_cleaning));

  const micLine = s.mic.live === "off" ? "Nobody is using the mic" :
    `${s.mic.live === "phone" ? "Phone mic, cleaned on the phone" : "Laptop mic"}${s.mic.app ? " · " + esc(s.mic.app) + " is listening" : ""}`;
  $("home-mic").innerHTML = micLine;
  const audioLine = s.audio.playing
    ? `${esc(s.audio.playing)} is playing ${s.audio.on_speaker ? "on " + esc(s.audio.speaker) : "on the phone"}`
    : s.audio.on_speaker ? `Connected to ${esc(s.audio.speaker)}` : "Nothing playing";
  $("home-audio").innerHTML = audioLine;
  $("audio-where").innerHTML = `Automatic plays it on ${esc(s.audio.speaker || "the laptop")} while the laptop is quiet`;
  $("home-conn").textContent = s.connected ? s.link_text : s.hint;
  $("home-ping").textContent = s.connected && s.ping_ms != null ? `${s.ping_ms} ms` : "";

  $("log").innerHTML = s.decisions.length
    ? s.decisions.slice().reverse().map((d) =>
        `<div class="entry"><span class="t num">${clock(d.at)}</span>${icon(KIND_ICON[d.icon] || "flow_20_regular", "i16")}` +
        `<div>${esc(d.text)}<div class="why">${esc(d.reason)}</div></div>` +
        `<div>${d.undoable ? `<span class="link" data-act="thats_wrong" data-arg="${d.id}" title="Put it back, and remember for next time">${d.icon === "game" ? "Change" : "Undo"}</span>`
          : d.corrected ? `<span class="sub">Undone</span>` : ""}</div></div>`).join("")
    : `<div class="empty">Nothing yet today.</div>`;

  $("auto-switch").classList.toggle("on", s.smart_auto);
  $("auto-state").textContent = s.smart_auto ? "On" : "Off";
  $("rules").innerHTML = s.rules.length
    ? s.rules.map((r) => `<div class="entry" style="grid-template-columns: 20px 1fr auto">${icon(r.icon === "game" ? "games_20_regular" : "flow_20_regular", "i16")}` +
        `<div>${esc(r.text)}</div><div><span class="link" data-act="forget_rule" data-arg="${esc(r.key)}">Forget</span></div></div>`).join("")
    : `<div class="empty">Nothing yet. When you correct it, or answer a question about a game, it remembers here.</div>`;

  const note = !s.connected ? s.hint_long : s.phone_warning;
  $("conn-info").innerHTML = note ? `<div class="info">${icon("info_20_regular")}<div class="sub">${esc(note)}</div></div>` : "";
  $("conn-phones").innerHTML = (s.phones || []).filter((p) => !p.active).map((p) =>
    `<div class="card">${icon("phone_20_regular")}<div class="body">${esc(p.label)}<div class="sub">${p.ready ? "Also reachable over wireless debugging" : "Your phone, not reachable right now"}</div></div>` +
    `<div class="right">${p.ready ? `<span class="link" data-act="use_phone" data-arg="${esc(p.id)}">Use this phone</span>` : ""}</div></div>`).join("");
  $("phone-auto").classList.toggle("on", !!s.auto_switch);
  $("aec-delay").textContent = `${s.aec_delay_ms} ms`;
  $("aec-raw").style.outline = s.aec_fallback_silence ? "none" : "2px solid var(--accent)";
  $("aec-silence").style.outline = s.aec_fallback_silence ? "2px solid var(--accent)" : "none";
  $("conn-link").textContent = s.connected ? s.link_text : "Not connected";
  $("conn-ping").textContent = s.ping_ms != null ? `${s.ping_ms} ms` : "";
  $("conn-bt").textContent = s.audio.available ? `Bluetooth, to ${s.audio.speaker}` : "Bluetooth audio isn't available (is the phone paired with this laptop?)";
  $("conn-delay").textContent = s.mic.live === "phone" ? `${s.mic.delay_ms} ms` : "—";
  $("about-ver").textContent = `Version ${s.version}`;
  $("autostart").classList.toggle("on", s.autostart);
});
