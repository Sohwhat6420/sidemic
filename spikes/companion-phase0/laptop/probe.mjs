// Laptop side of the Phase 0 spike. Finds the spike app over mDNS, holds the link, pings it every second and
// logs every gap. Node, no dependencies: node.exe already has a Private-profile inbound rule on Ethan's laptop.
//
//   node probe.mjs browse [secs]                    mDNS only: when, from where, and what the phone advertises
//   node probe.mjs link [--addr ip] [--mic] [--stream] [--hours h] [--log file]
//
// The phone is found by mDNS (_tplspike._tcp.local); --addr skips discovery. After any gap the probe reconnects
// by itself, re-resolving over mDNS when the last address stops answering.
import dgram from 'node:dgram';
import net from 'node:net';
import os from 'node:os';
import fs from 'node:fs';

const args = process.argv.slice(2);
const mode = args[0] || 'link';
const opt = (k, d) => { const i = args.indexOf(k); return i < 0 ? d : (args[i + 1] && !args[i + 1].startsWith('--') ? args[i + 1] : true); };
const LOG = opt('--log', null);
const SERVICE = '_tplspike._tcp.local';

function ts() { const d = new Date(); return d.toISOString().replace('T', ' ').slice(0, 23) + 'Z'; }
function log(msg) { const l = `${ts()} ${msg}`; console.log(l); if (LOG) fs.appendFileSync(LOG, l + '\n'); }

// ------------------------------------------------------------------ mDNS (just enough of RFC 6762/6763)

/** The real LAN adapter: --iface wins; else Wi-Fi/Ethernet by name, skipping virtual adapters (VMware, Hyper-V, WSL). */
function lanAddr() {
  if (opt('--iface', null)) return opt('--iface');
  const all = Object.entries(os.networkInterfaces())
    .filter(([name]) => !/vmware|virtual|vethernet|wsl|hyper-v|loopback|bluetooth/i.test(name))
    .flatMap(([name, list]) => list.filter(a => a.family === 'IPv4' && !a.internal).map(a => ({ name, a: a.address })));
  const pick = all.find(x => /wi-?fi|wlan/i.test(x.name)) || all.find(x => /ethernet/i.test(x.name)) || all[0];
  return pick ? pick.a : '0.0.0.0';
}

function encName(n) {
  const parts = n.split('.').filter(Boolean);
  const bufs = parts.map(p => Buffer.concat([Buffer.from([p.length]), Buffer.from(p)]));
  return Buffer.concat([...bufs, Buffer.from([0])]);
}

function query() {
  const h = Buffer.alloc(12); h.writeUInt16BE(1, 4);
  const q = Buffer.alloc(4); q.writeUInt16BE(12, 0); q.writeUInt16BE(1, 2);
  return Buffer.concat([h, encName(SERVICE), q]);
}

function readName(buf, off) {
  const labels = []; let jumped = false; let end = off; let guard = 0;
  while (guard++ < 64) {
    const len = buf[off];
    if (len === 0) { if (!jumped) end = off + 1; break; }
    if ((len & 0xc0) === 0xc0) { if (!jumped) end = off + 2; off = ((len & 0x3f) << 8) | buf[off + 1]; jumped = true; continue; }
    labels.push(buf.toString('utf8', off + 1, off + 1 + len)); off += 1 + len;
  }
  return [labels.join('.'), end];
}

function parse(buf) {
  const qd = buf.readUInt16BE(4), an = buf.readUInt16BE(6), ns = buf.readUInt16BE(8), ar = buf.readUInt16BE(10);
  let off = 12;
  for (let i = 0; i < qd; i++) { off = readName(buf, off)[1] + 4; }
  const recs = [];
  for (let i = 0; i < an + ns + ar; i++) {
    const [name, o] = readName(buf, off); off = o;
    const type = buf.readUInt16BE(off), ttl = buf.readUInt32BE(off + 4), len = buf.readUInt16BE(off + 8);
    const d = off + 10; off = d + len;
    const r = { name, type, ttl };
    if (type === 12) r.ptr = readName(buf, d)[0];
    else if (type === 33) { r.port = buf.readUInt16BE(d + 4); r.target = readName(buf, d + 6)[0]; }
    else if (type === 1) r.a = [...buf.subarray(d, d + 4)].join('.');
    else if (type === 16) { r.txt = []; let p = d; while (p < d + len) { const l = buf[p]; r.txt.push(buf.toString('utf8', p + 1, p + 1 + l)); p += 1 + l; } }
    recs.push(r);
  }
  return recs;
}

