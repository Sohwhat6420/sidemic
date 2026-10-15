/*
 * Derived from scrcpy (https://github.com/Genymobile/scrcpy), Apache-2.0:
 * server/src/main/java/com/genymobile/scrcpy/FakeContext.java
 * Copyright (C) 2018 Genymobile, Copyright (C) 2018-2026 Romain Vimont.
 * Modified for TRUE Phone Link: no content resolver or clipboard hooks (the mic server needs neither).
 */
package com.truephonelink.server;

import android.content.AttributionSource;
import android.content.Context;
import android.content.ContextWrapper;
import android.os.Process;

/** A Context that tells the audio framework "I am com.android.shell, uid 2000", which holds RECORD_AUDIO. */
final class FakeContext extends ContextWrapper {

    static final String PACKAGE_NAME = "com.android.shell";

    private static FakeContext instance;

    static synchronized FakeContext get() {
        if (instance == null) {
            instance = new FakeContext();
        }
        return instance;
    }

    private FakeContext() {
        super(Workarounds.getSystemContext());
    }

    @Override
    public String getPackageName() {
        return PACKAGE_NAME;
    }

    @Override
    public String getOpPackageName() {
        return PACKAGE_NAME;
    }

    @Override
    public AttributionSource getAttributionSource() {
        AttributionSource.Builder builder = new AttributionSource.Builder(Process.SHELL_UID);
        builder.setPackageName(PACKAGE_NAME);
        return builder.build();
    }

    @Override
    public int getDeviceId() {
        return 0;
    }

    @Override
    public Context getApplicationContext() {
        return this;
    }

    @Override
    public Context createPackageContext(String packageName, int flags) {
        return this;
    }
}
