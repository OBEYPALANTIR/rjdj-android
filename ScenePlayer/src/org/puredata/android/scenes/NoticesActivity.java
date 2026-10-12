package org.puredata.android.scenes;

import java.io.IOException;
import java.io.InputStreamReader;
import java.io.Reader;
import java.nio.charset.StandardCharsets;

import android.app.Activity;
import android.os.Bundle;
import android.util.Log;
import android.widget.TextView;

public class NoticesActivity extends Activity {
    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        setTitle(R.string.notices_title);
        setContentView(R.layout.notices);
        SceneWindow.fitSystemBars(this);
        TextView notices = (TextView) findViewById(R.id.notices_text);
        try (Reader input = new InputStreamReader(getResources().openRawResource(R.raw.notices),
                StandardCharsets.UTF_8)) {
            StringBuilder text = new StringBuilder();
            char[] buffer = new char[8192];
            int count;
            while ((count = input.read(buffer)) != -1) text.append(buffer, 0, count);
            notices.setText(text);
        } catch (IOException e) {
            Log.e("Notices", "Cannot read bundled notices", e);
            notices.setText(R.string.notices_read_fail);
        }
    }
}
