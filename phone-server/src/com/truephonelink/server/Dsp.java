package com.truephonelink.server;

/**
 * The "perfect mic" chain, run on the phone so the laptop does no audio work while gaming.
 *
 * Input is a clean, un-gained source (voice-recognition or unprocessed: no AGC, so nothing clips upstream).
 * The chain then gives it camcorder-like loudness without camcorder's clipping:
 *
 *   high-pass 90 Hz -> presence peak 3 kHz -> speech leveller (+ downward expander between words)
 *   -> 1.5 ms look-ahead limiter at the ceiling -> hard clamp
 *
 * Mono float in [-1, 1] at 48 kHz. Not thread-safe; one instance per stream.
 */
final class Dsp {

    static final class Params {
        float sampleRate = 48000f;
        float highPassHz = 90f;
        float presenceHz = 3000f;
        float presenceDb = 3f;
        float targetDb = -20f;     // speech RMS the leveller aims for
        float maxGainDb = 24f;     // how far a distant, quiet voice may be lifted
        float minGainDb = -12f;    // how far a loud voice may be pulled down
        float gateDb = -50f;       // below this (after the current gain is removed) it is "not speech"
        float expandDb = -8f;      // extra cut between words, so the noise floor is not lifted with the voice
        float ceilingDb = -1f;     // the limiter's ceiling; nothing leaves above it
        float riseDbPerSec = 6f;   // the leveller lifts slowly (no pumping) ...
        float fallDbPerSec = 60f;  // ... and pulls down quickly (the limiter catches the first transient)
    }

    private final Params p;
    private final Biquad highPass;
    private final Biquad presence;

    // Leveller state.
    private final float envCoef;
    private double env = 1e-9;
    private float gainDb = 0f;
    private float expandGainDb = 0f;
    private int holdSamples;
    private int holdRemaining;

    // Limiter state.
    private final float[] delay;
    private int delayPos;
    private final float ceiling;
    private final float limitReleaseCoef;
    private float limitGain = 1f;

    // Per-block meter readings for the dashboard / logs.
    float lastInputDb = -120f;
    float lastGainDb = 0f;
    float lastLimitDb = 0f;

    Dsp(Params p) {
        this.p = p;
        highPass = Biquad.highPass(p.sampleRate, p.highPassHz, 0.7071f);
        presence = Biquad.peaking(p.sampleRate, p.presenceHz, 1.0f, p.presenceDb);
        envCoef = (float) Math.exp(-1.0 / (0.050 * p.sampleRate)); // ~50 ms RMS window
        holdSamples = (int) (0.200f * p.sampleRate);               // keep word tails for 200 ms
        delay = new float[(int) (0.0015f * p.sampleRate)];          // 72 samples at 48 kHz
        ceiling = dbToLin(p.ceilingDb);
        limitReleaseCoef = 1f - (float) Math.exp(-1.0 / (0.050 * p.sampleRate));
    }

