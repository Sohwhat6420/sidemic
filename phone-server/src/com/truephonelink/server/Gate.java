package com.truephonelink.server;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.security.SecureRandom;
import java.util.ArrayDeque;

/**
 * The door on one of the phone's sockets: accepts connections for ever, and hands one to {@link #serve} only after
 * it has proved the session key (see {@link Auth}). Everything else is closed without being served.
 *
 * Each connection proves itself on its own thread, so one that connects and says nothing can't hold up the laptop
 * behind it. At most {@link #MAX_UNPROVEN} are kept waiting at once; one more closes the oldest, so silent
 * connections can't use up the phone's threads, and those already waiting can't keep the laptop out. (A program
 * that keeps opening connections faster than the laptop's handshake takes can still get it closed; the laptop
 * tries again, and no audio is lost to anyone.) Proven connections are served one at a time, in turn, as they
 * were before the handshake existed.
 *
 * {@code C} is the connection type: android.net.LocalSocket on the phone ({@link LocalGate}), java.net.Socket in
 * GateTest. Nothing here uses android.* classes, so the test runs this same code.
 */
abstract class Gate<C> {
    static final int MAX_UNPROVEN = 8;
    /** A burst of refusals is one log line, then at most one line this often. */
    private static final long REPORT_EVERY_NANOS = 10_000_000_000L;

    private final byte[] key;
    private final String channel;
    private final int deadlineMs;
    private final SecureRandom random = new SecureRandom();
    private final ArrayDeque<C> unproven = new ArrayDeque<C>();
    private final Object turn = new Object();
    private final Object report = new Object();
    private long refusals;
    private long reported;
    private long reportedAt;

    Gate(byte[] key, String channel, int deadlineMs) {
        this.key = key;
        this.channel = channel;
        this.deadlineMs = deadlineMs;
    }

    abstract C accept() throws IOException;

    abstract InputStream in(C client) throws IOException;

    abstract OutputStream out(C client) throws IOException;

    /** Sets the read (and, where the socket has one, write) timeout; 0 means none. */
    abstract void timeout(C client, int ms) throws IOException;

    /** Closes the connection so that a thread blocked reading it wakes up. Never throws. */
    abstract void close(C client);

    /** The stream itself, for a connection that proved the key. Called for one connection at a time. */
    abstract void serve(C client);

    /** Accepts until the listening socket fails. */
    final void run() throws IOException {
        while (true) {
            final C client = accept();
            C oldest = null;
            synchronized (unproven) {
                unproven.addLast(client);
                if (unproven.size() > MAX_UNPROVEN) {
                    oldest = unproven.removeFirst();
                }
            }
            if (oldest != null) {
                close(oldest); // its thread wakes, finds it unproven, and counts the refusal
            }
            // An anonymous class, not a lambda: the build compiles against android.jar, which has no LambdaMetafactory.
            Thread t = new Thread(new Runnable() {
                @Override
                public void run() {
                    admit(client);
                }
            }, "tpl-" + channel + "-door");
            t.setDaemon(true);
            t.start();
        }
    }

    private void admit(C client) {
        try {
            byte[] ns = new byte[Auth.NONCE_LEN];
            random.nextBytes(ns);
            byte[] nc = null;
            boolean kept;
            try {
                nc = verify(client, ns);
            } finally {
                // Leaving the waiting list is what makes a connection safe from being closed to make room. One
                // that is no longer on it was closed while it proved itself, so it is not served either.
                synchronized (unproven) {
                    kept = unproven.remove(client);
                }
            }
            if (nc == null || !kept) {
                refused();
                return;
            }
            // Only now, when nothing will close it to make room, is the other end told that it is in.
            Auth.confirm(out(client), key, channel, ns, nc);
            timeout(client, 0);
            synchronized (turn) {
                serve(client);
            }
        } catch (IOException e) {
            // went away between proving the key and being told so
        } catch (Throwable t) {
            // Under app_process an uncaught exception on any thread ends the whole server.
            Log.e(channel + ": " + t);
        } finally {
            close(client);
        }
    }

    /** The other end's nonce if it proved the key in time, else null. */
    private byte[] verify(C client, byte[] ns) {
        try {
            timeout(client, deadlineMs);
            long deadline = System.nanoTime() + deadlineMs * 1_000_000L;
            return Auth.verify(in(client), out(client), key, channel, ns, deadline);
        } catch (IOException e) {
            return null; // went away, said nothing in time, or was closed to make room
        }
    }

    /** Says that connections were refused, without a line per connection and without anything they sent. */
    private void refused() {
        synchronized (report) {
            refusals++;
            long now = System.nanoTime();
            if (reported == 0 || now - reportedAt >= REPORT_EVERY_NANOS) {
                long fresh = refusals - reported;
                Log.i(channel + ": refused " + fresh + (fresh == 1 ? " connection" : " connections")
                        + " that didn't prove this session's key (" + refusals + " since the server started)");
                reported = refusals;
                reportedAt = now;
            }
        }
    }
}
