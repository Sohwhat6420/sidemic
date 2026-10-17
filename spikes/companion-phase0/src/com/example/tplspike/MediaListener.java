package com.example.tplspike;

import android.service.notification.NotificationListenerService;

/** Exists only so Notification access can be granted; it reads nothing (S7). */
public class MediaListener extends NotificationListenerService {
    @Override
    public void onListenerConnected() {
        L.init(this);
        L.i("notification listener CONNECTED");
    }

    @Override
    public void onListenerDisconnected() {
        L.i("notification listener DISCONNECTED");
    }
}
