package org.puredata.android.scenes;

import android.Manifest;
import android.app.Activity;
import android.app.Instrumentation;
import android.content.ComponentName;
import android.content.Context;
import android.content.ContextWrapper;
import android.content.Intent;
import android.content.ServiceConnection;
import android.content.pm.PackageManager;
import android.database.Cursor;
import android.database.DatabaseErrorHandler;
import android.database.sqlite.SQLiteDatabase;
import android.os.Bundle;
import android.os.IBinder;
import android.os.ParcelFileDescriptor;
import java.io.File;
import java.io.InputStream;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicReference;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;

/** Exercises native storage/SQLite integration and real AAudio recordings on a visible app. */
public final class SceneEngineInstrumentation extends Instrumentation {
    @Override public void onCreate(Bundle args) { super.onCreate(args); start(); }

    @Override public void onStart() {
        Bundle result = new Bundle();
        Context context = getTargetContext();
        File realScenes = context.getDir("scenes", Context.MODE_PRIVATE);
        File realRecordings = new File(context.getFilesDir(), "recordings");
        SceneDataBase realDatabase = null;
        String[] directoriesBefore = null, recordingsBefore = null;
        List<String> rowsBefore = null;
        boolean storageSnapshot = false;
        File work = null, toneWav = null, muteWav = null;
        Activity visible = null;
        AtomicReference<SceneAudioService> service = new AtomicReference<>();
        CountDownLatch connected = new CountDownLatch(1);
        ServiceConnection connection = new ServiceConnection() {
            @Override public void onServiceConnected(ComponentName name, IBinder binder) {
                service.set(((SceneAudioService.SceneAudioBinder) binder).getService());
                connected.countDown();
            }
            @Override public void onServiceDisconnected(ComponentName name) { service.set(null); }
        };
        boolean bound = false;
        int status = Activity.RESULT_CANCELED;
        try {
            directoriesBefore = sceneDirectoryNames(realScenes);
            recordingsBefore = recordingNames(realRecordings);
            realDatabase = new SceneDataBase(context);
            rowsBefore = sceneRows(realDatabase);
            storageSnapshot = true;
            require(!RjdjCore.isRunning(), "Pause scene playback before running instrumentation");
            int initialized = RjdjCore.nativeInit(context.getFilesDir().getAbsolutePath(),
                    context.getCacheDir().getAbsolutePath(), context.getApplicationInfo().nativeLibraryDir);
            require(initialized == RjdjCore.OK, "nativeInit: " + ScenePlayer.statusName(initialized));
            work = Files.createTempDirectory(context.getCacheDir().toPath(), "engine-check-").toFile();
            checkStorage(context, work);
            require(context.checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED,
                    "Grant microphone permission in the app before running instrumentation");
            visible = startActivitySync(new Intent(context, NoticesActivity.class).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
            waitForIdleSync();
            bound = context.bindService(new Intent(context, SceneAudioService.class), connection, Context.BIND_AUTO_CREATE);
            require(bound && connected.await(5, TimeUnit.SECONDS), "audio service binder");
            SceneAudioService audioService = service.get();
            require(audioService != null && audioService.requestAudioFocus(), "audio focus");
            checkExtraResources(context);
            File recordings = new File(work, "recordings");
            Files.createDirectories(recordings.toPath());
            toneWav = new File(recordings, work.getName() + "-tone.wav");
            muteWav = new File(recordings, work.getName() + "-mute.wav");
            double toneEnergy = recordScene(context, work, toneWav, false);
            require(toneEnergy > 0.005, "tone recording energy > 0.005");
            double muteEnergy = recordScene(context, work, muteWav, true);
            require(muteEnergy < 0.000001, "microphone mute recording energy < 1e-6");
            result.putString("stream", "PASS: native import/plist, SQLite registration recovery, staging cleanup, recording validation, "
                    + "seven Pd extra abstractions, AAudio stereo WAV at 22050 Hz, channel parity; tone energy=" + toneEnergy + ", muted energy=" + muteEnergy + "\n");
            status = Activity.RESULT_OK;
        } catch (Throwable error) {
            result.putString("stream", "FAIL: " + android.util.Log.getStackTraceString(error));
        } finally {
            try {
                SceneAudioService audioService = service.get();
                if (audioService != null) audioService.stopAudio();
                if (bound) context.unbindService(connection);
                if (visible != null) { Activity activity = visible; runOnMainSync(activity::finish); }
                File recordings = work == null ? null : new File(work, "recordings");
                if (toneWav != null) RjdjCore.deleteOwnedTree(recordings.getAbsolutePath(), toneWav.getAbsolutePath());
                if (muteWav != null) RjdjCore.deleteOwnedTree(recordings.getAbsolutePath(), muteWav.getAbsolutePath());
                if (work != null) RjdjCore.deleteOwnedTree(context.getCacheDir().getAbsolutePath(), work.getAbsolutePath());
            } catch (Throwable error) {
                status = Activity.RESULT_CANCELED;
                result.putString("stream", "FAIL: " + android.util.Log.getStackTraceString(error));
            } finally {
                try {
                    if (storageSnapshot) {
                        boolean directoriesUnchanged = Arrays.equals(directoriesBefore, sceneDirectoryNames(realScenes));
                        boolean rowsUnchanged = rowsBefore.equals(sceneRows(realDatabase));
                        boolean recordingsUnchanged = Arrays.equals(recordingsBefore, recordingNames(realRecordings));
                        require(directoriesUnchanged, "whole run preserves real scene directories");
                        require(rowsUnchanged, "whole run preserves real scene rows");
                        require(recordingsUnchanged, "whole run preserves real recordings listing");
                    }
                } catch (Throwable error) {
                    status = Activity.RESULT_CANCELED;
                    result.putString("stream", "FAIL: " + android.util.Log.getStackTraceString(error));
                } finally {
                    if (realDatabase != null) realDatabase.close();
                }
            }
        }
        finish(status, result);
    }

    private void checkExtraResources(Context context) throws Exception {
        int files = 0;
        byte[] buffer = new byte[2048];
        try (ZipInputStream zip = new ZipInputStream(context.getResources().openRawResource(R.raw.extra_abs))) {
            ZipEntry entry;
            while ((entry = zip.getNextEntry()) != null) {
                require(!entry.isDirectory(), "extra resource contains abstractions only");
                byte[] installed = Files.readAllBytes(new File(context.getFilesDir(), entry.getName()).toPath());
                int offset = 0, count;
                while ((count = zip.read(buffer)) != -1) {
                    require(offset + count <= installed.length, "installed extra abstraction length");
                    for (int i = 0; i < count; i++) require(buffer[i] == installed[offset + i], "installed extra abstraction bytes");
                    offset += count;
                }
                require(offset > 0 && offset == installed.length, "complete nonempty extra abstraction");
                files++;
            }
        }
        require(files == 7, "seven generated Pd extra abstractions");
    }

    private void checkStorage(Context context, File work) throws Exception {
        File realScenes = context.getDir("scenes", Context.MODE_PRIVATE);
        String[] directoriesBefore = sceneDirectoryNames(realScenes);
        SceneDataBase realDatabase = new SceneDataBase(context);
        try {
            List<String> rowsBefore = sceneRows(realDatabase);
            Context isolated = new ContextWrapper(context) {
                @Override public File getDir(String name, int mode) {
                    File directory = new File(work, "app_" + name);
                    require(directory.isDirectory() || directory.mkdirs(), "isolated storage directory");
                    return directory;
                }
                @Override public File getFilesDir() { return work; }
                @Override public File getDatabasePath(String name) { return new File(work, name); }
                @Override public SQLiteDatabase openOrCreateDatabase(String name, int mode,
                        SQLiteDatabase.CursorFactory factory) {
                    return SQLiteDatabase.openOrCreateDatabase(getDatabasePath(name), factory);
                }
                @Override public SQLiteDatabase openOrCreateDatabase(String name, int mode,
                        SQLiteDatabase.CursorFactory factory, DatabaseErrorHandler handler) {
                    return SQLiteDatabase.openOrCreateDatabase(getDatabasePath(name).getAbsolutePath(), factory, handler);
                }
            };
            try {
                checkIsolatedStorage(isolated, work);
            } finally {
                require(Arrays.equals(directoriesBefore, sceneDirectoryNames(realScenes)),
                        "storage checks preserve real scene directories");
                require(rowsBefore.equals(sceneRows(realDatabase)), "storage checks preserve real scene rows");
            }
        } finally { realDatabase.close(); }
    }

    private String[] sceneDirectoryNames(File scenes) {
        String[] names = scenes.list((parent, name) -> new File(parent, name).isDirectory());
        require(names != null, "list real scene directories");
        Arrays.sort(names);
        return names;
    }

    private String[] recordingNames(File recordings) {
        if (!recordings.exists()) return null;
        String[] names = recordings.list();
        require(names != null, "list real recordings");
        Arrays.sort(names);
        return names;
    }

    private List<String> sceneRows(SceneDataBase database) {
        List<String> rows = new ArrayList<>();
        try (Cursor cursor = database.getAllScenes()) {
            while (cursor.moveToNext()) {
                String[] row = new String[cursor.getColumnCount()];
                for (int column = 0; column < row.length; column++) row[column] = cursor.getString(column);
                rows.add(Arrays.toString(row));
            }
        }
        Collections.sort(rows);
        return rows;
    }

    private void checkIsolatedStorage(Context context, File work) throws Exception {
        File zip = new File(work, "atsuke.zip");
        try (InputStream resource = context.getResources().openRawResource(R.raw.atsuke)) {
            Files.copy(resource, zip.toPath());
        }
        File imported;
        try (ParcelFileDescriptor descriptor = ParcelFileDescriptor.open(zip, ParcelFileDescriptor.MODE_READ_ONLY)) {
            imported = new File(RjdjCore.importScene(descriptor.detachFd(), new File(work, "import").getAbsolutePath()));
        }
        String[] info = RjdjCore.readInfo(imported.getAbsolutePath());
        boolean atsuke = false;
        require(info.length % 2 == 0, "flat native plist metadata");
        for (int i = 0; i < info.length; i += 2) {
            if ("name".equals(info[i]) && "Atsuke".equals(info[i + 1])) atsuke = true;
        }
        require(atsuke, "native plist import");
        File scenes = context.getDir("scenes", 0);
        File scene = new File(scenes, work.getName() + ".rj");
        require(imported.renameTo(scene), "publish recovery fixture");
        File staging = new File(scenes, ".import-" + work.getName());
        require(staging.mkdir(), "abandoned import fixture");
        Files.write(new File(staging, "unfinished").toPath(), new byte[] {1});
        SceneDataBase database = new SceneDataBase(context);
        long id = -1, recordingId = -1;
        File recordings = new File(context.getFilesDir(), "recordings");
        File recording = new File(recordings, work.getName() + ".wav");
        try {
            int before = sceneCount(database);
            database.recoverScenes();
            int recovered = sceneCount(database);
            id = database.addScene(scene);
            require(recovered == before + 1, "startup recovers published user scene before explicit registration");
            require(!staging.exists(), "abandoned import staging removed");
            database.recoverScenes();
            require(sceneCount(database) == before + 1, "recovery is idempotent");
            require(database.addScene(scene) == id, "interrupted registration recovers without duplicate row");
            require(new File(scene, "_main.pd").isFile(), "registration preserves scene");
            File mainPatch = new File(scene, "_main.pd");
            File savedPatch = new File(work, "saved-main.pd");
            require(mainPatch.renameTo(savedPatch), "interrupted scene deletion fixture");
            database.recoverScenes();
            require(sceneCount(database) == before + 1, "registered partial scene does not block recovery");
            require(savedPatch.renameTo(mainPatch), "restore registered fixture");
            require(!RjdjCore.validateRecording(recording.getAbsolutePath()), "missing recording fails native validation");
            require(database.addRecording(recording.getPath(), 0, 0, 0, 0, id) < 0, "missing recording is not registered");
            Files.createDirectories(recordings.toPath());
            Files.createFile(recording.toPath());
            require(!RjdjCore.validateRecording(recording.getAbsolutePath()), "empty recording fails native validation");
            require(!recording.exists(), "native validation removes empty recording");
            require(database.addRecording(recording.getPath(), 0, 0, 0, 0, id) < 0, "empty recording is not registered");
            Files.write(recording.toPath(), new byte[] {1, 2});
            require(RjdjCore.validateRecording(recording.getAbsolutePath()), "native validation retains nonempty recording");
            recordingId = database.addRecording(recording.getPath(), 0, 1, 0, 0, id);
            require(recordingId >= 0, "nonempty recording is registered");
        } finally {
            try {
                if (recordingId >= 0) database.deleteRecording(recordingId);
                else RjdjCore.deleteOwnedTree(recordings.getAbsolutePath(), recording.getAbsolutePath());
                if (id >= 0) database.deleteScene(id);
                else RjdjCore.deleteOwnedTree(scenes.getAbsolutePath(), scene.getAbsolutePath());
                RjdjCore.deleteOwnedTree(scenes.getAbsolutePath(), staging.getAbsolutePath());
            } finally { database.close(); }
        }
    }

    private double recordScene(Context context, File work, File wav, boolean mute) throws Exception {
        File source = new File(work, mute ? "mute.rj" : "tone.rj");
        Files.createDirectory(source.toPath());
        String patch = "#N canvas 0 0 400 300 10;\n"
                + "#X obj 20 20 soundinput;\n#X obj 20 80 soundoutput;\n"
                + (mute ? "" : "#X obj 180 20 osc~ 440;\n#X obj 180 50 *~ 0.25;\n")
                + "#X connect 0 0 1 0;\n#X connect 0 1 1 1;\n"
                + (mute ? "" : "#X connect 2 0 3 0;\n#X connect 3 0 1 0;\n#X connect 3 0 1 1;\n");
        Files.write(new File(source, "_main.pd").toPath(), patch.getBytes(StandardCharsets.UTF_8));
        String playback = RjdjCore.preparePlayback(source.getAbsolutePath(), context.getCacheDir().getAbsolutePath());
        boolean started = false;
        try {
            int status = RjdjCore.startAudio(playback, 1.0f);
            require(status == RjdjCore.OK, "native start: " + ScenePlayer.statusName(status));
            started = true;
            require(RjdjCore.isRunning(), "native running state");
            if (mute) {
                RjdjCore.sendMicVolume(0.0f);
                Thread.sleep(100); // Let the bridge's 50 ms gain ramp settle before recording.
            }
            RjdjCore.sendTransportPlay(true);
            RjdjCore.sendRecord(wav.getAbsolutePath(), true);
            Thread.sleep(2000);
            RjdjCore.sendRecord(wav.getAbsolutePath(), false);
            RjdjCore.stopAudio();
            require(!RjdjCore.isRunning(), "native stopped state");
        } finally {
            if (started) RjdjCore.stopAudio();
            RjdjCore.releasePlayback(playback);
        }
        require(RjdjCore.validateRecording(wav.getAbsolutePath()), "native recording is nonempty");
        return wavEnergy(wav);
    }

    private double wavEnergy(File wav) throws Exception {
        ByteBuffer bytes = ByteBuffer.wrap(Files.readAllBytes(wav.toPath())).order(ByteOrder.LITTLE_ENDIAN);
        require(bytes.limit() >= 12, "WAV header");
        require(bytes.getInt(0) == 0x46464952 && bytes.getInt(8) == 0x45564157, "RIFF/WAVE header");
        require(bytes.getInt(4) == bytes.limit() - 8, "finalized RIFF length");
        int format = 0, channels = 0, rate = 0, bits = 0, data = -1, dataSize = 0;
        for (int offset = 12; offset + 8 <= bytes.limit();) {
            int chunk = bytes.getInt(offset), size = bytes.getInt(offset + 4);
            require(size >= 0 && size <= bytes.limit() - offset - 8, "WAV chunk bounds");
            if (chunk == 0x20746d66) { // fmt
                require(size >= 16, "WAV fmt chunk");
                format = bytes.getShort(offset + 8) & 0xffff;
                channels = bytes.getShort(offset + 10) & 0xffff;
                rate = bytes.getInt(offset + 12);
                bits = bytes.getShort(offset + 22) & 0xffff;
            } else if (chunk == 0x61746164) { // data
                data = offset + 8;
                dataSize = size;
            }
            offset += 8 + size + (size & 1);
        }
        require(format == 1 && bits == 16 && channels == 2, "16-bit stereo PCM WAV");
        require(rate == 22050, "WAV sample rate is 22050 Hz, got " + rate);
        require(data >= 0 && dataSize > 0 && dataSize % 4 == 0, "complete nonempty stereo WAV frames");
        double energy = 0;
        for (int offset = data; offset < data + dataSize; offset += 4) {
            double left = bytes.getShort(offset) / 32768.0;
            double right = bytes.getShort(offset + 2) / 32768.0;
            require(Math.abs(left - right) < 0.00001, "stereo channel parity < 1e-5");
            energy += left * left + right * right;
        }
        return energy / (dataSize / 2);
    }

    private int sceneCount(SceneDataBase database) {
        try (Cursor scenes = database.getAllScenes()) { return scenes.getCount(); }
    }

    private void require(boolean condition, String name) {
        if (!condition) throw new AssertionError(name);
    }
}
