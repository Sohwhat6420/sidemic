package com.truephonelink.server;

import android.content.Context;
import android.media.AudioManager;
import android.media.MicrophoneInfo;
import android.media.audiofx.AudioEffect;
import android.net.LocalServerSocket;
import android.net.LocalSocket;
import android.os.Looper;
import android.os.SystemClock;

import java.io.IOException;
import java.io.OutputStream;
import java.util.HashMap;
import java.util.Map;

/**
 * TRUE Phone Link phone server. Pushed to /data/local/tmp and run by the laptop over (wireless) adb:
 *
 *   CLASSPATH=/data/local/tmp/tpl-server.jar app_process / com.truephonelink.server.Main mode=... key=value...
 *
 * Modes:
 *   probe   list microphones, pre-processing effects and source support
 *   record  record N seconds to WAV (raw, plus the DSP chain's output when dsp=1) for A/B measurement
 *   stream  warm server on localabstract:tpl_mic; the mic starts only while a client is connected.
 *           Sends raw s16le mono 48 kHz (after the DSP chain unless dsp=0).
 *   wallpaper  report the lock/home wallpapers and save thumbnails to {out}-lock.png / {out}-home.png
 *              watch=1 keeps {out}-lock.png current and prints "wallpaper-changed lock id=N" on each change
 *
 * Common keys: source=voice-recognition|camcorder|unprocessed|mic|voice-communication|voice-performance
 *              channels=1|2  mix=avg|left|right  ns|agc|aec=on|off|default  dsp=1|0  debug=1
 *              engine=native|java  denoise=dfn|rnnoise|none  atten=35  lib=/data/local/tmp/tpl/libtpl_voice_jni.so
 *              target=-20 maxgain=24 gate=-50 expand=0 (native) / -8 (java) ceiling=-1.5 presence=3
 */
public final class Main {

    private static final int BLOCK_FRAMES = 480; // 10 ms at 48 kHz

    private Main() {
    }

    public static void main(String... args) {
        Map<String, String> o = parse(args);
        Log.debug = "1".equals(o.get("debug"));
        try {
            Looper.prepareMainLooper();
            Workarounds.apply();
            String mode = get(o, "mode", "probe");
            if ("probe".equals(mode)) {
                probe();
            } else if ("record".equals(mode)) {
                record(o);
            } else if ("stream".equals(mode)) {
                stream(o);
            } else if ("wallpaper".equals(mode)) {
                if ("1".equals(o.get("watch"))) {
                    Wallpaper.watch(get(o, "out", "/data/local/tmp/tpl/wallpaper"), Long.parseLong(get(o, "every", "2000")));
                } else {
                    Wallpaper.probe(get(o, "out", "/data/local/tmp/tpl/wallpaper"));
                }
            } else {
                throw new IllegalArgumentException("unknown mode: " + mode);
            }
            System.exit(0);
        } catch (Throwable t) {
            Log.e(t.toString());
            t.printStackTrace();
            System.exit(1);
        }
    }

    private static void probe() throws IOException {
        AudioManager am = (AudioManager) FakeContext.get().getSystemService(Context.AUDIO_SERVICE);
        Log.i("unprocessed source supported: " + am.getProperty(AudioManager.PROPERTY_SUPPORT_AUDIO_SOURCE_UNPROCESSED));
        Log.i("native output rate: " + am.getProperty(AudioManager.PROPERTY_OUTPUT_SAMPLE_RATE)
                + ", frames per buffer: " + am.getProperty(AudioManager.PROPERTY_OUTPUT_FRAMES_PER_BUFFER));
        for (MicrophoneInfo m : am.getMicrophones()) {
            Log.i("mic: " + MicCapture.describe(m));
        }
        for (AudioEffect.Descriptor d : AudioEffect.queryEffects()) {
            Log.i("effect: " + d.name + " | " + d.implementor + " | type " + d.type + " | connect " + d.connectMode);
        }
    }

    private static MicCapture open(Map<String, String> o) {
        return new MicCapture(get(o, "source", "voice-recognition"), Integer.parseInt(get(o, "channels", "1")),
                tri(o.get("ns")), tri(o.get("agc")), tri(o.get("aec")));
    }

