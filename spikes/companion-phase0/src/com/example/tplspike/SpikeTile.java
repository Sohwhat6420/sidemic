package com.example.tplspike;

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

    @Override
    public void onClick() {
        L.init(this);
        L.i("TILE click locked=" + isLocked());
        SpikeService.start(this, SpikeService.ACT_MIC_ON, "tile");
    }
}
