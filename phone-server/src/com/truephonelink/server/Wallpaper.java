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
        Bitmap b = picture(wm, which);
        if (b == null) {
            return null;
        }
        int w = 360;
        int h = Math.round(w * (float) b.getHeight() / b.getWidth());
        java.io.ByteArrayOutputStream out = new java.io.ByteArrayOutputStream();
        Bitmap.createScaledBitmap(b, w, h, true).compress(Bitmap.CompressFormat.JPEG, 88, out);
        return out.toByteArray();
    }

    /**
     * The wallpaper's picture, by the first route that works:
     * 1. the wallpaper file (a static picture, or on Samsung the picture a live wallpaper animates);
     * 2. the phone's built-in wallpaper, while that is still the one in use (a fresh HyperOS phone keeps its
     *    default as a resource, so there is no file); checked by comparing its colours with the wallpaper's;
     * 3. a gradient of the wallpaper's own colours, so the phone picture at least looks like the phone.
     */
    static Bitmap picture(WallpaperManager wm, int which) throws IOException {
        ParcelFileDescriptor pfd = wm.getWallpaperFile(which);
        if (pfd != null) {
            try {
                Bitmap b = BitmapFactory.decodeFileDescriptor(pfd.getFileDescriptor());
                if (b != null) {
                    return b;
                }
            } finally {
                pfd.close();
            }
        }
        WallpaperColors colours = null;
        try {
            colours = wm.getWallpaperColors(which);
        } catch (Throwable t) {
            Log.d("wallpaper colours: " + t);
        }
        if (colours == null) {
            return null;
        }
        try {
            Drawable d = wm.getBuiltInDrawable(which);
            if (d != null && sameColour(WallpaperColors.fromDrawable(d).getPrimaryColor(), colours.getPrimaryColor())) {
                return bitmap(d);
            }
        } catch (Throwable t) {
            Log.d("built-in wallpaper: " + t);
        }
        return gradient(colours);
    }

    private static boolean sameColour(Color a, Color b) {
        int x = a.toArgb();
        int y = b.toArgb();
        return Math.abs(Color.red(x) - Color.red(y)) + Math.abs(Color.green(x) - Color.green(y))
                + Math.abs(Color.blue(x) - Color.blue(y)) < 24;
    }

    private static Bitmap gradient(WallpaperColors c) {
        int top = c.getPrimaryColor().toArgb();
        int bottom = (c.getSecondaryColor() != null ? c.getSecondaryColor() : c.getPrimaryColor()).toArgb();
        Bitmap b = Bitmap.createBitmap(360, 780, Bitmap.Config.ARGB_8888);
        Canvas canvas = new Canvas(b);
        android.graphics.Paint p = new android.graphics.Paint();
        p.setShader(new android.graphics.LinearGradient(0, 0, 0, 780, top, bottom, android.graphics.Shader.TileMode.CLAMP));
        canvas.drawRect(0, 0, 360, 780, p);
        return b;
    }

    private static Bitmap bitmap(Drawable d) {
        if (d instanceof BitmapDrawable && ((BitmapDrawable) d).getBitmap() != null) {
            return ((BitmapDrawable) d).getBitmap();
        }
        Bitmap b = Bitmap.createBitmap(Math.max(1, d.getIntrinsicWidth()), Math.max(1, d.getIntrinsicHeight()), Bitmap.Config.ARGB_8888);
        Canvas c = new Canvas(b);
        d.setBounds(0, 0, c.getWidth(), c.getHeight());
        d.draw(c);
        return b;
    }

    /** Writes a thumbnail of one wallpaper (lock or home). Returns false when the phone won't hand it over. */
    static boolean export(WallpaperManager wm, int which, String path) throws IOException {
        Bitmap b = picture(wm, which);
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
                    continue;
                }
            } catch (Throwable t) {
                Log.i(n + " getDrawable: " + t);
            }
            try {
                Drawable d = wm.getBuiltInDrawable(which[i]);
                Log.i(n + " built-in: " + (d == null ? "null" : d.getIntrinsicWidth() + "x" + d.getIntrinsicHeight()
                        + " colour " + hex(WallpaperColors.fromDrawable(d).getPrimaryColor())));
                Bitmap b = picture(wm, which[i]);
                if (b != null) {
                    Log.i(n + " fallback picture: " + b.getWidth() + "x" + b.getHeight());
                    save(b, out + "-" + n + ".png");
                }
            } catch (Throwable t) {
                Log.i(n + " built-in: " + t);
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