/** Calls onFound({name, addr, port, txt, from}) for every answer about our service type. */
function mdns(onFound) {
  const iface = lanAddr();
  const s = dgram.createSocket({ type: 'udp4', reuseAddr: true });
  s.on('error', e => log(`mdns socket error ${e.message}`));
  s.on('message', (msg, rinfo) => {
    let recs; try { recs = parse(msg); } catch { return; }
    const ptr = recs.find(r => r.type === 12 && r.name.toLowerCase() === SERVICE.toLowerCase());
    if (!ptr) return;
    const srv = recs.find(r => r.type === 33 && r.name === ptr.ptr);
    const a = srv && recs.find(r => r.type === 1 && r.name === srv.target);
    const txt = recs.find(r => r.type === 16 && r.name === ptr.ptr);
    onFound({ name: ptr.ptr, addr: a ? a.a : rinfo.address, port: srv ? srv.port : 27199, txt: txt ? txt.txt.join(' ') : '', from: rinfo.address, ttl: ptr.ttl });
  });
  s.bind(5353, () => {
    try { s.addMembership('224.0.0.251', iface); s.setMulticastInterface(iface); } catch (e) { log(`mdns join failed ${e.message}`); }
    log(`mdns bound 5353 on ${iface}`);
  });
  const ask = () => s.send(query(), 5353, '224.0.0.251');
  return { ask, close: () => s.close() };
}

// ------------------------------------------------------------------ modes

if (mode === 'browse') {
  const secs = Number(args[1] || 20); const t0 = Date.now(); let first = null;
  const m = mdns(f => { if (!first) first = Date.now() - t0; log(`FOUND ${f.name} ${f.addr}:${f.port} txt="${f.txt}" from=${f.from} ttl=${f.ttl} t+${Date.now() - t0}ms`); });
  setTimeout(m.ask, 300); const iv = setInterval(m.ask, 2000);
  setTimeout(() => { clearInterval(iv); m.close(); log(`browse done: first answer ${first === null ? 'NEVER' : first + ' ms'}`); process.exit(first === null ? 1 : 0); }, secs * 1000);
} else if (mode === 'cmd') {
  // node probe.mjs cmd <ip> <secs> "line" "line" ...   sends each line, prints every reply for <secs>
  const [ip, secs, ...lines] = args.slice(1);
  const s = net.connect({ host: ip, port: 27199 }, () => { for (const l of lines) s.write(l + '\n'); });
  let buf = '';
  s.on('data', d => { buf += d; let i; while ((i = buf.indexOf('\n')) >= 0) { const l = buf.slice(0, i); buf = buf.slice(i + 1); if (!l.startsWith('a ')) log(`<< ${l}`); } });
  s.on('error', e => { log(`error ${e.message}`); process.exit(1); });
  setTimeout(() => { s.end('bye\n'); setTimeout(() => process.exit(0), 300); }, Number(secs) * 1000);
} else {
  link();
}

