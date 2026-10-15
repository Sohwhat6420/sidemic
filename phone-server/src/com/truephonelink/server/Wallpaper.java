package com.truephonelink.server;

import android.app.WallpaperColors;
import android.app.WallpaperInfo;
import android.app.WallpaperManager;
import android.graphics.Bitmap;
import android.graphics.BitmapFactory;
import android.graphics.Canvas;
import android.graphics.Color;
import android.graphics.drawable.BitmapDrawable;
import android.graphics.drawable.Drawable;
import android.os.ParcelFileDescriptor;

import java.io.FileOutputStream;
import java.io.IOException;

/**
 * Reads the phone's wallpaper, so the laptop can draw the phone the way Windows Phone Link does: with its own
 * picture on it. Read-only. Live wallpapers have no bitmap, so it falls back to the wallpaper's colours.
 */
final class Wallpaper {

    private Wallpaper() {
    }

    /**
     * Keeps {out}-lock.png current: exports it now, then checks the wallpaper id every {@code everyMs} and
     * re-exports when it changes, printing "wallpaper-changed lock id=N" for the laptop to pick up.
     * The id check is one binder call, so this costs nothing while the wallpaper stays the same.
     */
    static void watch(String out, long everyMs) throws IOException, InterruptedException {
        WallpaperManager wm = WallpaperManager.getInstance(FakeContext.get());
        int last = Integer.MIN_VALUE;
        while (true) {
            int id = wm.getWallpaperId(WallpaperManager.FLAG_LOCK);
            if (id != last) {
                if (export(wm, WallpaperManager.FLAG_LOCK, out + "-lock.png")) {
                    System.out.println("wallpaper-changed lock id=" + id);
                    System.out.flush();
                }
                last = id;
            }
            Thread.sleep(everyMs);
        }
    }

    /** A small JPEG of one wallpaper (lock or home) for the laptop UI, or null when the phone won't hand it over. */
    static byte[] jpeg(WallpaperManager wm, int which) throws IOException {
        ParcelFileDescriptor pfd = wm.getWallpaperFile(which);
        if (pfd == null) {
            return null;
        }
        Bitmap b;
        try {
            b = BitmapFactory.decodeFileDescriptor(pfd.getFileDescriptor());
        } finally {
            pfd.close();
        }
        if (b == null) {
            return null;
        }
        int w = 360;
        int h = Math.round(w * (float) b.getHeight() / b.getWidth());
        java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
        Bitmap.createScaledBitmap(b, w, h, true).compress(Bitmap.CompressFormat.JPEG, 88, out);
        return out.toByteArray();
    }

    /** Writes a thumbnail of one wallpaper (lock or home). Returns false when the phone won't hand it over. */
    static boolean export(WallpaperManager wm, int which, String path) throws IOException {
        ParcelFileDescriptor pfd = wm.getWallpaperFile(which);
        if (pfd == null) {
            return false;
        }
        Bitmap b;
        try {
            b = BitmapFactory.decodeFileDescriptor(pfd.getFileDescriptor());
        } finally {
            pfd.close();
        }
        if (b == null) {
            return false;
        }
        save(b, path);
        return true;
    }

    /** Tries each route for the lock and home wallpapers, writing {out}-lock.png / {out}-home.png when one works. */
    static void probe(String out) {
        WallpaperManager wm = WallpaperManager.getInstance(FakeContext.get());
        int[] which = {WallpaperManager.FLAG_LOCK, WallpaperManager.FLAG_SYSTEM};
        String[] name = {"lock", "home"};
        for (int i = 0; i < which.length; i++) {
            String n = name[i];
            try {
                WallpaperInfo info = which[i] == WallpaperManager.FLAG_LOCK
                        ? wm.getWallpaperInfo(WallpaperManager.FLAG_LOCK) : wm.getWallpaperInfo();
                Log.i(n + " live wallpaper: " + (info == null ? "none (static image)" : info.getComponent().flattenToShortString()));
            } catch (Throwable t) {
                Log.i(n + " getWallpaperInfo: " + t);
            }
            try {
                WallpaperColors c = wm.getWallpaperColors(which[i]);
                Log.i(n + " colours: " + (c == null ? "null" : hex(c.getPrimaryColor()) + " " + hex(c.getSecondaryColor())
                        + " " + hex(c.getTertiaryColor()) + " hints=" + c.getColorHints()));
            } catch (Throwable t) {
                Log.i(n + " getWallpaperColors: " + t);
            }
            try {
                ParcelFileDescriptor pfd = wm.getWallpaperFile(which[i]);
                Log.i(n + " getWallpaperFile: " + (pfd == null ? "null" : "fd ok, " + pfd.getStatSize() + " bytes"));
                if (pfd != null) {
                    // Samsung's live wallpapers keep the picture they animate here, so this works for them too.
                    Bitmap b = BitmapFactory.decodeFileDescriptor(pfd.getFileDescriptor());
                    pfd.close();
                    if (b != null) {
                        Log.i(n + " picture: " + b.getWidth() + "x" + b.getHeight());
                        save(b, out + "-" + n + ".png");
                        continue;
                    }
                }
            } catch (Throwable t) {
                Log.i(n + " getWallpaperFile: " + t);
            }
            try {
                Drawable d = wm.getDrawable(which[i]);
                Log.i(n + " getDrawable: " + (d == null ? "null" : d.getIntrinsicWidth() + "x" + d.getIntrinsicHeight()));
                if (d != null) {
                    save(d, out + "-" + n + ".png");
                }
            } catch (Throwable t) {
                Log.i(n + " getDrawable: " + t);
            }
        }
    }

    private static void save(Drawable d, String path) throws IOException {
        Bitmap b;
        if (d instanceof BitmapDrawable) {
            b = ((BitmapDrawable) d).getBitmap();
        } else {
            b = Bitmap.createBitmap(Math.max(1, d.getIntrinsicWidth()), Math.max(1, d.getIntrinsicHeight()), Bitmap.Config.ARGB_8888);
            Canvas c = new Canvas(b);
            d.setBounds(0, 0, c.getWidth(), c.getHeight());
            d.draw(c);
        }
        save(b, path);
    }

    private static void save(Bitmap b, String path) throws IOException {
        // A thumbnail is all the laptop needs: the phone picture in the UI is about 120 px wide.
        int w = 360;
        int h = Math.round(w * (float) b.getHeight() / b.getWidth());
        Bitmap small = Bitmap.createScaledBitmap(b, w, h, true);
        try (FileOutputStream f = new FileOutputStream(path)) {
            small.compress(Bitmap.CompressFormat.PNG, 100, f);
        }
        Log.i("wrote " + path + " (" + w + "x" + h + ")");
    }

    private static String hex(Color c) {
        return c == null ? "-" : String.format("#%06X", c.toArgb() & 0xFFFFFF);
    }
}
