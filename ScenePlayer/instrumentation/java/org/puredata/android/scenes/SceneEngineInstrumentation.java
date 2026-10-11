package org.puredata.android.scenes;

import android.app.Activity;
import android.app.Instrumentation;
import android.content.Context;
import android.os.Bundle;
import org.puredata.core.PdBase;
import org.puredata.core.PdBaseLoader;
import org.puredata.core.utils.IoUtils;
import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;

/** Exercises Android's real XML/SQLite and the packaged native DSP without opening audio hardware. */
public final class SceneEngineInstrumentation extends Instrumentation {
    @Override public void onCreate(Bundle args) { super.onCreate(args); start(); }

    @Override public void onStart() {
        Bundle result = new Bundle();
        File work = null;
        boolean engineReady = false;
        int status = Activity.RESULT_CANCELED;
        try {
            Context context = getTargetContext();
            work = Files.createTempDirectory(context.getCacheDir().toPath(), "engine-check-").toFile();
            File imported = SceneArchive.importScene(context.getResources().openRawResource(R.raw.atsuke), new File(work, "import"));
            require("Atsuke".equals(SceneArchive.readInfo(imported).get("name")), "Android plist import");
            File scene = new File(context.getDir("scenes", 0), work.getName() + ".rj");
            require(imported.renameTo(scene), "publish recovery fixture");
            SceneDataBase database = new SceneDataBase(context);
            long id = -1;
            try {
                id = database.addScene(scene);
                require(database.addScene(scene) == id, "interrupted registration recovers without duplicate row");
                require(new File(scene, "_main.pd").isFile(), "registration preserves scene");
            } finally {
                if (id >= 0) database.deleteScene(id);
                else SceneArchive.deleteOwnedTree(context.getDir("scenes", 0), scene);
                database.close();
            }

            // The plain JNI entry point renders deterministic buffers through the same packaged libpd.
            PdBaseLoader.loaderHandler = new PdBaseLoader() {
                @Override public void load() { System.loadLibrary("pd"); System.loadLibrary("pdnative"); }
            };
            IoUtils.extractZipResource(context.getResources().openRawResource(R.raw.abstractions), work, true);
            PdBase.addToSearchPath(work.getAbsolutePath());
            File patch = new File(work, "_main.pd");
            Files.write(patch.toPath(), ("#N canvas 0 0 400 300 10;\n"
                    + "#X obj 20 20 soundinput;\n#X obj 20 80 soundoutput;\n"
                    + "#X connect 0 0 1 0;\n#X connect 0 1 1 1;\n").getBytes(StandardCharsets.UTF_8));
            require(PdBase.openAudio(1, 2, 48000) == 0, "native audio configuration");
            engineReady = true;
            PdBase.openPatch(patch);
            PdBase.computeAudio(true);
            PdBase.sendFloat("#micvolume", 1);
            PdBase.sendMessage("#transport", "play", 1);
            float[] input = new float[64], output = new float[128];
            double energy = render(input, output);
            require(energy > 0.005, "microphone buffer reaches stereo scene output");
            for (int i = 0; i < output.length; i += 2) require(Math.abs(output[i] - output[i + 1]) < 0.00001, "stereo channel parity");
            PdBase.sendFloat("#micvolume", 0);
            require(render(input, output) < 0.000001, "microphone mute bridge");
            result.putString("stream", "PASS: Android archive parser, SQLite recovery, native stereo DSP, microphone mute\n");
            status = Activity.RESULT_OK;
        } catch (Throwable error) {
            result.putString("stream", "FAIL: " + android.util.Log.getStackTraceString(error));
        } finally {
            if (engineReady) PdBase.release();
            ScenePlayback.delete(work);
        }
        finish(status, result);
    }

    private double render(float[] input, float[] output) {
        for (int block = 0; block < 800; block++) {
            for (int frame = 0; frame < input.length; frame++)
                input[frame] = (float) (0.25 * Math.sin(2 * Math.PI * 440 * (block * 64 + frame) / 48000));
            require(PdBase.process(1, input, output) == 0, "native process");
        }
        double energy = 0;
        for (float value : output) energy += value * value;
        return energy / output.length;
    }

    private void require(boolean condition, String name) {
        if (!condition) throw new AssertionError(name);
    }
}