    /** Processes {@code n} samples in place. Call with ~10 ms blocks; the leveller's target is set per block. */
    void process(float[] buf, int n) {
        // 1. Filters, and the RMS envelope of the filtered input.
        double sumSq = 0;
        for (int i = 0; i < n; i++) {
            float x = presence.process(highPass.process(buf[i]));
            buf[i] = x;
            env = envCoef * env + (1 - envCoef) * x * x;
            sumSq += x * x;
        }
        float levelDb = (float) (10 * Math.log10(env + 1e-12));
        lastInputDb = (float) (10 * Math.log10(sumSq / Math.max(1, n) + 1e-12));

        // 2. Leveller target for this block.
        float blockSec = n / p.sampleRate;
        boolean speech = levelDb > p.gateDb;
        float targetGain = gainDb;
        if (speech) {
            targetGain = clamp(p.targetDb - levelDb, p.minGainDb, p.maxGainDb);
            holdRemaining = holdSamples;
        } // else: hold the gain, so the gaps between words are not lifted

        float startGain = gainDb;
        if (targetGain > gainDb) {
            gainDb = Math.min(targetGain, gainDb + p.riseDbPerSec * blockSec);
        } else {
            gainDb = Math.max(targetGain, gainDb - p.fallDbPerSec * blockSec);
        }

        // Downward expander: after the hold, ease towards expandDb; on speech, open quickly.
        float startExpand = expandGainDb;
        if (speech || holdRemaining > 0) {
            expandGainDb = Math.min(0f, expandGainDb + 200f * blockSec); // opens in ~40 ms
        } else {
            expandGainDb = Math.max(p.expandDb, expandGainDb - 40f * blockSec); // closes in ~200 ms
        }
        holdRemaining = Math.max(0, holdRemaining - n);

        // 3. Apply the gain (interpolated across the block) and the look-ahead limiter.
        float minLimit = 1f;
        for (int i = 0; i < n; i++) {
            float t = (i + 1) / (float) n;
            float g = dbToLin(startGain + (gainDb - startGain) * t + startExpand + (expandGainDb - startExpand) * t);
            float x = buf[i] * g;

            // Look-ahead: the sample entering the delay line sets the gain before it reaches the output.
            float out = delay[delayPos];
            delay[delayPos] = x;
            delayPos = (delayPos + 1) % delay.length;

            float peak = 0f;
            for (float v : delay) {
                float a = Math.abs(v);
                if (a > peak) {
                    peak = a;
                }
            }
            float target = peak > ceiling ? ceiling / peak : 1f;
            if (target < limitGain) {
                limitGain = target;
            } else {
                limitGain += (target - limitGain) * limitReleaseCoef;
            }
            if (limitGain < minLimit) {
                minLimit = limitGain;
            }
            float y = out * limitGain;
            buf[i] = y > 1f ? 1f : (y < -1f ? -1f : y);
        }
        lastGainDb = gainDb + expandGainDb;
        lastLimitDb = linToDb(minLimit);
    }

    static float dbToLin(float db) {
        return (float) Math.pow(10, db / 20.0);
    }

    static float linToDb(float lin) {
        return (float) (20 * Math.log10(Math.max(lin, 1e-9)));
    }

    private static float clamp(float v, float lo, float hi) {
        return v < lo ? lo : (v > hi ? hi : v);
    }

    /** RBJ audio-EQ-cookbook biquad, transposed direct form II. */
    static final class Biquad {
        private final float b0, b1, b2, a1, a2;
        private float z1, z2;

        private Biquad(double b0, double b1, double b2, double a0, double a1, double a2) {
            this.b0 = (float) (b0 / a0);
            this.b1 = (float) (b1 / a0);
            this.b2 = (float) (b2 / a0);
            this.a1 = (float) (a1 / a0);
            this.a2 = (float) (a2 / a0);
        }

        static Biquad highPass(float fs, float f0, float q) {
            double w = 2 * Math.PI * f0 / fs;
            double cos = Math.cos(w);
            double alpha = Math.sin(w) / (2 * q);
            return new Biquad((1 + cos) / 2, -(1 + cos), (1 + cos) / 2, 1 + alpha, -2 * cos, 1 - alpha);
        }

        static Biquad peaking(float fs, float f0, float q, float gainDb) {
            double a = Math.pow(10, gainDb / 40.0);
            double w = 2 * Math.PI * f0 / fs;
            double cos = Math.cos(w);
            double alpha = Math.sin(w) / (2 * q);
            return new Biquad(1 + alpha * a, -2 * cos, 1 - alpha * a, 1 + alpha / a, -2 * cos, 1 - alpha / a);
        }

        float process(float x) {
            float y = b0 * x + z1;
            z1 = b1 * x - a1 * y + z2;
            z2 = b2 * x - a2 * y;
            return y;
        }
    }
}
