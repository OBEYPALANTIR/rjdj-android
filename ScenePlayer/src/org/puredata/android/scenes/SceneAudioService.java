package org.puredata.android.scenes;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.media.AudioAttributes;
import android.media.AudioFocusRequest;
import android.media.AudioManager;
import android.os.Binder;
import android.os.Build;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;
import android.util.Log;
import java.io.BufferedInputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;

/** Foreground lifetime and audio focus; the Rust core owns Pd and the streams. */
public final class SceneAudioService extends Service {
    public final class SceneAudioBinder extends Binder {
        public SceneAudioService getService() {
            return SceneAudioService.this;
        }
    }

    private static final String TAG = "PD Service";
    private static final int NOTIFICATION_ID = 1;
    private static boolean abstractionsInstalled;
    private final SceneAudioBinder binder = new SceneAudioBinder();
    private boolean hasForeground;
    private volatile Runnable onAudioFocusLoss;
    private AudioManager audioManager;
    private AudioFocusRequest focus;

    @Override public void onCreate() {
        super.onCreate();
        audioManager = (AudioManager) getSystemService(AUDIO_SERVICE);
        focus = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                .setAudioAttributes(new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC).build())
                .setOnAudioFocusChangeListener(change -> {
                    if (change == AudioManager.AUDIOFOCUS_LOSS
                            || change == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT
                            || change == AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK) {
                        Runnable listener = onAudioFocusLoss;
                        if (listener != null) listener.run();
                        else { stopAudio(); abandonAudioFocus(); }
                    }
                }, new Handler(Looper.getMainLooper())).build();
        if (!abstractionsInstalled) {
            try {
                installAbstractions();
                abstractionsInstalled = true;
            } catch (IOException e) {
                Log.e(TAG, "unable to unpack abstractions:" + e.toString());
            }
        }
    }

    private void installAbstractions() throws IOException {
        File root = getFilesDir().getCanonicalFile();
        Path rootPath = root.toPath();
        byte[] buffer = new byte[2048];
        try (ZipInputStream zip = new ZipInputStream(new BufferedInputStream(
                getResources().openRawResource(R.raw.extra_abs)))) {
            ZipEntry entry;
            while ((entry = zip.getNextEntry()) != null) {
                File file = new File(root, entry.getName()).getCanonicalFile();
                if (!file.toPath().startsWith(rootPath) || file.equals(root)) {
                    throw new IOException("Zip Path Traversal Vulnerability: Zip entry " + entry.getName()
                            + " has " + file.getPath() + " as its target path. "
                            + "But it should not be outside target dir " + root.getPath());
                }
                if (entry.isDirectory()) {
                    Files.createDirectories(file.toPath());
                } else {
                    Files.createDirectories(file.getParentFile().toPath());
                    try (FileOutputStream output = new FileOutputStream(file)) {
                        int count;
                        while ((count = zip.read(buffer)) != -1) output.write(buffer, 0, count);
                    }
                }
            }
        }
    }

    public void setOnAudioFocusLossListener(Runnable listener) {
        onAudioFocusLoss = listener;
    }

    public synchronized boolean requestAudioFocus() {
        return audioManager.requestAudioFocus(focus) == AudioManager.AUDIOFOCUS_REQUEST_GRANTED;
    }

    public synchronized void startAudio(Intent returnIntent, int icon, String title, String body) {
        NotificationManager notificationManager = (NotificationManager) getSystemService(NOTIFICATION_SERVICE);
        NotificationChannel channel = new NotificationChannel(TAG, TAG, NotificationManager.IMPORTANCE_LOW);
        if (notificationManager != null) notificationManager.createNotificationChannel(channel);
        PendingIntent pendingIntent = PendingIntent.getActivity(getApplicationContext(), 0, returnIntent,
                PendingIntent.FLAG_IMMUTABLE | PendingIntent.FLAG_UPDATE_CURRENT);
        Notification notification = new Notification.Builder(this, TAG)
                .setSmallIcon(icon)
                .setContentTitle(title)
                .setTicker(title)
                .setContentText(body)
                .setOngoing(true)
                .setContentIntent(pendingIntent)
                .setWhen(System.currentTimeMillis())
                .build();
        stopForeground();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            int types = ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK;
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) types |= ServiceInfo.FOREGROUND_SERVICE_TYPE_MICROPHONE;
            startForeground(NOTIFICATION_ID, notification, types);
        } else {
            startForeground(NOTIFICATION_ID, notification);
        }
        hasForeground = true;
    }

    public synchronized void stopAudio() {
        RjdjCore.stopAudio();
        stopForeground();
    }

    public synchronized void abandonAudioFocus() {
        if (audioManager != null) audioManager.abandonAudioFocusRequest(focus);
    }

    public synchronized boolean isRunning() {
        return RjdjCore.isRunning();
    }

    private void stopForeground() {
        if (hasForeground) {
            stopForeground(true);
            hasForeground = false;
        }
    }

    @Override public IBinder onBind(Intent intent) {
        return binder;
    }

    @Override public boolean onUnbind(Intent intent) {
        onAudioFocusLoss = null;
        stopAudio();
        abandonAudioFocus();
        return false;
    }

    @Override public void onDestroy() {
        super.onDestroy();
        onAudioFocusLoss = null;
        stopAudio();
        abandonAudioFocus();
    }
}
