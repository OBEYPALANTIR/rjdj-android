package org.puredata.android.scenes;

import java.io.IOException;

final class RjdjCore {
    static final int OK = 0;
    static final int ALREADY_RUNNING = 1;
    static final int NOT_INITIALIZED = -1;
    static final int PATCH_OPEN_FAILED = -2;
    static final int OUTPUT_OPEN_FAILED = -3;
    static final int INPUT_OPEN_FAILED = -4;
    static final int START_FAILED = -5;
    static final int INVALID_ARGUMENT = -6;
    static final int DISCONNECTED = -7;
    static final int INTERNAL = -8;

    static final int STOPPED = 0;
    static final int STARTING = 1;
    static final int RUNNING = 2;
    static final int RESTARTING = 3;
    static final int FAILED = 4;

    // Loaded here so any first caller (instrumentation threads included) cannot race
    // RjdjApplication.onCreate; System.loadLibrary is idempotent.
    static {
        System.loadLibrary("rjdjcore");
    }

    private RjdjCore() {
    }

    static native String hello();
    static native int nativeInit(String filesDir, String cacheDir, String nativeLibraryDir);

    static native String importScene(int detachedFd, String scenesRoot) throws IOException;
    static native void recoverScenes(String scenesRoot, String[] registered, Registrar registrar) throws IOException;
    static native String seedDefault(String scenesRoot, String atsukeZipPath, boolean alreadySeeded) throws IOException;
    static native String[] readInfo(String sceneDir) throws IOException;
    static native void deleteOwnedTree(String root, String target) throws IOException;
    static native String preparePlayback(String sceneDir, String cacheDir) throws IOException;
    static native void releasePlayback(String playbackDir) throws IOException;
    static native boolean validateRecording(String path) throws IOException;

    static native int startAudio(String playbackDir, float micVolume);
    static native void stopAudio();
    static native boolean isRunning();
    static native void sendTransportPlay(boolean on);
    static native void sendTouch(int kind, int pointerId1Based, float x, float y);
    static native void sendAccelerate(float x, float y, float z);
    static native void sendMicVolume(float gain);
    static native boolean sendRecord(String wavPath, boolean start);
    static native long[] audioInfo();
    static native void setListener(Listener listener);

    interface Registrar {
        void register(String sceneDir) throws IOException;
    }

    interface Listener {
        void onList(String receiver, Object[] args);
        void onPrint(String line);
        void onAudioState(int state, int status);
    }
}
