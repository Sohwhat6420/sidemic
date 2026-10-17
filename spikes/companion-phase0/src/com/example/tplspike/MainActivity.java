package com.example.tplspike;

import android.app.Activity;
import android.companion.AssociationInfo;
import android.companion.AssociationRequest;
import android.companion.BluetoothDeviceFilter;
import android.companion.CompanionDeviceManager;
import android.content.Intent;
import android.content.IntentSender;
import android.net.Uri;
import android.os.Bundle;
import android.provider.Settings;
import android.widget.Button;
import android.widget.LinearLayout;
import android.widget.ScrollView;
import android.widget.TextView;

import java.io.File;
import java.io.RandomAccessFile;

/** Route (d), opening the app, plus the setup buttons the spike needs. No polish: it is thrown away. */
public class MainActivity extends Activity {
    private TextView log;

    @Override
    protected void onCreate(Bundle b) {
        super.onCreate(b);
        L.init(this);
        L.i("activity onCreate");
        LinearLayout col = new LinearLayout(this);
        col.setOrientation(LinearLayout.VERTICAL);
        col.setPadding(32, 96, 32, 32);
        add(col, "Grant mic + notifications", () -> requestPermissions(new String[]{
                android.Manifest.permission.RECORD_AUDIO, android.Manifest.permission.POST_NOTIFICATIONS}, 1));
        add(col, "Mic on (app open)", () -> SpikeService.start(this, SpikeService.ACT_MIC_ON, "app"));
        add(col, "Mic off", () -> SpikeService.start(this, SpikeService.ACT_MIC_OFF, "app"));
        add(col, "Notification access page", () -> startActivity(new Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)));
        add(col, "Battery page (app info)", () -> startActivity(new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                Uri.parse("package:" + getPackageName()))));
        add(col, "Associate laptop (Companion Device Manager)", this::associate);
        log = new TextView(this);
        log.setTextSize(10);
        ScrollView sv = new ScrollView(this);
        sv.addView(log);
        col.addView(sv);
        setContentView(col);
        // Route (d) test: `am start ... --ez arm true` is "open the app and press Mic on" in one step.
        SpikeService.start(this, getIntent().getBooleanExtra("arm", false) ? SpikeService.ACT_MIC_ON : null, "app-open");
    }

    @Override
    protected void onResume() {
        super.onResume();
        L.i("activity onResume");
        log.setText(tail());
    }

    @Override
    protected void onPause() {
        L.i("activity onPause");
        super.onPause();
    }

    private void add(LinearLayout col, String label, Runnable r) {
        Button btn = new Button(this);
        btn.setText(label);
        btn.setAllCaps(false);
        btn.setOnClickListener(v -> {
            L.i("button " + label);
            r.run();
            log.setText(tail());
        });
        col.addView(btn);
    }

    private void associate() {
        CompanionDeviceManager cdm = getSystemService(CompanionDeviceManager.class);
        AssociationRequest req = new AssociationRequest.Builder()
                .addDeviceFilter(new BluetoothDeviceFilter.Builder().build())
                .setSingleDevice(false)
                .build();
        cdm.associate(req, getMainExecutor(), new CompanionDeviceManager.Callback() {
            @Override
            public void onAssociationPending(IntentSender sender) {
                try {
                    startIntentSenderForResult(sender, 42, null, 0, 0, 0);
                } catch (Exception e) {
                    L.e("cdm chooser", e);
                }
            }

            @Override
            public void onAssociationCreated(AssociationInfo info) {
                L.i("cdm ASSOCIATED id=" + info.getId() + " mac=" + info.getDeviceMacAddress() + " name=" + info.getDisplayName());
            }

            @Override
            public void onFailure(CharSequence error) {
                L.i("cdm FAILED " + error);
            }
        });
    }

    @Override
    protected void onActivityResult(int req, int res, Intent data) {
        L.i("activity result req=" + req + " res=" + res);
    }

    @Override
    public void onRequestPermissionsResult(int req, String[] perms, int[] results) {
        StringBuilder sb = new StringBuilder("permissions");
        for (int i = 0; i < perms.length; i++) sb.append(' ').append(perms[i]).append('=').append(results[i] == 0 ? "GRANTED" : "DENIED");
        L.i(sb.toString());
        log.setText(tail());
    }

    private String tail() {
        try (RandomAccessFile f = new RandomAccessFile(new File(getFilesDir(), "spike.log"), "r")) {
            long len = f.length();
            long from = Math.max(0, len - 6000);
            f.seek(from);
            byte[] b = new byte[(int) (len - from)];
            f.readFully(b);
            return new String(b, "UTF-8");
        } catch (Exception e) {
            return "(no log yet)";
        }
    }
}
