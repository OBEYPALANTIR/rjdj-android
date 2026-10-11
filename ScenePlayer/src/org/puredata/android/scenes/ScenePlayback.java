package org.puredata.android.scenes;

import java.io.File;
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.LinkOption;
import java.util.HashSet;
import java.util.Set;

/** Private playback copies leave imported scene abstractions intact. */
final class ScenePlayback {
    private static final long MAX_BYTES = 256L * 1024 * 1024;
    private static final Set<File> activeCopies = new HashSet<>();

    static File containedFile(File root, String path) throws IOException {
        File file = new File(root, path).getCanonicalFile();
        if (new File(path).isAbsolute() || !file.toPath().startsWith(root.getCanonicalFile().toPath())
                || file.equals(root.getCanonicalFile())) throw new IOException("Path escapes scene");
        return file;
    }

    static File prepare(File source, File cache) throws IOException {
        if (!source.isDirectory()) throw new IOException("Scene is missing");
        Files.createDirectories(cache.toPath());
        File copy;
        synchronized (ScenePlayback.class) {
            File[] previous = cache.listFiles((directory, name) -> name.startsWith("playback-"));
            if (previous == null) throw new IOException("Cannot inspect scene cache");
            for (File stale : previous) {
                if (activeCopies.contains(stale)) continue;
                delete(stale);
                if (stale.exists()) throw new IOException("Cannot reclaim scene cache");
            }
            copy = Files.createTempDirectory(cache.toPath(), "playback-").toFile();
            // Process-local ownership keeps live players safe; after process death none are owned.
            activeCopies.add(copy);
        }
        try {
            copy(source.getCanonicalFile(), copy, 0, new long[2]);
            if (!new File(copy, "_main.pd").isFile()) throw new IOException("Scene has no _main.pd");
            return copy;
        } catch (IOException e) {
            delete(copy);
            throw e;
        }
    }

    private static void copy(File source, File target, int depth, long[] size) throws IOException {
        if (Thread.currentThread().isInterrupted()) throw new IOException("Playback preparation cancelled");
        if (depth > 64 || ++size[0] > 10000 || Files.isSymbolicLink(source.toPath()))
            throw new IOException("Scene exceeds limits or contains links");
        if (Files.isDirectory(source.toPath(), LinkOption.NOFOLLOW_LINKS)) {
            Files.createDirectories(target.toPath());
            File[] children = source.listFiles();
            if (children == null) throw new IOException("Cannot read scene");
            for (File child : children) {
                String name = child.getName();
                if (name.equals("rj_image.pd") || name.equals("rj_text.pd")
                        || name.equals("soundinput.pd") || name.equals("soundoutput.pd")) continue;
                copy(child, new File(target, name), depth + 1, size);
            }
        } else {
            if (!Files.isRegularFile(source.toPath(), LinkOption.NOFOLLOW_LINKS)
                    || (size[1] += source.length()) > MAX_BYTES) throw new IOException("Scene exceeds limits");
            Files.copy(source.toPath(), target.toPath());
        }
    }

    static synchronized void delete(File file) {
        if (file == null) return;
        if (!Files.isSymbolicLink(file.toPath()) && file.isDirectory()) {
            File[] children = file.listFiles();
            if (children != null) for (File child : children) delete(child);
        }
        file.delete();
        activeCopies.remove(file);
    }
}
