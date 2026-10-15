package com.truephonelink.server;

/** stdout goes back over `adb shell`, so plain prints are the log. Debug lines only with debug=1. */
final class Log {
    static boolean debug;

    private Log() {
    }

    static void i(String msg) {
        System.out.println(msg);
        System.out.flush();
    }

    static void d(String msg) {
        if (debug) {
            System.out.println("[debug] " + msg);
            System.out.flush();
        }
    }

    static void e(String msg) {
        System.err.println("ERROR: " + msg);
        System.err.flush();
    }
}