    /**
     * The voice chain: native (Rust; DeepFilterNet 3 capped at 35 dB by default, the 15 Oct 2026 benchmark's
     * winner) when the library loads, else the Java chain (leveller + limiter only), so the mic always works.
     */
    private static VoiceChain chain(Map<String, String> o) {
        float target = Float.parseFloat(get(o, "target", "-20"));
        float maxGain = Float.parseFloat(get(o, "maxgain", "24"));
        float ceiling = Float.parseFloat(get(o, "ceiling", "-1.5"));
        float presence = Float.parseFloat(get(o, "presence", "3"));
        if (!"java".equals(get(o, "engine", "native")) && NativeVoice.load(get(o, "lib", NativeVoice.DEFAULT_LIB))) {
            String denoise = get(o, "denoise", "dfn");
            int kind = "dfn".equals(denoise) ? 2 : "rnnoise".equals(denoise) ? 1 : 0;
            float expand = Float.parseFloat(get(o, "expand", kind == 0 ? "-8" : "0"));
            VoiceChain c = new NativeVoice(kind, Float.parseFloat(get(o, "atten", "35")), target, maxGain, expand,
                    ceiling, presence);
            Log.i("voice chain: " + c.describe());
            return c;
        }
        Dsp.Params p = new Dsp.Params();
        p.targetDb = target;
        p.maxGainDb = maxGain;
        p.gateDb = Float.parseFloat(get(o, "gate", "-50"));
        p.expandDb = Float.parseFloat(get(o, "expand", "-8"));
        p.ceilingDb = ceiling;
        p.presenceDb = presence;
        VoiceChain c = new Dsp(p);
        Log.i("voice chain: " + c.describe());
        return c;
    }

    /** Interleaved 16-bit frames to mono float, per the mix rule. */
    private static void toMono(short[] in, int frames, int channels, String mix, float[] out) {
        for (int i = 0; i < frames; i++) {
            float v;
            if (channels == 1) {
                v = in[i];
            } else if ("left".equals(mix)) {
                v = in[2 * i];
            } else if ("right".equals(mix)) {
                v = in[2 * i + 1];
            } else {
                v = (in[2 * i] + in[2 * i + 1]) * 0.5f;
            }
            out[i] = v / 32768f;
        }
    }

    private static void toShorts(float[] in, int n, short[] out) {
        for (int i = 0; i < n; i++) {
            float v = in[i] * 32767f;
            out[i] = (short) (v > 32767f ? 32767 : (v < -32768f ? -32768 : Math.round(v)));
        }
    }

    private static void record(Map<String, String> o) throws IOException {
        int secs = Integer.parseInt(get(o, "secs", "10"));
        String out = get(o, "out", "/data/local/tmp/tpl/rec");
        boolean useDsp = !"0".equals(get(o, "dsp", "1"));
        String mix = get(o, "mix", "avg");

        MicCapture cap = open(o);
        int ch = cap.channels;
        VoiceChain dsp = useDsp ? chain(o) : null;
        Stats raw = new Stats(ch);
        Stats processed = new Stats(1);
        short[] buf = new short[BLOCK_FRAMES * ch];
        float[] mono = new float[BLOCK_FRAMES];
        short[] monoOut = new short[BLOCK_FRAMES];

        WavWriter rawWav = new WavWriter(out + "-raw.wav", ch, MicCapture.SAMPLE_RATE);
        WavWriter dspWav = useDsp ? new WavWriter(out + "-dsp.wav", 1, MicCapture.SAMPLE_RATE) : null;
        cap.start();
        long total = (long) secs * MicCapture.SAMPLE_RATE;
        long done = 0;
        long dspNanos = 0;
        try {
            while (done < total) {
                int n = cap.read(buf, buf.length);
                if (n < 0) {
                    throw new IOException("AudioRecord.read error " + n);
                }
                int frames = n / ch;
                rawWav.write(buf, n);
                raw.add(buf, frames);
                if (dsp != null) {
                    toMono(buf, frames, ch, mix, mono);
                    long t = System.nanoTime();
                    dsp.process(mono, frames);
                    dspNanos += System.nanoTime() - t;
                    toShorts(mono, frames, monoOut);
                    dspWav.write(monoOut, frames);
                    processed.add(monoOut, frames);
                }
                done += frames;
            }
        } finally {
            cap.release();
            if (dsp != null) {
                dsp.close();
            }
            rawWav.close();
            if (dspWav != null) {
                dspWav.close();
            }
        }
        Log.i("raw: " + raw.summary());
        if (dsp != null) {
            Log.i("dsp: " + processed.summary());
            Log.i(String.format("dsp CPU: %.1f%% of real time (%.2f ms per 10 ms block)", 100.0 * dspNanos / (done * 1e9 / MicCapture.SAMPLE_RATE), dspNanos / 1e6 / Math.max(1, done / BLOCK_FRAMES)));
        }
        Log.i("wrote " + out + "-raw.wav" + (dsp != null ? " and " + out + "-dsp.wav" : ""));
    }

