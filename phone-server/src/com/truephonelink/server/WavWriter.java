package com.truephonelink.server;

import java.io.IOException;
import java.io.RandomAccessFile;

/** Minimal 16-bit PCM WAV writer; the header sizes are patched on close. */
final class WavWriter implements AutoCloseable {
    private final RandomAccessFile file;
    private final int channels;
    private final int rate;
    private long dataBytes;
    private byte[] scratch = new byte[0];

    WavWriter(String path, int channels, int rate) throws IOException {
        this.file = new RandomAccessFile(path, "rw");
        this.channels = channels;
        this.rate = rate;
        file.setLength(0);
        file.write(new byte[44]);
    }

    void write(short[] samples, int count) throws IOException {
        if (scratch.length < count * 2) {
            scratch = new byte[count * 2];
        }
        for (int i = 0; i < count; i++) {
            scratch[2 * i] = (byte) samples[i];
            scratch[2 * i + 1] = (byte) (samples[i] >> 8);
        }
        file.write(scratch, 0, count * 2);
        dataBytes += count * 2L;
    }

    @Override
    public void close() throws IOException {
        file.seek(0);
        file.writeBytes("RIFF");
        writeLe32((int) (36 + dataBytes));
        file.writeBytes("WAVEfmt ");
        writeLe32(16);
        writeLe16(1);
        writeLe16(channels);
        writeLe32(rate);
        writeLe32(rate * channels * 2);
        writeLe16(channels * 2);
        writeLe16(16);
        file.writeBytes("data");
        writeLe32((int) dataBytes);
        file.close();
    }

    private void writeLe32(int v) throws IOException {
        file.write(new byte[] {(byte) v, (byte) (v >> 8), (byte) (v >> 16), (byte) (v >> 24)});
    }

    private void writeLe16(int v) throws IOException {
        file.write(new byte[] {(byte) v, (byte) (v >> 8)});
    }
}