function link() {
  const hours = Number(opt('--hours', 24)); const wantMic = !!opt('--mic', false); const stream = !!opt('--stream', false);
  let addr = opt('--addr', null); let port = 27199;
  let sock = null, up = false, downSince = Date.now(), seq = 0, pending = new Map();
  let minute = { ok: 0, fail: 0, rtts: [] }, gaps = [], audio = { frames: 0, lost: 0, last: -1 };
  let mdnsLast = null;
  const m = mdns(f => { if (f.addr !== addr) log(`mdns ${f.name} at ${f.addr}:${f.port} (was ${addr})`); mdnsLast = f; });
  log(`link start addr=${addr} mic=${wantMic} stream=${stream} hours=${hours}`);

  function connect() {
    if (sock) return;
    if (!addr && mdnsLast) addr = mdnsLast.addr;
    if (!addr) { m.ask(); return; }
    const s = net.connect({ host: addr, port, timeout: 3000 });
    sock = s; let buf = '';
    s.setNoDelay(true);
    s.on('connect', () => { s.setTimeout(0); log(`TCP connected ${addr}:${port}`); if (wantMic) s.write(`mic on${stream ? ' stream' : ''}\n`); s.write('status\n'); });
    s.on('data', d => {
      buf += d.toString('utf8'); let i;
      while ((i = buf.indexOf('\n')) >= 0) { const l = buf.slice(0, i); buf = buf.slice(i + 1); line(l); }
    });
    const drop = why => { if (sock !== s) return; sock = null; s.destroy(); markDown(why); };
    s.on('timeout', () => drop('connect-timeout')); s.on('error', e => drop(e.code || e.message)); s.on('close', () => drop('closed'));
  }

  function line(l) {
    if (l.startsWith('pong ')) {
      const n = Number(l.split(' ')[1]); const t = pending.get(n); pending.delete(n);
      if (t !== undefined) { const rtt = Date.now() - t; minute.ok++; minute.rtts.push(rtt); failRun = 0; markUp(l); }
    } else if (l.startsWith('a ')) {
      const n = Number(l.split(' ')[1]); audio.frames++;
      if (audio.last >= 0 && n > audio.last + 1) audio.lost += n - audio.last - 1;
      audio.last = n;
    } else if (l.startsWith('mic stats')) {
      if (/ s=([1-5]|\d*0) /.test(l)) log(l);
    } else log(`<< ${l}`);
  }

  function markUp(pongLine) { if (!up) { up = true; const gap = Date.now() - downSince; gaps.push(gap); log(`LINK UP after ${(gap / 1000).toFixed(1)} s down | ${pongLine}`); } }
  function markDown(why) {
    if (up) { up = false; downSince = Date.now(); log(`LINK DOWN (${why})`); }
    audio.last = -1;
  }

  setInterval(() => {
    // a ping unanswered for 2 s counts as a failure; 5 straight failures drop the socket and reconnect
    const now = Date.now();
    for (const [n, t] of pending) if (now - t > 2000) {
      pending.delete(n); minute.fail++;
      if (++failRun === 2 && up) { up = false; downSince = t; log(`LINK DOWN (ping ${n} unanswered for 2 s)`); }
      if (failRun >= 5 && sock) { log('5 pings unanswered: reconnecting'); const s = sock; sock = null; s.destroy(); markDown('ping-timeout'); pending.clear(); }
    }
    if (!sock) { if (!up && now - downSince > 10000 && mdnsLast && mdnsLast.addr !== addr) { addr = mdnsLast.addr; } if (!up && now - downSince > 30000) m.ask(); connect(); return; }
    if (sock.readyState === 'open') { seq++; pending.set(seq, now); sock.write(`ping ${seq}\n`); }
  }, 1000);
  let failRun = 0;

  setInterval(() => {
    const r = minute.rtts.sort((a, b) => a - b); const p = q => r.length ? r[Math.min(r.length - 1, Math.floor(q * r.length))] : -1;
    log(`min ok=${minute.ok} fail=${minute.fail} rtt p50=${p(0.5)} p95=${p(0.95)} max=${p(1)} up=${up}${wantMic ? ` audioFrames=${audio.frames} audioLost=${audio.lost}` : ''}`);
    minute = { ok: 0, fail: 0, rtts: [] };
  }, 60000);

  setInterval(() => m.ask(), 60000);
  setTimeout(() => {
    const big = gaps.filter(g => g > 60000).length;
    log(`DONE gaps=${gaps.length - 1} over60s=${big} longest=${(Math.max(0, ...gaps.slice(1)) / 1000).toFixed(1)}s audioLost=${audio.lost}`);
    if (sock) sock.write(wantMic ? 'mic off\nbye\n' : 'bye\n');
    setTimeout(() => process.exit(0), 500);
  }, hours * 3600 * 1000);
}
