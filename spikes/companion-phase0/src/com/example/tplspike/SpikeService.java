package com.example.tplspike;

import android.app.ActivityManager;
import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.app.WallpaperColors;
import android.app.WallpaperInfo;
import android.app.WallpaperManager;
import android.app.usage.UsageStatsManager;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.ServiceInfo;
import android.graphics.drawable.Icon;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioManager;
import android.media.AudioPlaybackConfiguration;
import android.media.AudioRecord;
import android.media.AudioRecordingConfiguration;
import android.media.MediaMetadata;
import android.media.MediaRecorder;
import android.media.session.MediaController;
import android.media.session.MediaSessionManager;
import android.media.session.PlaybackState;
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.net.RouteInfo;
import android.net.nsd.NsdManager;
import android.net.nsd.NsdServiceInfo;
import android.net.wifi.WifiManager;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.os.PowerManager;
import android.os.SystemClock;
import android.util.Base64;
import android.view.KeyEvent;

import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.InterfaceAddress;
import java.net.NetworkInterface;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.Collections;
import java.util.List;
import java.util.Locale;
import java.util.UUID;
import java.util.concurrent.CopyOnWriteArrayList;
import java.util.concurrent.LinkedBlockingDeque;
import java.util.concurrent.TimeUnit;

/**
 * The spike's LinkService stand-in. A foreground service of type connectedDevice that serves a line protocol
 * on TCP 27199, advertises _tplspike._tcp over NSD, and adds type microphone while the mic is wanted.
 * Every decision point is logged so the spike reads its answers from spike.log, not from belief.
 */
public class SpikeService extends Service {
    static final String ACT_MIC_ON = "com.example.tplspike.MIC_ON";
    static final String ACT_MIC_OFF = "com.example.tplspike.MIC_OFF";
    static final String CH = "spike";
    static final int NID = 1;
    static final int PORT = 27199;

    static volatile SpikeService instance;

    private Handler h;
    private String phoneId;
    private ServerSocket server;
    private final List<Client> clients = new CopyOnWriteArrayList<>();
    private NsdManager nsd;
    private NsdManager.RegistrationListener reg;
    private volatile boolean foreground;
    private volatile boolean micType;
    private volatile Mic mic;
    private volatile long lastPing;
    private volatile Boolean lastBlocked;
    private WifiManager.WifiLock wifiLock;
    private MediaSessionManager.OnActiveSessionsChangedListener sessionsListener;

    // ---------------------------------------------------------------- lifecycle

    @Override
    public void onCreate() {
        super.onCreate();
        L.init(this);
        instance = this;
        h = new Handler(Looper.getMainLooper());
        SharedPreferences p = getSharedPreferences("spike", MODE_PRIVATE);
        phoneId = p.getString("id", null);
        if (phoneId == null) {
            phoneId = UUID.randomUUID().toString().substring(0, 8);
            p.edit().putString("id", phoneId).apply();
        }
        NotificationChannel ch = new NotificationChannel(CH, "Spike link", NotificationManager.IMPORTANCE_LOW);
        getSystemService(NotificationManager.class).createNotificationChannel(ch);
        L.i("service onCreate id=" + phoneId + " sdk=" + Build.VERSION.SDK_INT + " " + state());
        startServer();
        registerNsd();
        watchNetworks();
        watchMedia();
        watchWallpaper();
        h.postDelayed(heartbeat, 60_000);
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        String act = intent == null ? "null(sticky-restart)" : String.valueOf(intent.getAction());
        String src = intent == null ? "sticky" : intent.getStringExtra("src");
        L.i("onStartCommand act=" + act + " src=" + src + " flags=" + flags + " " + state());
        if (!foreground) {
            if ("boot".equals(src)) {
                // Android 14+ should refuse microphone from BOOT_COMPLETED. Try it, record it, then fall back.
                if (!goForeground(true, "boot-direct-mic")) goForeground(false, "boot");
                else micType = true;
            } else {
                goForeground(false, src);
            }
        }
        if (intent != null && ACT_MIC_ON.equals(intent.getAction())) {
            micOn(src, intent.getBooleanExtra("stream", false), intent.getIntExtra("secs", 0));
        } else if (intent != null && ACT_MIC_OFF.equals(intent.getAction())) {
            micOff(src);
        } else if ("boot".equals(src)) {
            micOn("boot-auto", false, 6);
        }
        return START_STICKY;
    }

