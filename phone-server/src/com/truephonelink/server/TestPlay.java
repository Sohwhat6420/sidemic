package com.truephonelink.server;

import android.content.Context;
import android.media.AudioAttributes;
import android.media.AudioFormat;
import android.media.AudioTrack;
import android.media.MediaMetadata;
import android.media.session.MediaSession;
import android.media.session.PlaybackState;
import android.os.Handler;
import android.os.Looper;

import java.io.PrintStream;
import java.util.Map;

/**
 * A stand-in for TikTok, for testing the laptop app without touching the phone: a real media session (owned by
 * com.android.shell) playing a very quiet tone, which pauses and resumes when the laptop sends pause/play.
 *
 *   mode=testplay seconds=90 level=-45
 */
final class TestPlay {

    private TestPlay() {
    }

    /** mode=media: prints what MediaSessionManager returns to the shell user (the server's media watcher). */
    static void probe(PrintStream out) {
        try {
            android.media.session.MediaSessionManager msm = (android.media.session.MediaSessionManager)
                    FakeContext.get().getSystemService(Context.MEDIA_SESSION_SERVICE);
            for (android.media.session.MediaController c : msm.getActiveSessions(null)) {
                out.println(c.getPackageName() + " " + c.getPlaybackState());
            }
            for (String u : new String[] {"ALL", "CURRENT", "SYSTEM"}) {
                try {
                    Object h = android.os.UserHandle.class.getField(u).get(null);
                    java.lang.reflect.Method m = msm.getClass().getMethod("getActiveSessionsForUser",
                            android.content.ComponentName.class, android.os.UserHandle.class);
                    java.util.List<?> l = (java.util.List<?>) m.invoke(msm, null, h);
                    out.println(u + ": " + l.size());
                    for (Object c : l) {
                        android.media.session.MediaController mc = (android.media.session.MediaController) c;
                        out.println("  " + mc.getPackageName() + " " + mc.getPlaybackState());
                    }
                } catch (Throwable t) {
                    out.println(u + ": " + t + (t.getCause() != null ? " / " + t.getCause() : ""));
                }
            }
            out.println("media probe done");
        } catch (Throwable t) {
            t.printStackTrace(out);
        }
    }

    static void run(Map<String, String> o, final PrintStream out) throws Exception {
        final int seconds = Integer.parseInt(o.containsKey("seconds") ? o.get("seconds") : "90");
        final double level = Math.pow(10, Double.parseDouble(o.containsKey("level") ? o.get("level") : "-45") / 20);
        Context ctx = FakeContext.get();
        if (Looper.myLooper() == null) {
            Looper.prepare();
        }
        final Handler handler = new Handler(Looper.myLooper());

        final int rate = 48000;
        final AudioTrack track = new AudioTrack.Builder()
                .setAudioAttributes(new AudioAttributes.Builder()
                        .setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC)
                        .build())
                .setAudioFormat(new AudioFormat.Builder()
                        .setSampleRate(rate)
                        .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                        .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
                        .build())
                .setTransferMode(AudioTrack.MODE_STREAM)
                .build();

        final MediaSession session = new MediaSession(ctx, "tpl-test");
        session.setMetadata(new MediaMetadata.Builder()
                .putString(MediaMetadata.METADATA_KEY_TITLE, "TRUE Phone Link test tone")
                .putString(MediaMetadata.METADATA_KEY_ARTIST, "TRUE Phone Link")
                .build());
        final boolean[] playing = {true};
        final Runnable[] setState = new Runnable[1];
        setState[0] = new Runnable() {
            @Override
            public void run() {
                session.setPlaybackState(new PlaybackState.Builder()
                        .setActions(PlaybackState.ACTION_PLAY | PlaybackState.ACTION_PAUSE | PlaybackState.ACTION_PLAY_PAUSE)
                        .setState(playing[0] ? PlaybackState.STATE_PLAYING : PlaybackState.STATE_PAUSED, 0, 1f)
                        .build());
            }
        };
        session.setCallback(new MediaSession.Callback() {
            @Override
            public void onPlay() {
                playing[0] = true;
                track.play();
                setState[0].run();
                out.println("testplay: play (from media controls)");
            }

            @Override
            public void onPause() {
                playing[0] = false;
                track.pause();
                setState[0].run();
                out.println("testplay: pause (from media controls)");
            }
        }, handler);
        setState[0].run();
        session.setActive(true);
        track.play();
        out.println("testplay: playing a " + o.get("level") + " dBFS tone for " + seconds + " s");

        // Feed the tone from a worker thread; the looper thread handles the session's callbacks.
        new Thread(new Runnable() {
            @Override
            public void run() {
                short[] buf = new short[rate / 50];
                double phase = 0;
                long end = System.currentTimeMillis() + seconds * 1000L;
                while (System.currentTimeMillis() < end) {
                    for (int i = 0; i < buf.length; i++) {
                        buf[i] = (short) (Math.sin(phase) * level * 32767);
                        phase += 2 * Math.PI * 440 / rate;
                    }
                    if (playing[0]) {
                        track.write(buf, 0, buf.length);
                    } else {
                        try {
                            Thread.sleep(20);
                        } catch (InterruptedException e) {
                            return;
                        }
                    }
                }
                handler.post(new Runnable() {
                    @Override
                    public void run() {
                        session.setActive(false);
                        session.release();
                        track.stop();
                        track.release();
                        out.println("testplay: done");
                        System.exit(0);
                    }
                });
            }
        }).start();
        Looper.loop();
    }
}