    private static void stream(Map<String, String> o) throws IOException {
        boolean useDsp = !"0".equals(get(o, "dsp", "1"));
        String mix = get(o, "mix", "avg");
        LocalServerSocket server = new LocalServerSocket("tpl_mic");
        Log.i("listening on localabstract:tpl_mic (dsp=" + (useDsp ? "on" : "off") + ")");
        while (true) {
            LocalSocket client = server.accept();
            long t0 = SystemClock.elapsedRealtime();
            Log.i("client connected; starting mic");
            MicCapture cap = null;
            VoiceChain dsp = null;
            try {
                cap = open(o);
                int ch = cap.channels;
                dsp = useDsp ? chain(o) : null;
                cap.start();
                OutputStream os = client.getOutputStream();
                short[] buf = new short[BLOCK_FRAMES * ch];
                float[] mono = new float[BLOCK_FRAMES];
                short[] monoOut = new short[BLOCK_FRAMES];
                byte[] bytes = new byte[BLOCK_FRAMES * 2];
                boolean first = true;
                long blocks = 0;
                while (true) {
                    int n = cap.read(buf, buf.length);
                    if (n < 0) {
                        throw new IOException("AudioRecord.read error " + n);
                    }
                    int frames = n / ch;
                    if (first) {
                        Log.i("first audio " + (SystemClock.elapsedRealtime() - t0) + " ms after connect");
                        first = false;
                    }
                    toMono(buf, frames, ch, mix, mono);
                    if (dsp != null) {
                        dsp.process(mono, frames);
                    }
                    toShorts(mono, frames, monoOut);
                    for (int i = 0; i < frames; i++) {
                        bytes[2 * i] = (byte) monoOut[i];
                        bytes[2 * i + 1] = (byte) (monoOut[i] >> 8);
                    }
                    os.write(bytes, 0, frames * 2);
                    if (dsp instanceof Dsp && Log.debug && ++blocks % 100 == 0) {
                        Log.d(String.format("in %.1f dB, gain %.1f dB, limiter %.1f dB", ((Dsp) dsp).lastInputDb, ((Dsp) dsp).lastGainDb,
                                ((Dsp) dsp).lastLimitDb));
                    }
                }
            } catch (IOException e) {
                Log.i("client gone (" + e.getMessage() + "); mic stopped");
            } finally {
                if (cap != null) {
                    cap.release();
                }
                if (dsp != null) {
                    dsp.close();
                }
                try {
                    client.close();
                } catch (IOException ignored) {
                    // already closed
                }
            }
        }
    }

    /** Peak, RMS, clipped-sample count and (stereo) inter-channel correlation. */
    static final class Stats {
        private final int ch;
        private final long[] clipped;
        private final int[] peak;
        private final double[] sumSq;
        private double sumLR;
        private long frames;

        Stats(int ch) {
            this.ch = ch;
            clipped = new long[ch];
            peak = new int[ch];
            sumSq = new double[ch];
        }

        void add(short[] buf, int n) {
            for (int i = 0; i < n; i++) {
                for (int c = 0; c < ch; c++) {
                    int v = buf[i * ch + c];
                    int a = Math.abs(v);
                    if (a > peak[c]) {
                        peak[c] = a;
                    }
                    if (a >= 32767) {
                        clipped[c]++;
                    }
                    sumSq[c] += (double) v * v;
                }
                if (ch == 2) {
                    sumLR += (double) buf[2 * i] * buf[2 * i + 1];
                }
            }
            frames += n;
        }

        String summary() {
            StringBuilder sb = new StringBuilder();
            for (int c = 0; c < ch; c++) {
                double rms = Math.sqrt(sumSq[c] / Math.max(1, frames)) / 32768.0;
                sb.append(String.format("ch%d peak %.1f dBFS rms %.1f dBFS clipped %d; ", c,
                        20 * Math.log10(Math.max(peak[c], 1) / 32768.0), 20 * Math.log10(Math.max(rms, 1e-9)), clipped[c]));
            }
            if (ch == 2) {
                sb.append(String.format("L/R correlation %.3f", sumLR / Math.sqrt(sumSq[0] * sumSq[1] + 1e-9)));
            }
            return sb.toString();
        }
    }

    private static Boolean tri(String v) {
        if ("on".equals(v) || "1".equals(v)) {
            return Boolean.TRUE;
        }
        if ("off".equals(v) || "0".equals(v)) {
            return Boolean.FALSE;
        }
        return null;
    }

    private static String get(Map<String, String> o, String key, String def) {
        String v = o.get(key);
        return v == null ? def : v;
    }

    private static Map<String, String> parse(String[] args) {
        Map<String, String> o = new HashMap<>();
        for (String a : args) {
            int eq = a.indexOf('=');
            if (eq > 0) {
                o.put(a.substring(0, eq), a.substring(eq + 1));
            }
        }
        return o;
    }
}
