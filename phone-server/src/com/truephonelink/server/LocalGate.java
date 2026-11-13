package com.truephonelink.server;

import android.net.LocalServerSocket;
import android.net.LocalSocket;

import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;

/** A {@link Gate} on one of the phone's abstract sockets (localabstract:NAME, which adb forwards to the laptop). */
abstract class LocalGate extends Gate<LocalSocket> {
    private final LocalServerSocket server;

    LocalGate(String name, byte[] key, String channel) throws IOException {
        super(key, channel, Auth.DEADLINE_MS);
        server = new LocalServerSocket(name);
    }

    @Override
    final LocalSocket accept() throws IOException {
        return server.accept();
    }

    @Override
    final InputStream in(LocalSocket client) throws IOException {
        return client.getInputStream();
    }

    @Override
    final OutputStream out(LocalSocket client) throws IOException {
        return client.getOutputStream();
    }

    @Override
    final void timeout(LocalSocket client, int ms) throws IOException {
        client.setSoTimeout(ms);
    }

    @Override
    final void close(LocalSocket client) {
        // Closing alone doesn't wake a thread blocked in read on Linux; shutting the socket down does.
        try {
            client.shutdownInput();
        } catch (IOException ignored) {
            // already closed
        }
        try {
            client.shutdownOutput();
        } catch (IOException ignored) {
            // already closed
        }
        try {
            client.close();
        } catch (IOException ignored) {
            // already closed
        }
    }
}
