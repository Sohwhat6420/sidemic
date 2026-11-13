package com.truephonelink.server;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.InetAddress;
import java.net.ServerSocket;
import java.net.Socket;
import java.nio.charset.StandardCharsets;
import java.util.Arrays;

/**
 * Runs the phone's handshake code (Auth and Gate, the same classes the phone runs) on a desktop JVM. No test
 * framework and no Android SDK: javac and java are enough. crates/auth/tests/phone_gate.rs builds and drives it.
 *
 *   GateTest check                         unit checks of Auth; exits 1 and says why if one fails
 *   GateTest serve CHANNEL DEADLINE_MS     reads the session key from standard input exactly as the phone server
 *                                          does, listens on a free loopback port behind a Gate, prints "port N",
 *                                          and sends STREAM to each connection that proves the key
 */
public final class GateTest {
    /** What a proven connection receives: stands in for the mic audio. */
    static final String STREAM = "sidemic-test-stream\n";

    private static int failures;

    private GateTest() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && "check".equals(args[0])) {
            check();
            if (failures > 0) {
                System.out.println(failures + " check(s) failed");
                System.exit(1);
            }
            System.out.println("all checks passed");
        } else if (args.length == 3 && "serve".equals(args[0])) {
            serve(args[1], Integer.parseInt(args[2]));
        } else {
            System.err.println("usage: GateTest check | GateTest serve mic|ctl DEADLINE_MS");
            System.exit(2);
        }
    }

    // ---------------------------------------------------------------------------------------------------------
    // serve

    private static void serve(String channel, int deadlineMs) throws IOException {
        byte[] key = Auth.readKey(System.in);
        final ServerSocket server = new ServerSocket(0, 50, InetAddress.getLoopbackAddress());
        System.out.println("port " + server.getLocalPort());
        System.out.flush();
        new Gate<Socket>(key, channel, deadlineMs) {
            @Override
            Socket accept() throws IOException {
                return server.accept();
            }

            @Override
            InputStream in(Socket client) throws IOException {
                return client.getInputStream();
            }

            @Override
            OutputStream out(Socket client) throws IOException {
                return client.getOutputStream();
            }

            @Override
            void timeout(Socket client, int ms) throws IOException {
                client.setSoTimeout(ms);
            }

            @Override
            void close(Socket client) {
                try {
                    client.close();
                } catch (IOException ignored) {
                    // already closed
                }
            }

            @Override
            void serve(Socket client) {
                try {
                    OutputStream out = client.getOutputStream();
                    out.write(STREAM.getBytes(StandardCharsets.US_ASCII));
                    out.flush();
                    // Stay until the other end hangs up, as the mic stream does.
                    while (client.getInputStream().read() >= 0) {
                        // nothing is expected
                    }
                } catch (IOException e) {
                    // the other end went away
                }
            }
        }.run();
    }

    // ---------------------------------------------------------------------------------------------------------
    // check

    private static void expect(boolean ok, String what) {
        if (!ok) {
            failures++;
            System.out.println("FAILED: " + what);
        }
    }

    private static byte[] unhex(String s) {
        byte[] b = new byte[s.length() / 2];
        for (int i = 0; i < b.length; i++) {
            b[i] = (byte) Integer.parseInt(s.substring(2 * i, 2 * i + 2), 16);
        }
        return b;
    }

    private static String hex(byte[] b) {
        StringBuilder sb = new StringBuilder();
        for (byte x : b) {
            sb.append(String.format("%02x", x & 0xff));
        }
        return sb.toString();
    }

    private static byte[] range(int from, int n) {
        byte[] b = new byte[n];
        for (int i = 0; i < n; i++) {
            b[i] = (byte) (from + i);
        }
        return b;
    }

    private static byte[] join(byte[]... parts) {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        for (byte[] p : parts) {
            out.write(p, 0, p.length);
        }
        return out.toByteArray();
    }

    private static long soon() {
        return System.nanoTime() + 5_000_000_000L;
    }

    /** Whether readKey turns this input down. A message that repeats the input would be a leak, so that fails too. */
    private static boolean rejectsKey(String input) {
        try {
            Auth.readKey(new ByteArrayInputStream(input.getBytes(StandardCharsets.US_ASCII)));
            return false;
        } catch (IOException e) {
            String line = input.trim();
            return line.length() < 8 || !String.valueOf(e.getMessage()).contains(line.substring(0, 8));
        }
    }

    private static void check() throws IOException {
        final byte[] key = range(0x00, 32);
        final byte[] ns = range(0xa0, 16);
        final byte[] nc = range(0xb0, 16);
        // The same four values are in crates/auth/src/lib.rs, worked out with a third implementation (Python).
        final String pcMic = "a5fa245ae4a50642c62841eae9c8662d79e0fe345173d3d99ee2a97da223a224";
        final String pcCtl = "e3ea7731e2ab5bf40ffe4776e51ee8f0dcb91ebbc2b73281e99886bb72589068";
        final String phoneMic = "ab16aa6088a3c9c18d8e269c1a4b5049d6e7215a593bc947c912e07c0a29a07b";
        final String phoneCtl = "4be009072acc3544e0d64f0a8b2cf003c5a2a644b3f1cbeccf6b0da2fd673702";

        expect(hex(Auth.pcProof(key, Auth.MIC, ns, nc)).equals(pcMic), "the laptop's mic proof matches the shared vector");
        expect(hex(Auth.pcProof(key, Auth.CTL, ns, nc)).equals(pcCtl), "the laptop's control proof matches the shared vector");
        expect(hex(Auth.phoneProof(key, Auth.MIC, ns, nc)).equals(phoneMic), "the phone's mic proof matches the shared vector");
        expect(hex(Auth.phoneProof(key, Auth.CTL, ns, nc)).equals(phoneCtl), "the phone's control proof matches the shared vector");

        // The right proof: accepted, and the phone answers with its challenge and then its own proof, nothing else.
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        boolean ok = Auth.accept(new ByteArrayInputStream(join(nc, unhex(pcMic))), out, key, Auth.MIC, ns, soon());
        expect(ok, "the right proof is accepted");
        expect(Arrays.equals(out.toByteArray(), join(ns, unhex(phoneMic))), "the phone sends Ns, then its proof");

        // Wrong proofs: refused, and after its challenge the phone sends nothing at all.
        byte[] flipped = unhex(pcMic);
        flipped[31] ^= 1;
        byte[] firstByte = unhex(pcMic);
        firstByte[0] ^= (byte) 0x80;
        byte[][] wrong = {
            flipped, firstByte, new byte[32], unhex(pcCtl), unhex(phoneMic), Auth.pcProof(range(0x01, 32), Auth.MIC, ns, nc),
        };
        String[] names = {
            "a proof with its last bit flipped", "a proof with its first bit flipped", "an all-zero proof",
            "the proof for the control socket", "the phone's own proof played back", "a proof made with another key",
        };
        for (int i = 0; i < wrong.length; i++) {
            out = new ByteArrayOutputStream();
            ok = Auth.accept(new ByteArrayInputStream(join(nc, wrong[i])), out, key, Auth.MIC, ns, soon());
            expect(!ok, names[i] + " is refused");
            expect(Arrays.equals(out.toByteArray(), ns), names[i] + " gets nothing after the challenge");
        }

        // A proof for an earlier challenge (a recording played back) fails against a new one.
        out = new ByteArrayOutputStream();
        ok = Auth.accept(new ByteArrayInputStream(join(nc, unhex(pcMic))), out, key, Auth.MIC, range(0x10, 16), soon());
        expect(!ok, "a proof recorded from an earlier connection is refused");

        // Too little, or nothing: an error (the Gate turns it into a refusal), never an accept.
        byte[][] shortInputs = {new byte[0], nc, join(nc, Arrays.copyOf(unhex(pcMic), 31))};
        for (byte[] input : shortInputs) {
            out = new ByteArrayOutputStream();
            boolean threw = false;
            try {
                Auth.accept(new ByteArrayInputStream(input), out, key, Auth.MIC, ns, soon());
            } catch (IOException e) {
                threw = true;
            }
            expect(threw, "an answer of " + input.length + " bytes is turned away");
            expect(Arrays.equals(out.toByteArray(), ns), "an answer of " + input.length + " bytes gets nothing after the challenge");
        }

        // A sender that is still trickling when the deadline has passed is cut off.
        boolean threw = false;
        try {
            InputStream trickle = new InputStream() {
                @Override
                public int read() {
                    return 0;
                }

                @Override
                public int read(byte[] b, int off, int len) {
                    b[off] = 0;
                    return 1;
                }
            };
            Auth.accept(trickle, new ByteArrayOutputStream(), key, Auth.MIC, ns, System.nanoTime() - 1);
        } catch (IOException e) {
            threw = true;
        }
        expect(threw, "a sender past the deadline is cut off");

        // same(): equal, unequal anywhere, and different lengths.
        expect(Auth.same(unhex(pcMic), unhex(pcMic)), "same() accepts equal bytes");
        expect(!Auth.same(unhex(pcMic), flipped) && !Auth.same(unhex(pcMic), firstByte), "same() rejects a one-bit difference");
        expect(!Auth.same(unhex(pcMic), Arrays.copyOf(unhex(pcMic), 31)), "same() rejects a shorter proof");
        expect(Auth.same(new byte[0], new byte[0]), "same() accepts two empty strings");

        // readKey(): one line of exactly 64 hex digits, as crates/auth writes it.
        String line = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        expect(Arrays.equals(Auth.readKey(new ByteArrayInputStream((line + "\n").getBytes(StandardCharsets.US_ASCII))), key),
                "readKey reads the laptop's line");
        expect(Arrays.equals(Auth.readKey(new ByteArrayInputStream((line.toUpperCase() + "\r\n").getBytes(StandardCharsets.US_ASCII))), key),
                "readKey takes capitals and a Windows line ending");
        expect(Arrays.equals(Auth.readKey(new ByteArrayInputStream(line.getBytes(StandardCharsets.US_ASCII))), key),
                "readKey takes a line that ends with the input");
        expect(rejectsKey(""), "no input is not a key");
        expect(rejectsKey("\n"), "an empty line is not a key");
        expect(rejectsKey(line.substring(2) + "\n"), "62 digits are not a key");
        expect(rejectsKey(line + "00\n"), "66 digits are not a key");
        expect(rejectsKey(line.substring(0, 62) + "zz\n"), "other letters are not a key");
        expect(rejectsKey(line.substring(0, 32) + "\n" + line.substring(32) + "\n"), "a key split over two lines is not a key");
        expect(rejectsKey(new String(new char[5000]).replace('\0', 'a')), "a very long line is not a key");
    }
}
