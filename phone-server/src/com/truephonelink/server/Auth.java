package com.truephonelink.server;

import java.io.EOFException;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.nio.charset.StandardCharsets;
import java.security.GeneralSecurityException;
import java.util.Arrays;

import javax.crypto.Mac;
import javax.crypto.spec.SecretKeySpec;

/**
 * The phone's half of the per-session handshake on tpl_mic and tpl_ctl. The laptop's half, and the reasons, are in
 * crates/auth/src/lib.rs.
 *
 * The laptop makes a fresh 32-byte key each time it starts this server and writes it to the server's standard
 * input, through the adb shell session that starts it: 64 hex digits and a newline. It is never an argument (other
 * programs can read a command line) and it is never logged.
 *
 * On every connection, before anything else:
 *   phone to laptop: Ns, 16 random bytes
 *   laptop to phone: Nc, 16 random bytes, then HMAC-SHA-256(key, "sidemic/1 pc CHANNEL" || Ns || Nc)
 *   phone to laptop: HMAC-SHA-256(key, "sidemic/1 phone CHANNEL" || Ns || Nc), only if the laptop's was right
 * CHANNEL is "mic" or "ctl". A wrong, late or missing answer gets the connection closed with nothing more sent;
 * the mic is not opened for it.
 *
 * Plain Java (no android.* classes), so GateTest runs this same code on a desktop JVM.
 */
final class Auth {
    static final int KEY_LEN = 32;
    static final int NONCE_LEN = 16;
    static final int MAC_LEN = 32;
    static final String MIC = "mic";
    static final String CTL = "ctl";
    /** How long a connection has to prove the key before it is closed. */
    static final int DEADLINE_MS = 3000;

    private static final String PC = "pc";
    private static final String PHONE = "phone";

    private Auth() {
    }

    /**
     * Reads the session key from the server's standard input: one line of exactly 64 hex digits. Anything else
     * (no input, a short line, other characters) is an error, so a server started without a key serves nobody.
     * The error never repeats what was read.
     */
    static byte[] readKey(InputStream in) throws IOException {
        byte[] line = new byte[KEY_LEN * 2];
        int n = 0;
        boolean ok = true;
        try {
            while (true) {
                int c = in.read();
                if (c < 0 || c == '\n') {
                    break;
                }
                if (c == '\r') {
                    continue; // a Windows pipe may add one
                }
                if (n < line.length) {
                    line[n] = (byte) c;
                }
                n++;
                if (n > line.length * 2) {
                    break; // far too long to be a key; don't read for ever
                }
            }
            byte[] key = new byte[KEY_LEN];
            ok = n == line.length;
            for (int i = 0; ok && i < KEY_LEN; i++) {
                int hi = Character.digit(line[2 * i], 16);
                int lo = Character.digit(line[2 * i + 1], 16);
                ok = hi >= 0 && lo >= 0;
                key[i] = (byte) (hi << 4 | lo);
            }
            if (!ok) {
                Arrays.fill(key, (byte) 0);
                throw new IOException("no session key on standard input; refusing to serve. The laptop app sends one"
                        + " when it starts this server");
            }
            // A phone whose Java has no HMAC-SHA-256 should say so now, not refuse every connection later.
            mac(key, PC, MIC, new byte[NONCE_LEN], new byte[NONCE_LEN]);
            return key;
        } finally {
            Arrays.fill(line, (byte) 0);
        }
    }

    /**
     * The first two steps of the phone's half on a new connection: sends {@code ns}, then reads the other end's
     * nonce and proof, giving up at {@code deadlineNanos} on the System.nanoTime clock. Returns the other end's
     * nonce if its proof is right, or null if it is wrong; throws if the other end went away or ran out of time.
     * Nothing is sent after {@code ns} here: the phone's own proof is {@link #confirm}.
     *
     * No flush anywhere in this class: these are socket streams with no buffer of their own, and on Android a
     * LocalSocket's flush waits until the other end has read everything, which a silent connection never does.
     */
    static byte[] verify(InputStream in, OutputStream out, byte[] key, String channel, byte[] ns, long deadlineNanos)
            throws IOException {
        out.write(ns);
        byte[] reply = new byte[NONCE_LEN + MAC_LEN];
        readFully(in, reply, deadlineNanos);
        byte[] nc = Arrays.copyOfRange(reply, 0, NONCE_LEN);
        byte[] proof = Arrays.copyOfRange(reply, NONCE_LEN, reply.length);
        return same(proof, mac(key, PC, channel, ns, nc)) ? nc : null;
    }

    /** The last step: the phone's own proof, for a connection whose proof {@link #verify} found right. */
    static void confirm(OutputStream out, byte[] key, String channel, byte[] ns, byte[] nc) throws IOException {
        out.write(mac(key, PHONE, channel, ns, nc));
    }

    /** {@link #verify} then, if the proof was right, {@link #confirm}. Returns whether it was right. */
    static boolean accept(InputStream in, OutputStream out, byte[] key, String channel, byte[] ns, long deadlineNanos)
            throws IOException {
        byte[] nc = verify(in, out, key, channel, ns, deadlineNanos);
        if (nc == null) {
            return false;
        }
        confirm(out, key, channel, ns, nc);
        return true;
    }

    /** The laptop's proof, for GateTest. */
    static byte[] pcProof(byte[] key, String channel, byte[] ns, byte[] nc) {
        return mac(key, PC, channel, ns, nc);
    }

    /** The phone's proof, for GateTest. */
    static byte[] phoneProof(byte[] key, String channel, byte[] ns, byte[] nc) {
        return mac(key, PHONE, channel, ns, nc);
    }

    private static byte[] mac(byte[] key, String role, String channel, byte[] ns, byte[] nc) {
        try {
            Mac m = Mac.getInstance("HmacSHA256");
            m.init(new SecretKeySpec(key, "HmacSHA256"));
            m.update(("sidemic/1 " + role + " " + channel).getBytes(StandardCharsets.US_ASCII));
            m.update(ns);
            m.update(nc);
            return m.doFinal();
        } catch (GeneralSecurityException e) {
            throw new IllegalStateException("HMAC-SHA-256 isn't available on this phone: " + e);
        }
    }

    /**
     * Whether two byte strings are equal, taking the same time wherever they differ, so the time a refusal takes
     * tells a guesser nothing about how much of a proof was right.
     */
    static boolean same(byte[] a, byte[] b) {
        if (a.length != b.length) {
            return false;
        }
        int diff = 0;
        for (int i = 0; i < a.length; i++) {
            diff |= a[i] ^ b[i];
        }
        return diff == 0;
    }

    /** Fills {@code buf}, or throws: at end of input, on a read timeout, or once the deadline has passed. */
    private static void readFully(InputStream in, byte[] buf, long deadlineNanos) throws IOException {
        int n = 0;
        while (n < buf.length) {
            int r = in.read(buf, n, buf.length - n);
            if (r < 0) {
                throw new EOFException("closed before proving the session key");
            }
            n += r;
            // The socket's own timeout bounds each read; this bounds a sender that trickles one byte at a time.
            if (n < buf.length && System.nanoTime() - deadlineNanos > 0) {
                throw new IOException("too slow to prove the session key");
            }
        }
    }
}
