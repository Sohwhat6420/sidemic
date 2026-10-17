package com.example.tplspike;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** BOOT_COMPLETED: start the link as connectedDevice; the service then tries the mic with no tap (S1). */
public class BootReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context c, Intent intent) {
        L.init(c);
        L.i("BOOT receiver action=" + intent.getAction());
        SpikeService.start(c, null, "boot");
    }
}
