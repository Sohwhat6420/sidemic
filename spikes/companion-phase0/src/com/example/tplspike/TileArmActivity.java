package com.example.tplspike;

import android.app.Activity;
import android.os.Bundle;

/** Route (c2): the tile opens this invisible activity, which arms the mic while it is on top, then closes. */
public class TileArmActivity extends Activity {
    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        L.init(this);
        L.i("tile-activity onCreate");
        SpikeService.start(this, SpikeService.ACT_MIC_ON, "tile-activity");
        finish();
    }
}
