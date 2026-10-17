package com.example.tplspike;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.provider.Settings;

/** BOOT_COMPLETED: start the link as connectedDevice; the service then tries the mic with no tap (S1). */
public class BootReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context c, Intent intent) {
        L.init(c);
        L.i("BOOT receiver action=" + intent.getAction());
        // Test rig only (Ethan approved in chat, 17 Oct): on Claude's own test phone, switch Wireless debugging back
        // on after a reboot so the spike can be read without a tap. Needs `pm grant ... WRITE_SECURE_SETTINGS`;
        // it goes away with the uninstall, and the product never does this.
        if (c.checkSelfPermission("android.permission.WRITE_SECURE_SETTINGS") == PackageManager.PERMISSION_GRANTED) {
            try {
                Settings.Global.putInt(c.getContentResolver(), "adb_wifi_enabled", 1);
                L.i("rig: adb_wifi_enabled=1 set");
            } catch (Throwable t) {
                L.e("rig: adb_wifi_enabled", t);
            }
        }
        SpikeService.start(c, null, "boot");
    }
}
