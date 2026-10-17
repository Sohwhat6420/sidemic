package com.example.tplspike;

import android.app.PendingIntent;
import android.content.Intent;
import android.os.Build;
import android.service.quicksettings.Tile;
import android.service.quicksettings.TileService;

/** Route (c): does a tile tap arm the mic from the background? (S2) */
public class SpikeTile extends TileService {
    @Override
    public void onStartListening() {
        Tile t = getQsTile();
        if (t == null) return;
        t.setState(SpikeService.instance == null ? Tile.STATE_INACTIVE : Tile.STATE_ACTIVE);
        t.updateTile();
    }

    /** Which tile route to test: false = start the service directly (c), true = open TileArmActivity (c2). */
    static volatile boolean viaActivity = true;

    @Override
    public void onClick() {
        L.init(this);
        L.i("TILE click locked=" + isLocked() + " viaActivity=" + viaActivity);
        if (!viaActivity) {
            SpikeService.start(this, SpikeService.ACT_MIC_ON, "tile");
            return;
        }
        Intent i = new Intent(this, TileArmActivity.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        if (Build.VERSION.SDK_INT >= 34) {
            startActivityAndCollapse(PendingIntent.getActivity(this, 7, i, PendingIntent.FLAG_IMMUTABLE));
        } else {
            startActivityAndCollapse(i);
        }
    }
}
