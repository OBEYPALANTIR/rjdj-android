package org.puredata.android.scenes;

import android.app.Activity;
import android.os.Build;
import android.view.View;
import android.view.WindowInsets;

final class SceneWindow {
    private SceneWindow() {}

    static void fitSystemBars(Activity activity) {
        View content = activity.findViewById(android.R.id.content);
        content.setOnApplyWindowInsetsListener((view, insets) -> {
            if (Build.VERSION.SDK_INT >= 30) {
                android.graphics.Insets bars = insets.getInsets(
                        WindowInsets.Type.systemBars() | WindowInsets.Type.displayCutout());
                view.setPadding(bars.left, bars.top, bars.right, bars.bottom);
            } else {
                view.setPadding(insets.getSystemWindowInsetLeft(), insets.getSystemWindowInsetTop(),
                        insets.getSystemWindowInsetRight(), insets.getSystemWindowInsetBottom());
            }
            return insets.consumeSystemWindowInsets();
        });
        content.requestApplyInsets();
    }
}
