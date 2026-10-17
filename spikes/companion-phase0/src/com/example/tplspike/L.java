package com.example.tplspike;

import android.content.Context;
import android.os.SystemClock;
import android.util.Log;

import java.io.File;
import java.io.FileWriter;
import java.text.SimpleDateFormat;
import java.util.Date;
import java.util.Locale;

/** One log for the whole spike: logcat tag TPLSPIKE plus files/spike.log (read with run-as). */
final class L {
    private static File file;
    private static final SimpleDateFormat FMT = new SimpleDateFormat("MM-dd HH:mm:ss.SSS", Locale.UK);

    static synchronized void init(Context c) {
        if (file == null) file = new File(c.getFilesDir(), "spike.log");
    }

    static synchronized void i(String msg) {
        String line = FMT.format(new Date()) + " up=" + SystemClock.elapsedRealtime() / 1000 + " " + msg;
        Log.i("TPLSPIKE", msg);
        if (file == null) return;
        try (FileWriter w = new FileWriter(file, true)) {
            w.write(line);
            w.write('\n');
        } catch (Exception ignored) {
        }
    }

    /** The last n lines of spike.log, so the laptop can read it over the link when adb is down. */
    static synchronized String tail(int n) {
        if (file == null) return "(no log)";
        try {
            java.util.List<String> all = java.nio.file.Files.readAllLines(file.toPath());
            return String.join("\n", all.subList(Math.max(0, all.size() - n), all.size()));
        } catch (Exception e) {
            return "(log read failed: " + e + ")";
        }
    }

    static void e(String msg, Throwable t) {
        i(msg + " !! " + t.getClass().getName() + ": " + t.getMessage());
    }
}
