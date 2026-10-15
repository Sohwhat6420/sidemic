package com.truephonelink.server;

import android.app.WallpaperManager;
import android.content.Context;
import android.media.AudioManager;
import android.media.MediaMetadata;
import android.media.session.MediaController;
import android.media.session.MediaSessionManager;
import android.media.session.PlaybackState;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.os.BatteryManager;
import android.os.Build;
import android.util.Base64;

import org.json.JSONException;
import org.json.JSONObject;

import java.io.BufferedReader;
import java.io.IOException;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.util.List;
import java.util.Map;

/**
 * The laptop app's phone side: the mic on localabstract:tpl_mic, and a control channel on localabstract:tpl_ctl.
 *
 * Control channel, one JSON object per line.
 *   phone to laptop: {"t":"hello","name","model","android"}  {"t":"battery","level","charging"}
 *                    {"t":"media","playing","pkg","title","artist"}  {"t":"wallpaper","id","jpeg":base64}  {"t":"pong","n"}
 *                    {"t":"volume","level":0..1}
 *   laptop to phone: {"c":"play"} {"c":"pause"} {"c":"duck","level":0.3} {"c":"unduck"} {"c":"volume","level":0..1}
 *                    {"c":"ping","n":1}
 * Only changes are sent, so an idle phone sends almost nothing.
 */
final class Serve {

    private Serve() {
    }

    static void run(Map<String, String> o) throws IOException {
        LocalServerSocket mic = new LocalServerSocket("tpl_mic");
        // Anonymous classes, not lambdas: the build compiles against android.jar, which has no LambdaMetafactory.
        Thread micThread = new Thread(new Runnable() {
            @Override
            public void run() {
                while (true) {
                    try {
                        Main.micClient(o, mic.accept());
                    } catch (IOException e) {
                        Log.e("mic accept: " + e);
                        return;
                    }
                }
            }
        }, "tpl-mic");
        micThread.setDaemon(true);
        micThread.start();

        LocalServerSocket ctl = new LocalServerSocket("tpl_ctl");
        Log.i("serving localabstract:tpl_mic and localabstract:tpl_ctl");
        while (true) {
            LocalSocket client = ctl.accept();
            new Control(client).run();
        }
    }

    /** One laptop connection. Returns when the laptop goes away. */
    private static final class Control {
        private final LocalSocket socket;
        private final OutputStream out;
        private final Context ctx = FakeContext.get();
        private final AudioManager audio = (AudioManager) ctx.getSystemService(Context.AUDIO_SERVICE);
        private volatile boolean open = true;
        private volatile int savedVolume = -1;

        Control(LocalSocket socket) throws IOException {
            this.socket = socket;
            this.out = socket.getOutputStream();
        }

        void run() {
            Log.i("control: laptop connected");
            Thread watcher = new Thread(new Runnable() {
                @Override
                public void run() {
                    watch();
                }
            }, "tpl-watch");
            watcher.setDaemon(true);
            try {
                send(new JSONObject().put("t", "hello").put("name", deviceName()).put("model", Build.MODEL)
                        .put("android", Build.VERSION.RELEASE));
                watcher.start();
                BufferedReader in = new BufferedReader(new InputStreamReader(socket.getInputStream(), StandardCharsets.UTF_8));
                String line;
                while ((line = in.readLine()) != null) {
                    command(new JSONObject(line));
                }
            } catch (IOException | JSONException e) {
                Log.i("control: " + e.getMessage());
            } finally {
                open = false;
                unduck(); // never leave the phone quiet because the laptop went away
                try {
                    socket.close();
                } catch (IOException ignored) {
                    // already closed
                }
                Log.i("control: laptop gone");
            }
        }

        private void command(JSONObject c) throws IOException, JSONException {
            switch (c.optString("c")) {
                case "play":
                    transport(true);
                    break;
                case "pause":
                    transport(false);
                    break;
                case "duck":
                    duck(c.optDouble("level", 0.3));
                    break;
                case "unduck":
                    unduck();
                    break;
                case "volume": {
                    int max = audio.getStreamMaxVolume(AudioManager.STREAM_MUSIC);
                    int v = (int) Math.round(Math.max(0, Math.min(1, c.optDouble("level", 0.5))) * max);
                    synchronized (this) {
                        savedVolume = -1; // the user's choice wins over a duck in progress
                    }
                    audio.setStreamVolume(AudioManager.STREAM_MUSIC, v, 0);
                    break;
                }
                case "ping":
                    send(new JSONObject().put("t", "pong").put("n", c.optLong("n")));
                    break;
                default:
                    Log.i("control: unknown command " + c);
            }
        }

