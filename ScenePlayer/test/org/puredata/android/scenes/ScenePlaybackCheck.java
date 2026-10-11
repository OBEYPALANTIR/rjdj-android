package org.puredata.android.scenes;

import java.io.File;
import java.io.IOException;
import java.nio.file.Files;

/** Standalone JVM regression: run with java -ea. */
public final class ScenePlaybackCheck {
    public static void main(String[] args) throws Exception {
        File root = Files.createTempDirectory("scene-playback-check").toFile();
        try {
            File source = new File(root, "source");
            File nested = new File(source, "nested");
            assert nested.mkdirs();
            Files.write(new File(source, "_main.pd").toPath(), new byte[] {1, 2});
            for (String name : new String[] {"rj_image.pd", "rj_text.pd", "soundinput.pd", "soundoutput.pd"}) {
                Files.write(new File(source, name).toPath(), new byte[] {3});
                Files.write(new File(nested, name).toPath(), new byte[] {4});
            }
            Files.write(new File(nested, "image.png").toPath(), new byte[] {5, 6});
            File cache = new File(root, "cache");
            File copy = ScenePlayback.prepare(source, cache);
            for (String name : new String[] {"rj_image.pd", "rj_text.pd", "soundinput.pd", "soundoutput.pd"}) {
                assert new File(source, name).isFile();
                assert new File(nested, name).isFile();
                assert !new File(copy, name).exists();
                assert !new File(copy, "nested/" + name).exists();
            }
            assert Files.readAllBytes(ScenePlayback.containedFile(copy, "nested/image.png").toPath())[0] == 5;
            for (String bad : new String[] {"../source/_main.pd", "/etc/passwd", "."}) {
                try { ScenePlayback.containedFile(copy, bad); throw new AssertionError("Accepted " + bad); }
                catch (IOException expected) { }
            }
            Files.createSymbolicLink(new File(source, "escape").toPath(), root.toPath());
            try { ScenePlayback.prepare(source, cache); throw new AssertionError("Accepted symbolic link"); }
            catch (IOException expected) { }
            assert cache.list().length == 1 : "Failed copy leaked cache";
            assert new File(source, "_main.pd").isFile();
            Files.delete(new File(source, "escape").toPath());
            File deep = source;
            for (int i = 0; i < 65; i++) { deep = new File(deep, "d"); assert deep.mkdir(); }
            try { ScenePlayback.prepare(source, cache); throw new AssertionError("Accepted excessive depth"); }
            catch (IOException expected) { }
            assert cache.list().length == 1;
            System.out.println("PASS: source preserved, nested bridge omission, asset bytes, traversal/absolute/root rejection, symlink/depth rejection, failed-copy cleanup");
        } finally { ScenePlayback.delete(root); }
    }
}
