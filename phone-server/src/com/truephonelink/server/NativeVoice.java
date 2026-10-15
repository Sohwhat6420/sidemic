package com.truephonelink.server;

/**
 * The Rust voice chain (crates/voice-jni): high-pass -> noise suppression (DeepFilterNet 3 / RNNoise / none)
 * -> presence -> leveller -> look-ahead limiter. Loaded from a path, because app_process has no app library dir.
 */
final class NativeVoice implements VoiceChain {

    static final String DEFAULT_LIB = "/data/local/tmp/tpl/libtpl_voice_jni.so";
    private static final int BLOCK = 480;

    private static boolean loaded;

    private long handle;
    private final String description;
    private final float[] padded = new float[BLOCK];

    /** Loads the library once; returns false (and logs why) if it is missing or will not link. */
    static synchronized boolean load(String path) {
        if (!loaded) {
            try {
                System.load(path);
                loaded = true;
            } catch (Throwable t) {
                Log.i("native voice chain unavailable (" + t.getMessage() + "); using the Java chain");
            }
        }
        return loaded;
    }

    /** {@code denoise}: 0 none, 1 RNNoise, 2 DeepFilterNet 3. */
    NativeVoice(int denoise, float attenDb, float targetDb, float maxGainDb, float expandDb, float ceilingDb,
            float presenceDb) {
        handle = create(denoise, attenDb, targetDb, maxGainDb, expandDb, ceilingDb, presenceDb);
        String ns = denoise == 2 ? "DeepFilterNet3 (cap " + attenDb + " dB)" : denoise == 1 ? "RNNoise" : "no NS";
        description = "native: " + ns + ", latency " + latencySamples(handle) / 48f + " ms";
    }

    @Override
    public void process(float[] block, int n) {
        if (n == BLOCK) {
            process(handle, block);
        } else {
            // A short read (rare): pad to a full block so the models keep their framing.
            System.arraycopy(block, 0, padded, 0, n);
            java.util.Arrays.fill(padded, n, BLOCK, 0f);
            process(handle, padded);
            System.arraycopy(padded, 0, block, 0, n);
        }
    }

    @Override
    public String describe() {
        return description;
    }

    @Override
    public void close() {
        if (handle != 0) {
            destroy(handle);
            handle = 0;
        }
    }

    private static native long create(int denoise, float attenDb, float targetDb, float maxGainDb, float expandDb,
            float ceilingDb, float presenceDb);

    private static native void process(long handle, float[] block);

    private static native int latencySamples(long handle);

    private static native void destroy(long handle);
}
