package com.truephonelink.server;

import android.media.AudioDeviceInfo;
import android.media.AudioFormat;
import android.media.AudioRecord;
import android.media.AudioRecordingConfiguration;
import android.media.MediaRecorder;
import android.media.MicrophoneInfo;
import android.media.audiofx.AcousticEchoCanceler;
import android.media.audiofx.AudioEffect;
import android.media.audiofx.AutomaticGainControl;
import android.media.audiofx.NoiseSuppressor;

import java.io.IOException;
import java.util.List;

/** One AudioRecord on a chosen source, plus the platform pre-processing effects we choose to switch on or off. */
final class MicCapture {

    static final int SAMPLE_RATE = 48000;

    final String sourceName;
    final int channels;
    private final AudioRecord record;
    private AudioEffect ns;
    private AudioEffect agc;
    private AudioEffect aec;

    /** {@code ns}, {@code agc}, {@code aec}: null = leave the platform default, true/false = force. */
    MicCapture(String sourceName, int channels, Boolean nsOn, Boolean agcOn, Boolean aecOn) {
        this.sourceName = sourceName;
        this.channels = channels;
        int source = sourceId(sourceName);
        int mask = channels == 2 ? AudioFormat.CHANNEL_IN_STEREO : AudioFormat.CHANNEL_IN_MONO;
        AudioFormat format = new AudioFormat.Builder()
                .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
                .setSampleRate(SAMPLE_RATE)
                .setChannelMask(mask)
                .build();
        AudioRecord.Builder builder = new AudioRecord.Builder();
        builder.setContext(FakeContext.get());
        builder.setAudioSource(source);
        builder.setAudioFormat(format);
        int min = AudioRecord.getMinBufferSize(SAMPLE_RATE, mask, AudioFormat.ENCODING_PCM_16BIT);
        if (min > 0) {
            builder.setBufferSizeInBytes(8 * min); // capacity only; it does not add latency
        }
        record = builder.build();
        if (record.getState() != AudioRecord.STATE_INITIALIZED) {
            record.release();
            throw new IllegalStateException("AudioRecord did not initialise for source " + sourceName);
        }
        int session = record.getAudioSessionId();
        ns = attach("ns", NoiseSuppressor.isAvailable() ? NoiseSuppressor.create(session) : null, nsOn);
        agc = attach("agc", AutomaticGainControl.isAvailable() ? AutomaticGainControl.create(session) : null, agcOn);
        aec = attach("aec", AcousticEchoCanceler.isAvailable() ? AcousticEchoCanceler.create(session) : null, aecOn);
    }

    private static AudioEffect attach(String name, AudioEffect effect, Boolean want) {
        if (effect == null) {
            Log.i("effect " + name + ": not available");
            return null;
        }
        boolean was = effect.getEnabled();
        if (want != null) {
            effect.setEnabled(want);
        }
        Log.i("effect " + name + ": default=" + (was ? "on" : "off") + " now=" + (effect.getEnabled() ? "on" : "off")
                + " impl=" + effect.getDescriptor().implementor + " / " + effect.getDescriptor().name);
        return effect;
    }

    void start() {
        record.startRecording();
        AudioDeviceInfo dev = record.getRoutedDevice();
        Log.i("recording: source=" + sourceName + " channels=" + channels + " rate=" + record.getSampleRate()
                + " routed=" + (dev == null ? "?" : dev.getProductName() + " type " + dev.getType()));
        try {
            List<MicrophoneInfo> mics = record.getActiveMicrophones();
            for (MicrophoneInfo m : mics) {
                Log.i("  active mic: " + describe(m));
            }
        } catch (IOException e) {
            Log.d("getActiveMicrophones failed: " + e.getMessage());
        }
    }

    /**
     * True while Android feeds this recording silence instead of the mic: another app with a higher claim took it
     * (a phone call, an assistant), or a privacy rule applies. The stream keeps running and recovers by itself.
     */
    boolean silenced() {
        try {
            AudioRecordingConfiguration c = record.getActiveRecordingConfiguration();
            return c != null && c.isClientSilenced();
        } catch (Throwable t) {
            return false;
        }
    }

    /** Blocking read of interleaved 16-bit samples. Returns the count read, or a negative AudioRecord error. */
    int read(short[] buf, int count) {
        return record.read(buf, 0, count);
    }

    void release() {
        for (AudioEffect e : new AudioEffect[] {ns, agc, aec}) {
            if (e != null) {
                e.release();
            }
        }
        record.release();
    }

    static String describe(MicrophoneInfo m) {
        MicrophoneInfo.Coordinate3F pos = m.getPosition();
        String where = pos == null ? "?" : String.format("(%.3f,%.3f,%.3f)", pos.x, pos.y, pos.z);
        return "id=" + m.getId() + " \"" + m.getDescription() + "\" location=" + m.getLocation()
                + " directionality=" + m.getDirectionality() + " position=" + where
                + " sensitivity=" + m.getSensitivity() + "dBFS maxSpl=" + m.getMaxSpl();
    }

    static int sourceId(String name) {
        switch (name) {
            case "mic":
                return MediaRecorder.AudioSource.MIC;
            case "camcorder":
                return MediaRecorder.AudioSource.CAMCORDER;
            case "voice-recognition":
                return MediaRecorder.AudioSource.VOICE_RECOGNITION;
            case "voice-communication":
                return MediaRecorder.AudioSource.VOICE_COMMUNICATION;
            case "unprocessed":
                return MediaRecorder.AudioSource.UNPROCESSED;
            case "voice-performance":
                return MediaRecorder.AudioSource.VOICE_PERFORMANCE;
            default:
                throw new IllegalArgumentException("unknown source: " + name);
        }
    }
}