    @Override
    public void onDestroy() {
        L.i("service onDestroy");
        instance = null;
        try { if (server != null) server.close(); } catch (Exception ignored) { }
        try { if (reg != null) nsd.unregisterService(reg); } catch (Exception ignored) { }
        if (mic != null) mic.stop = true;
        super.onDestroy();
    }

    @Override
    public void onTimeout(int startId, int fgsType) {
        L.i("FGS onTimeout type=" + fgsType);
    }

    @Override
    public IBinder onBind(Intent intent) { return null; }

    // ---------------------------------------------------------------- foreground + notification

    private boolean goForeground(boolean withMic, String why) {
        int type = ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE
                | (withMic ? ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE : 0);
        try {
            startForeground(NID, buildNotif(withMic), type);
            foreground = true;
            micType = withMic;
            L.i("startForeground OK type=" + (withMic ? "connectedDevice|microphone" : "connectedDevice") + " why=" + why);
            return true;
        } catch (Throwable t) {
            L.e("startForeground REFUSED type=" + (withMic ? "connectedDevice|microphone" : "connectedDevice") + " why=" + why, t);
            return false;
        }
    }

    private Notification buildNotif(boolean micLive) {
        PendingIntent on = PendingIntent.getForegroundService(this, 1,
                new Intent(this, SpikeService.class).setAction(ACT_MIC_ON).putExtra("src", "notif"),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        PendingIntent off = PendingIntent.getService(this, 2,
                new Intent(this, SpikeService.class).setAction(ACT_MIC_OFF).putExtra("src", "notif"),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        PendingIntent open = PendingIntent.getActivity(this, 3, new Intent(this, MainActivity.class),
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        Icon icon = Icon.createWithResource(this, android.R.drawable.ic_btn_speak_now);
        Notification.Builder b = new Notification.Builder(this, CH)
                .setSmallIcon(android.R.drawable.ic_btn_speak_now)
                .setContentTitle("TPL spike (test app)")
                .setContentText((micLive ? "Mic live" : "Mic ready") + " · clients " + clients.size())
                .setOngoing(true)
                .setContentIntent(open)
                .addAction(new Notification.Action.Builder(icon, "Mic on", on).build())
                .addAction(new Notification.Action.Builder(icon, "Mic off", off).build())
                .addAction(new Notification.Action.Builder(icon, "Open", open).build());
        if (Build.VERSION.SDK_INT >= 31) b.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE);
        return b.build();
    }

    private void refreshNotif() {
        try {
            getSystemService(NotificationManager.class).notify(NID, buildNotif(mic != null));
        } catch (Throwable ignored) { }
    }

    // ---------------------------------------------------------------- mic

    synchronized void micOn(String src, boolean stream, int secs) {
        if (mic != null) {
            L.i("micOn src=" + src + " ignored: already live (src=" + mic.src + ")");
            if (stream) mic.stream = true;
            return;
        }
        boolean fgsOk = micType || goForeground(true, "mic-" + src);
        Mic m = new Mic(src, fgsOk, stream, secs);
        mic = m;
        m.start();
        refreshNotif();
    }

    synchronized void micOff(String src) {
        Mic m = mic;
        if (m == null) {
            L.i("micOff src=" + src + " (already off)");
            return;
        }
        m.stop = true;
        try { m.join(1000); } catch (InterruptedException ignored) { }
    }

    private synchronized void micEnded(Mic m) {
        if (mic == m) mic = null;
        if (micType) goForeground(false, "mic-released");
        broadcast("mic live=false src=" + m.src);
        refreshNotif();
    }

    /** Captures VOICE_RECOGNITION at 48 kHz mono and reports whether the audio is REAL, not just "started". */
    private final class Mic extends Thread {
        final String src;
        final boolean fgsOk;
        final int secs;
        volatile boolean stream;
        volatile boolean stop;

        Mic(String src, boolean fgsOk, boolean stream, int secs) {
            super("mic-" + src);
            this.src = src;
            this.fgsOk = fgsOk;
            this.stream = stream;
            this.secs = secs;
        }

        @Override
        public void run() {
            AudioRecord rec = null;
            int seq = 0;
            try {
                if (wifiLock == null) {
                    wifiLock = getSystemService(WifiManager.class)
                            .createWifiLock(WifiManager.WIFI_MODE_FULL_LOW_LATENCY, "tplspike-mic");
                }
                wifiLock.acquire();
                int min = AudioRecord.getMinBufferSize(48000, AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT);
                rec = new AudioRecord(MediaRecorder.AudioSource.VOICE_RECOGNITION, 48000,
                        AudioFormat.CHANNEL_IN_MONO, AudioFormat.ENCODING_PCM_16BIT, Math.max(min, 48000));
                rec.startRecording();
                L.i("mic START src=" + src + " fgsMicType=" + fgsOk + " recState=" + rec.getRecordingState() + " " + state());
                broadcast("mic live=true src=" + src + " fgsMicType=" + fgsOk);
                short[] buf = new short[480];
                byte[] bytes = new byte[960];
                long t0 = SystemClock.elapsedRealtime();
                int second = 0, blocks = 0, nz = 0, n = 0, peak = 0;
                double sq = 0;
                while (!stop) {
                    int r = rec.read(buf, 0, 480);
                    if (r < 0) {
                        L.i("mic read error " + r);
                        break;
                    }
                    for (int i = 0; i < r; i++) {
                        int v = buf[i];
                        int a = Math.abs(v);
                        if (a > peak) peak = a;
                        if (v != 0) nz++;
                        sq += (double) v * v;
                    }
                    n += r;
                    if (stream) {
                        for (int i = 0; i < r; i++) {
                            bytes[2 * i] = (byte) buf[i];
                            bytes[2 * i + 1] = (byte) (buf[i] >> 8);
                        }
                        broadcastAudio("a " + (seq++) + " " + Base64.encodeToString(bytes, 0, r * 2, Base64.NO_WRAP));
                    }
                    if (++blocks == 100) {
                        second++;
                        double pk = peak == 0 ? -120 : 20 * Math.log10(peak / 32768.0);
                        double rms = sq == 0 ? -120 : 10 * Math.log10(sq / n / (32768.0 * 32768.0));
                        Boolean silenced = null;
                        AudioRecordingConfiguration cfg = rec.getActiveRecordingConfiguration();
                        if (cfg != null) silenced = cfg.isClientSilenced();
                        String verdict = nz == 0 ? "SILENT(all-zero)" : "REAL";
                        String line = String.format(Locale.UK, "mic stats src=%s s=%d peak=%.1f rms=%.1f nz=%.3f silenced=%s %s",
                                src, second, pk, rms, nz / (double) n, silenced, verdict);
                        if (second <= 5 || second % 30 == 0) L.i(line);
                        broadcast(line);
                        blocks = 0; nz = 0; n = 0; peak = 0; sq = 0;
                        if (secs > 0 && second >= secs) break;
                    }
                }
                L.i("mic STOP src=" + src + " after " + (SystemClock.elapsedRealtime() - t0) / 1000 + " s, frames=" + seq);
            } catch (Throwable t) {
                L.e("mic FAILED src=" + src + " fgsMicType=" + fgsOk, t);
                broadcast("mic live=false src=" + src + " why=" + t.getClass().getSimpleName());
            } finally {
                try { if (rec != null) { rec.stop(); rec.release(); } } catch (Throwable ignored) { }
                try { if (wifiLock != null && wifiLock.isHeld()) wifiLock.release(); } catch (Throwable ignored) { }
                micEnded(this);
            }
        }
    }

    // ---------------------------------------------------------------- TCP line protocol

    private void startServer() {
        new Thread(() -> {
            try {
                server = new ServerSocket();
                server.setReuseAddress(true);
                server.bind(new InetSocketAddress(PORT));
                L.i("server listening on " + PORT);
                while (true) {
                    Socket s = server.accept();
                    Client c = new Client(s);
                    clients.add(c);
                    c.start();
                }
            } catch (Throwable t) {
                L.e("server stopped", t);
            }
        }, "accept").start();
    }

    private void broadcast(String line) {
        for (Client c : clients) c.send(line, false);
    }

    private void broadcastAudio(String line) {
        for (Client c : clients) c.send(line, true);
    }

    private final class Client extends Thread {
        final Socket s;
        final LinkedBlockingDeque<String> out = new LinkedBlockingDeque<>();
        volatile boolean closed;
        int dropped;

        Client(Socket s) {
            super("client-" + s.getRemoteSocketAddress());
            this.s = s;
        }

        /** Never blocks the caller: audio drops its oldest frame when the queue is full. */
        void send(String line, boolean audio) {
            if (closed) return;
            if (audio && out.size() > 20) {
                out.pollFirst();
                dropped++;
            }
            out.offerLast(line);
        }

        @Override
        public void run() {
            String who = String.valueOf(s.getRemoteSocketAddress());
            L.i("client CONNECT " + who + " local=" + s.getLocalSocketAddress() + " " + state());
            refreshNotif();
            Thread writer = new Thread(() -> {
                try {
                    OutputStream o = s.getOutputStream();
                    while (!closed) {
                        String l = out.pollFirst(1, TimeUnit.SECONDS);
                        if (l == null) continue;
                        o.write((l + "\n").getBytes(StandardCharsets.UTF_8));
                    }
                } catch (Throwable t) {
                    if (!closed) L.e("client write " + who, t);
                }
                close();
            }, "w-" + who);
            writer.start();
            try {
                s.setTcpNoDelay(true);
                BufferedReader in = new BufferedReader(new InputStreamReader(s.getInputStream(), StandardCharsets.UTF_8));
                String line;
                while ((line = in.readLine()) != null) {
                    handle(line.trim());
                    if (line.equals("bye")) break;
                }
            } catch (Throwable t) {
                if (!closed) L.e("client read " + who, t);
            }
            close();
            L.i("client CLOSE " + who + " audioDropped=" + dropped + " " + state());
            refreshNotif();
        }

        void close() {
            closed = true;
            clients.remove(this);
            try { s.close(); } catch (Exception ignored) { }
        }

        void handle(String line) {
            String[] a = line.split(" ");
            try {
                switch (a[0]) {
                    case "ping": {
                        long now = SystemClock.elapsedRealtime();
                        if (lastPing != 0 && now - lastPing > 5000) L.i("ping GAP seen by phone: " + (now - lastPing) + " ms " + state());
                        lastPing = now;
                        send("pong " + (a.length > 1 ? a[1] : "") + " " + state(), false);
                        break;
                    }
                    case "mic":
                        if (a.length > 1 && a[1].equals("on")) {
                            boolean st = line.contains("stream");
                            int secs = 0;
                            for (String x : a) if (x.matches("\\d+")) secs = Integer.parseInt(x);
                            micOn("remote", st, secs);
                        } else {
                            micOff("remote");
                        }
                        break;
                    case "status": send("status " + state() + " " + powerState(), false); break;
                    case "wall": send(wallReport(), false); break;
                    case "media": send(mediaReport(), false); break;
                    case "key": send(key(a.length > 1 ? a[1] : "playpause", line.contains("ctl")), false); break;
                    case "net": send(netReport(), false); break;
                    case "dial": send(dial(a[1], Integer.parseInt(a[2])), false); break;
                    case "nsd": registerNsd(); send("nsd re-registered", false); break;
                    case "bye": break;
                    default: send("err unknown " + a[0], false);
                }
            } catch (Throwable t) {
                send("err " + t, false);
            }
        }
    }

    // ---------------------------------------------------------------- NSD

    private void registerNsd() {
        try {
            if (nsd == null) nsd = getSystemService(NsdManager.class);
            if (reg != null) {
                try { nsd.unregisterService(reg); } catch (Exception ignored) { }
            }
            NsdServiceInfo info = new NsdServiceInfo();
            info.setServiceName("tplspike-" + phoneId);
            info.setServiceType("_tplspike._tcp");
            info.setPort(PORT);
            info.setAttribute("id", phoneId);
            info.setAttribute("proto", "1");
            reg = new NsdManager.RegistrationListener() {
                public void onServiceRegistered(NsdServiceInfo i) { L.i("nsd REGISTERED " + i.getServiceName()); }
                public void onRegistrationFailed(NsdServiceInfo i, int e) { L.i("nsd register FAILED err=" + e); }
                public void onServiceUnregistered(NsdServiceInfo i) { L.i("nsd unregistered"); }
                public void onUnregistrationFailed(NsdServiceInfo i, int e) { L.i("nsd unregister failed " + e); }
            };
            nsd.registerService(info, NsdManager.PROTOCOL_DNS_SD, reg);
        } catch (Throwable t) {
            L.e("nsd register threw", t);
        }
    }

    // ---------------------------------------------------------------- state + watchers

    private final Runnable heartbeat = new Runnable() {
        @Override
        public void run() {
            long ago = lastPing == 0 ? -1 : (SystemClock.elapsedRealtime() - lastPing) / 1000;
            L.i("hb " + state() + " " + powerState() + " clients=" + clients.size() + " lastPingAgo=" + ago + "s");
            h.postDelayed(this, 60_000);
        }
    };

    String state() {
        PowerManager pm = getSystemService(PowerManager.class);
        Boolean light = null;
        try { light = (Boolean) PowerManager.class.getMethod("isDeviceLightIdleMode").invoke(pm); } catch (Throwable ignored) { }
        ActivityManager.RunningAppProcessInfo me = new ActivityManager.RunningAppProcessInfo();
        ActivityManager.getMyMemoryState(me);
        return "idle=" + pm.isDeviceIdleMode() + " light=" + light + " scr=" + pm.isInteractive()
                + " imp=" + me.importance + " fgs=" + foreground + " micType=" + micType + " mic=" + (mic != null)
                + " blocked=" + lastBlocked;
    }

    String powerState() {
        PowerManager pm = getSystemService(PowerManager.class);
        ActivityManager am = getSystemService(ActivityManager.class);
        UsageStatsManager us = getSystemService(UsageStatsManager.class);
        return "ignOpt=" + pm.isIgnoringBatteryOptimizations(getPackageName())
                + " bgRestricted=" + am.isBackgroundRestricted()
                + " bucket=" + us.getAppStandbyBucket()
                + " powerSave=" + pm.isPowerSaveMode();
    }

    private void watchNetworks() {
        ConnectivityManager cm = getSystemService(ConnectivityManager.class);
        cm.registerDefaultNetworkCallback(new ConnectivityManager.NetworkCallback() {
            public void onAvailable(Network n) { L.i("net default AVAILABLE " + n); }
            public void onLost(Network n) { L.i("net default LOST " + n); }
            public void onBlockedStatusChanged(Network n, boolean blocked) {
                lastBlocked = blocked;
                L.i("net default BLOCKED=" + blocked + " " + n);
            }
            public void onCapabilitiesChanged(Network n, NetworkCapabilities c) {
                String t = c.hasTransport(NetworkCapabilities.TRANSPORT_WIFI) ? "wifi"
                        : c.hasTransport(NetworkCapabilities.TRANSPORT_CELLULAR) ? "cell"
                        : c.hasTransport(NetworkCapabilities.TRANSPORT_BLUETOOTH) ? "bt" : "other";
                L.i("net default caps " + t + " validated=" + c.hasCapability(NetworkCapabilities.NET_CAPABILITY_VALIDATED));
            }
        }, h);
        cm.registerNetworkCallback(new NetworkRequest.Builder().build(), new ConnectivityManager.NetworkCallback() {
            public void onAvailable(Network n) { L.i("net any AVAILABLE " + n + " " + linkSummary(cm, n)); }
            public void onLost(Network n) { L.i("net any LOST " + n); }
        }, h);
    }

    private static String linkSummary(ConnectivityManager cm, Network n) {
        LinkProperties lp = cm.getLinkProperties(n);
        NetworkCapabilities c = cm.getNetworkCapabilities(n);
        StringBuilder sb = new StringBuilder();
        if (lp != null) {
            sb.append(lp.getInterfaceName()).append(' ').append(lp.getLinkAddresses());
            for (RouteInfo r : lp.getRoutes()) if (r.isDefaultRoute() && r.getGateway() != null) sb.append(" gw=").append(r.getGateway().getHostAddress());
        }
        if (c != null) sb.append(" caps=").append(c.toString().replaceAll("\\s+", " "));
        return sb.toString();
    }

    private void watchMedia() {
        AudioManager am = getSystemService(AudioManager.class);
        am.registerAudioPlaybackCallback(new AudioManager.AudioPlaybackCallback() {
            @Override
            public void onPlaybackConfigChanged(List<AudioPlaybackConfiguration> configs) {
                L.i("media playbackCallback " + playbackSummary(configs) + " musicActive=" + am.isMusicActive());
            }
        }, h);
        tryListenSessions();
    }

    /** Works only with Notification access; retried from mediaReport() so a grant mid-run is picked up. */
    private void tryListenSessions() {
        if (sessionsListener != null) return;
        try {
            MediaSessionManager msm = getSystemService(MediaSessionManager.class);
            MediaSessionManager.OnActiveSessionsChangedListener l = controllers ->
                    L.i("media sessionsChanged " + sessionsSummary(controllers));
            msm.addOnActiveSessionsChangedListener(l, new ComponentName(this, MediaListener.class), h);
            sessionsListener = l;
            L.i("media sessions listener ON");
        } catch (Throwable t) {
            L.e("media sessions listener refused", t);
        }
    }

    private static String playbackSummary(List<AudioPlaybackConfiguration> configs) {
        StringBuilder sb = new StringBuilder("players=" + configs.size() + " [");
        for (AudioPlaybackConfiguration c : configs) {
            AudioAttributes aa = c.getAudioAttributes();
            sb.append("usage=").append(aa.getUsage()).append("/ct=").append(aa.getContentType()).append(' ');
        }
        return sb.append(']').toString();
    }

    private static String sessionsSummary(List<MediaController> list) {
        if (list == null) return "null";
        StringBuilder sb = new StringBuilder("sessions=" + list.size() + " [");
        for (MediaController mc : list) {
            PlaybackState ps = mc.getPlaybackState();
            MediaMetadata md = mc.getMetadata();
            sb.append(mc.getPackageName()).append(" state=").append(ps == null ? "null" : ps.getState())
                    .append(" title=").append(md == null ? "null" : md.getString(MediaMetadata.METADATA_KEY_TITLE)).append("; ");
        }
        return sb.append(']').toString();
    }

    String mediaReport() {
        tryListenSessions();
        AudioManager am = getSystemService(AudioManager.class);
        String sessions;
        try {
            sessions = sessionsSummary(getSystemService(MediaSessionManager.class)
                    .getActiveSessions(new ComponentName(this, MediaListener.class)));
        } catch (SecurityException e) {
            sessions = "sessions=DENIED(" + e.getMessage() + ")";
        }
        return "media musicActive=" + am.isMusicActive() + " " + playbackSummary(am.getActivePlaybackConfigurations()) + " " + sessions;
    }

    String key(String which, boolean viaController) throws InterruptedException {
        AudioManager am = getSystemService(AudioManager.class);
        String before = mediaReport();
        String how;
        if (viaController) {
            List<MediaController> l = getSystemService(MediaSessionManager.class)
                    .getActiveSessions(new ComponentName(this, MediaListener.class));
            if (l.isEmpty()) return "key no-session";
            MediaController.TransportControls tc = l.get(0).getTransportControls();
            if (which.equals("pause")) tc.pause(); else tc.play();
            how = "controller(" + l.get(0).getPackageName() + ")";
        } else {
            int code = which.equals("play") ? KeyEvent.KEYCODE_MEDIA_PLAY
                    : which.equals("pause") ? KeyEvent.KEYCODE_MEDIA_PAUSE : KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE;
            long t = SystemClock.uptimeMillis();
            am.dispatchMediaKeyEvent(new KeyEvent(t, t, KeyEvent.ACTION_DOWN, code, 0));
            am.dispatchMediaKeyEvent(new KeyEvent(t, t, KeyEvent.ACTION_UP, code, 0));
            how = "dispatchMediaKeyEvent(" + code + ")";
        }
        Thread.sleep(1200);
        String line = "key " + which + " via " + how + " | before: " + before + " | after: " + mediaReport();
        L.i(line);
        return line;
    }

    private void watchWallpaper() {
        try {
            WallpaperManager.getInstance(this).addOnColorsChangedListener(
                    (colors, which) -> L.i("wall colorsChanged which=" + which + " " + colorsStr(colors)), h);
        } catch (Throwable t) {
            L.e("wall listener", t);
        }
    }

    private static String colorsStr(WallpaperColors c) {
        if (c == null) return "null";
        StringBuilder sb = new StringBuilder();
        sb.append(String.format("#%06X", c.getPrimaryColor().toArgb() & 0xFFFFFF));
        if (c.getSecondaryColor() != null) sb.append(String.format(",#%06X", c.getSecondaryColor().toArgb() & 0xFFFFFF));
        if (c.getTertiaryColor() != null) sb.append(String.format(",#%06X", c.getTertiaryColor().toArgb() & 0xFFFFFF));
        if (Build.VERSION.SDK_INT >= 31) sb.append(" hints=").append(c.getColorHints());
        return sb.toString();
    }

    String wallReport() {
        WallpaperManager wm = WallpaperManager.getInstance(this);
        StringBuilder sb = new StringBuilder("wall");
        for (int flag : new int[]{WallpaperManager.FLAG_SYSTEM, WallpaperManager.FLAG_LOCK}) {
            String n = flag == WallpaperManager.FLAG_SYSTEM ? "home" : "lock";
            try { sb.append(' ').append(n).append("Colors=").append(colorsStr(wm.getWallpaperColors(flag))); }
            catch (Throwable t) { sb.append(' ').append(n).append("Colors=THREW(").append(t.getClass().getSimpleName()).append(')'); }
            try { sb.append(' ').append(n).append("Id=").append(wm.getWallpaperId(flag)); }
            catch (Throwable t) { sb.append(' ').append(n).append("Id=THREW(").append(t.getClass().getSimpleName()).append(')'); }
            try {
                Object wi = WallpaperManager.class.getMethod("getWallpaperInfo", int.class).invoke(wm, flag);
                sb.append(' ').append(n).append("Info=").append(wi == null ? "null(static)" : ((WallpaperInfo) wi).getServiceName());
            } catch (Throwable t) { sb.append(' ').append(n).append("Info=THREW(").append(rootName(t)).append(')'); }
            try { sb.append(' ').append(n).append("File=").append(wm.getWallpaperFile(flag) != null ? "OK" : "null"); }
            catch (Throwable t) { sb.append(' ').append(n).append("File=THREW(").append(rootName(t)).append(')'); }
        }
        try { sb.append(" getDrawable=").append(wm.getDrawable() != null ? "OK" : "null"); }
        catch (Throwable t) { sb.append(" getDrawable=THREW(").append(rootName(t)).append(')'); }
        try { sb.append(" builtInDrawable=").append(wm.getBuiltInDrawable() != null ? "OK" : "null"); }
        catch (Throwable t) { sb.append(" builtInDrawable=THREW(").append(rootName(t)).append(')'); }
        L.i(sb.toString());
        return sb.toString();
    }

    private static String rootName(Throwable t) {
        while (t.getCause() != null) t = t.getCause();
        return t.getClass().getSimpleName() + ":" + t.getMessage();
    }

    String netReport() throws Exception {
        StringBuilder sb = new StringBuilder("net");
        for (NetworkInterface ni : Collections.list(NetworkInterface.getNetworkInterfaces())) {
            if (!ni.isUp() || ni.isLoopback()) continue;
            sb.append(' ').append(ni.getName()).append('=');
            for (InterfaceAddress ia : ni.getInterfaceAddresses()) {
                InetAddress ad = ia.getAddress();
                if (ad instanceof Inet4Address) sb.append(ad.getHostAddress()).append('/').append(ia.getNetworkPrefixLength()).append(',');
            }
        }
        ConnectivityManager cm = getSystemService(ConnectivityManager.class);
        for (Network n : cm.getAllNetworks()) {
            LinkProperties lp = cm.getLinkProperties(n);
            if (lp != null) sb.append(" | ").append(lp.getInterfaceName()).append(" routes=").append(lp.getRoutes().size());
        }
        return sb.toString();
    }

    String dial(String host, int port) {
        long t = SystemClock.elapsedRealtime();
        try (Socket s = new Socket()) {
            s.connect(new InetSocketAddress(host, port), 3000);
            return "dial OK " + host + ":" + port + " in " + (SystemClock.elapsedRealtime() - t) + " ms";
        } catch (Throwable e) {
            return "dial FAILED " + host + ":" + port + " " + e;
        }
    }

    static void start(Context c, String action, String src) {
        Intent i = new Intent(c, SpikeService.class).putExtra("src", src);
        if (action != null) i.setAction(action);
        try {
            c.startForegroundService(i);
        } catch (Throwable t) {
            L.init(c);
            L.e("startForegroundService(" + action + ", src=" + src + ") REFUSED", t);
        }
    }
}
