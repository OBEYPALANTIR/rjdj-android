package org.puredata.android.scenes;

import android.app.Application;
import android.util.Log;

public final class RjdjApplication extends Application {
    @Override
    public void onCreate() {
        super.onCreate();
        try {
            System.loadLibrary("rjdjcore");
        } catch (UnsatisfiedLinkError | SecurityException e) {
            Log.e("RjdjCore", "Unable to load rjdjcore", e);
            throw e;
        }
        Log.i("RjdjCore", RjdjCore.hello());
        Log.i("RjdjCore", "nativeInit -> " + RjdjCore.nativeInit(
                getFilesDir().getAbsolutePath(),
                getCacheDir().getAbsolutePath(),
                getApplicationInfo().nativeLibraryDir));
    }
}
