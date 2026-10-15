/*
 * Derived from scrcpy (https://github.com/Genymobile/scrcpy), Apache-2.0:
 * server/src/main/java/com/genymobile/scrcpy/Workarounds.java
 * Copyright (C) 2018 Genymobile, Copyright (C) 2018-2026 Romain Vimont.
 * Modified for TRUE Phone Link: trimmed to the parts the mic server needs.
 */
package com.truephonelink.server;

import android.app.Application;
import android.app.Instrumentation;
import android.content.Context;
import android.content.pm.ApplicationInfo;

import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * We run under app_process as the shell user (uid 2000), not as an app, so there is no ActivityThread, no
 * Application and no Context. AudioRecord needs a Context with a valid attribution source (Android 12+), so we
 * build just enough of an app environment by reflection.
 */
final class Workarounds {

    private static final Class<?> ACTIVITY_THREAD_CLASS;
    private static final Object ACTIVITY_THREAD;

    static {
        try {
            ACTIVITY_THREAD_CLASS = Class.forName("android.app.ActivityThread");
            Constructor<?> ctor = ACTIVITY_THREAD_CLASS.getDeclaredConstructor();
            ctor.setAccessible(true);
            ACTIVITY_THREAD = ctor.newInstance();

            Field current = ACTIVITY_THREAD_CLASS.getDeclaredField("sCurrentActivityThread");
            current.setAccessible(true);
            current.set(null, ACTIVITY_THREAD);

            Field systemThread = ACTIVITY_THREAD_CLASS.getDeclaredField("mSystemThread");
            systemThread.setAccessible(true);
            systemThread.setBoolean(ACTIVITY_THREAD, true);
        } catch (Exception e) {
            throw new AssertionError(e);
        }
    }

    private Workarounds() {
    }

    static void apply() {
        // Some Samsung builds need a ConfigurationController (Android 12+) before a system context can be made.
        fillConfigurationController();
        fillAppInfo();
        fillAppContext();
    }

    private static void fillAppInfo() {
        try {
            Class<?> appBindDataClass = Class.forName("android.app.ActivityThread$AppBindData");
            Constructor<?> ctor = appBindDataClass.getDeclaredConstructor();
            ctor.setAccessible(true);
            Object appBindData = ctor.newInstance();

            ApplicationInfo info = new ApplicationInfo();
            info.packageName = FakeContext.PACKAGE_NAME;

            Field appInfo = appBindDataClass.getDeclaredField("appInfo");
            appInfo.setAccessible(true);
            appInfo.set(appBindData, info);

            Field bound = ACTIVITY_THREAD_CLASS.getDeclaredField("mBoundApplication");
            bound.setAccessible(true);
            bound.set(ACTIVITY_THREAD, appBindData);
        } catch (Throwable t) {
            Log.d("Could not fill app info: " + t.getMessage());
        }
    }

    private static void fillAppContext() {
        try {
            Application app = Instrumentation.newApplication(Application.class, FakeContext.get());
            Field initial = ACTIVITY_THREAD_CLASS.getDeclaredField("mInitialApplication");
            initial.setAccessible(true);
            initial.set(ACTIVITY_THREAD, app);
        } catch (Throwable t) {
            Log.d("Could not fill app context: " + t.getMessage());
        }
    }

    private static void fillConfigurationController() {
        try {
            Class<?> controllerClass = Class.forName("android.app.ConfigurationController");
            Class<?> internalClass = Class.forName("android.app.ActivityThreadInternal");
            Constructor<?> ctor = controllerClass.getDeclaredConstructor(internalClass);
            ctor.setAccessible(true);
            Object controller = ctor.newInstance(ACTIVITY_THREAD);

            Field field = ACTIVITY_THREAD_CLASS.getDeclaredField("mConfigurationController");
            field.setAccessible(true);
            field.set(ACTIVITY_THREAD, controller);
        } catch (Throwable t) {
            Log.d("Could not fill configuration: " + t.getMessage());
        }
    }

    static Context getSystemContext() {
        try {
            Method method = ACTIVITY_THREAD_CLASS.getDeclaredMethod("getSystemContext");
            return (Context) method.invoke(ACTIVITY_THREAD);
        } catch (Throwable t) {
            Log.d("Could not get system context: " + t.getMessage());
            return null;
        }
    }
}
