// The tray flyout.
// Esc closes it, like the Windows quick settings.
document.addEventListener("keydown", (e) => e.key === "Escape" && invoke("hide_flyout"));
const BARS = 20;
$("meter").innerHTML = "<span></span>".repeat(BARS);
const bars = [...$("meter").children];

const vol = $("volume");
let dragging = false;
vol.addEventListener("input", () => {
  dragging = true;
  vol.style.setProperty("--fill", vol.value + "%");
  $("volume-val").textContent = vol.value;
  invoke("act", { action: "volume", arg: String(vol.value / 100) });
});
vol.addEventListener("change", () => (dragging = false));

start((s) => {
  $("flyout").classList.toggle("off", !s.connected);
  $("name").textContent = s.device.name || "Your phone";

  const meta = s.connected
    ? `${icon("checkmark_circle_16_filled", "i16 ok")}<span>Connected</span>` +
      `${icon(s.link_kind === "bluetooth" ? "bluetooth_20_regular" : "wifi_1_20_regular", "i16")}` +
      (s.battery != null ? `${icon("battery_7_20_regular", "i16")}<span class="num">${s.battery}%</span>` : "")
    : `<span>${esc(s.hint)}</span>`;
  $("meta").innerHTML = meta;

  const tile = (id, on, disabled) => {
    $(id).classList.toggle("on", on);
    $(id).classList.toggle("disabled", !!disabled);
  };
  tile("t-mic", s.mic.mode !== "laptop");
  tile("t-audio", s.audio.on_speaker, !s.audio.available);
  tile("t-auto", s.smart_auto);
  $("audio-label").textContent = s.audio.speaker_short ? `Phone to ${s.audio.speaker_short}` : "Phone to speaker";

  // Mic level: lit bars from -60 dBFS to 0.
  const lit = s.mic.live === "off" ? 0 : Math.round(Math.max(0, Math.min(1, (s.mic.level_db + 60) / 60)) * BARS);
  bars.forEach((b, i) => b.classList.toggle("lit", i < lit));
  $("level").textContent = s.mic.live === "off" ? "" : Math.round(s.mic.level_db);

  if (!dragging && s.volume != null) {
    vol.value = Math.round(s.volume * 100);
    vol.style.setProperty("--fill", vol.value + "%");
    $("volume-val").textContent = vol.value;
  }
  vol.disabled = !s.connected;

  // What's happening now, in plain words.
  const lines = [];
  const line = (ic, text, muted, action) =>
    `<div class="line">${icon(ic)}<div class="txt">${text}<div class="muted">${muted}</div></div>${action || ""}</div>`;
  if (s.paused) {
    lines.push(line("pause_20_regular", "Paused",
      "Your laptop mic is used, phone audio stays on the phone, and nothing moves by itself",
      `<span class="link" data-act="toggle_pause">Resume</span>`));
  } else if (s.connected && s.mic.live === "off" && !s.audio.playing) {
    lines.push(line("checkmark_circle_16_filled", "Ready, nothing to do",
      s.smart_auto
        ? "The phone mic turns on when an app listens. Phone media moves to the speaker while the laptop is quiet."
        : "The phone mic turns on when an app listens. Smart auto is off, so phone audio stays where you put it."));
  }
  if (s.mic.live !== "off") {
    const who = s.mic.app ? `${esc(s.mic.app)} is using` : "Apps are using";
    lines.push(`<div class="line">${icon("person_voice_20_regular")}<div class="txt">${who} your ${s.mic.live === "phone" ? "phone" : "laptop"} mic` +
      `<div class="muted">${s.mic.live === "phone" ? "Voice cleaned on the phone" : esc(s.mic.why || "Phone not reachable")}</div></div></div>`);
  }
  if (s.audio.playing) {
    const where = s.audio.on_speaker ? `on the ${esc(s.audio.speaker_short || "speaker")}` : "on the phone";
    lines.push(`<div class="line">${icon("video_clip_20_regular")}<div class="txt">${esc(s.audio.playing)} is playing ${where}` +
      `<div class="muted">${esc(s.audio.why || "")}</div></div>` +
      (s.audio.on_speaker ? `<span class="link" data-act="hand_back">Hand back</span>` : "") + `</div>`);
  }
  // The last thing Smart auto did, with a one-click undo, for half an hour.
  const d = s.decisions[s.decisions.length - 1];
  if (d && Date.now() - d.at < 30 * 60 * 1000) {
    lines.push(line("arrow_undo_16_regular", `${esc(d.text)} <span class="muted">at ${clock(d.at)}</span>`,
      d.corrected ? "Undone. It will remember next time." : esc(d.reason),
      d.undoable ? `<span class="link" data-act="thats_wrong" data-arg="${d.id}" title="Put it back, and remember for next time">Undo</span>` : ""));
  }
  $("status").innerHTML = lines.join("");

  $("pause").innerHTML = s.paused ? `${icon("play_20_regular", "i16")}Resume` : `${icon("pause_20_regular", "i16")}Pause`;
  $("pause").title = s.paused ? "Turn TRUE Phone Link back on, as it was" : "Use the laptop mic, keep phone audio on the phone, and stop moving things automatically";

  // A question or notice from Smart auto.
  const b = s.banner;
  $("bar").innerHTML = !b ? "" : `<div>${esc(b.text)}</div><div class="acts">` +
    b.buttons.map((x, i) => `<div class="btn ${i === 0 ? "accent" : ""}" data-act="${esc(x.action)}" data-arg="${esc(x.arg ?? "")}">${esc(x.label)}</div>`).join("") +
    `</div>`;

  $("delay").textContent = s.mic.live === "phone" ? `${s.mic.delay_ms} ms mic delay` : "";
  $("delay-dot").style.display = s.mic.live === "phone" ? "" : "none";

  // Tell the app our height so the window hugs the content.
  invoke("flyout_height", { height: document.getElementById("flyout").offsetHeight });
});