        /** Polls the cheap things often and sends only what changed. */
        private void watch() {
            WallpaperManager wm = WallpaperManager.getInstance(ctx);
            BatteryManager battery = (BatteryManager) ctx.getSystemService(Context.BATTERY_SERVICE);
            String lastMedia = null;
            String lastBattery = null;
            int lastVolume = -1;
            int lastWallpaper = Integer.MIN_VALUE;
            long tick = 0;
            try {
                while (open) {
                    JSONObject media = media();
                    String m = media.toString();
                    if (!m.equals(lastMedia)) {
                        send(media);
                        lastMedia = m;
                    }
                    int vol = audio.getStreamVolume(AudioManager.STREAM_MUSIC);
                    if (vol != lastVolume && savedVolume < 0) { // a duck is ours, not a volume change to report
                        send(new JSONObject().put("t", "volume")
                                .put("level", vol / (double) Math.max(1, audio.getStreamMaxVolume(AudioManager.STREAM_MUSIC))));
                        lastVolume = vol;
                    }
                    if (tick % 10 == 0) { // every 5 s
                        JSONObject b = new JSONObject().put("t", "battery")
                                .put("level", battery.getIntProperty(BatteryManager.BATTERY_PROPERTY_CAPACITY))
                                .put("charging", battery.isCharging());
                        if (!b.toString().equals(lastBattery)) {
                            send(b);
                            lastBattery = b.toString();
                        }
                    }
                    if (tick % 4 == 0) { // every 2 s; one binder call while it stays the same
                        int id = wm.getWallpaperId(WallpaperManager.FLAG_LOCK);
                        if (id != lastWallpaper) {
                            lastWallpaper = id;
                            byte[] jpeg = Wallpaper.jpeg(wm, WallpaperManager.FLAG_LOCK);
                            if (jpeg != null) {
                                send(new JSONObject().put("t", "wallpaper").put("id", id)
                                        .put("jpeg", Base64.encodeToString(jpeg, Base64.NO_WRAP)));
                            }
                        }
                    }
                    tick++;
                    Thread.sleep(500);
                }
            } catch (Exception e) {
                Log.i("watch stopped: " + e);
            }
        }

        private JSONObject media() throws JSONException {
            JSONObject j = new JSONObject().put("t", "media").put("playing", false);
            MediaController c = current();
            if (c != null) {
                PlaybackState s = c.getPlaybackState();
                MediaMetadata md = c.getMetadata();
                j.put("playing", s != null && s.getState() == PlaybackState.STATE_PLAYING).put("pkg", c.getPackageName());
                if (md != null) {
                    j.put("title", md.getString(MediaMetadata.METADATA_KEY_TITLE)).put("artist", md.getString(MediaMetadata.METADATA_KEY_ARTIST));
                }
            }
            return j;
        }

        /** The playing session if there is one, otherwise the most recent. Shell holds MEDIA_CONTENT_CONTROL. */
        private MediaController current() {
            try {
                MediaSessionManager msm = (MediaSessionManager) ctx.getSystemService(Context.MEDIA_SESSION_SERVICE);
                List<MediaController> list = msm.getActiveSessions(null);
                for (MediaController c : list) {
                    PlaybackState s = c.getPlaybackState();
                    if (s != null && s.getState() == PlaybackState.STATE_PLAYING) {
                        return c;
                    }
                }
                return list.isEmpty() ? null : list.get(0);
            } catch (RuntimeException e) {
                Log.d("media sessions: " + e);
                return null;
            }
        }

        private void transport(boolean play) {
            MediaController c = current();
            if (c == null) {
                Log.i("control: no media session to " + (play ? "play" : "pause"));
                return;
            }
            if (play) {
                c.getTransportControls().play();
            } else {
                c.getTransportControls().pause();
            }
        }

        private synchronized void duck(double level) {
            int now = audio.getStreamVolume(AudioManager.STREAM_MUSIC);
            if (savedVolume < 0) {
                savedVolume = now;
            }
            int target = (int) Math.max(1, Math.round(savedVolume * level));
            audio.setStreamVolume(AudioManager.STREAM_MUSIC, Math.min(target, now), 0);
        }

        private synchronized void unduck() {
            if (savedVolume >= 0) {
                audio.setStreamVolume(AudioManager.STREAM_MUSIC, savedVolume, 0);
                savedVolume = -1;
            }
        }

        private synchronized void send(JSONObject j) throws IOException {
            out.write((j.toString() + "\n").getBytes(StandardCharsets.UTF_8));
            out.flush();
        }

        private static String deviceName() {
            try {
                Process p = Runtime.getRuntime().exec(new String[] {"settings", "get", "global", "device_name"});
                String s = new BufferedReader(new InputStreamReader(p.getInputStream(), StandardCharsets.UTF_8)).readLine();
                return s == null || s.isEmpty() || "null".equals(s) ? Build.MODEL : s.trim();
            } catch (IOException e) {
                return Build.MODEL;
            }
        }
    }
}
