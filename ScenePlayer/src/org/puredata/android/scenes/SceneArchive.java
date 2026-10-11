package org.puredata.android.scenes;

import java.io.*;
import java.nio.charset.StandardCharsets;
import java.util.*;
import java.util.zip.*;
import javax.xml.parsers.SAXParserFactory;
import org.xml.sax.*;
import org.xml.sax.helpers.DefaultHandler;

/** Imports untrusted archives into private storage before publishing a scene. */
public final class SceneArchive {
    static final int MAX_ENTRIES = 4096;
    static final long MAX_ENTRY_BYTES = 64L * 1024 * 1024;
    static final long MAX_TOTAL_BYTES = 256L * 1024 * 1024;
    static final int MAX_INFO_BYTES = 1024 * 1024;

    private SceneArchive() { }

    public static synchronized File importScene(InputStream input, File root) throws IOException {
        if (input == null) throw new IOException("No archive was received");
        File staging = null;
        File published = null;
        try (ZipInputStream zip = new ZipInputStream(input)) {
            if (!root.isDirectory() && !root.mkdirs()) throw new IOException("Cannot create scene storage");
            staging = File.createTempFile(".import-", "", root);
            if (!staging.delete() || !staging.mkdir()) throw new IOException("Cannot stage scene");
            Set<String> paths = new HashSet<String>();
            List<File> scenes = new ArrayList<File>();
            long total = 0;
            int count = 0;
            ZipEntry entry;
            byte[] buffer = new byte[8192];
            while ((entry = zip.getNextEntry()) != null) {
                if (++count > MAX_ENTRIES) throw new IOException("Archive contains too many entries");
                String name = entry.getName();
                if (name.length() > 1024 || name.split("/").length > 32) throw new IOException("Archive path is too deep");
                if (name.startsWith("/") || name.contains("\\") || name.contains(":"))
                    throw new IOException("Invalid archive path");
                for (String part : name.split("/")) {
                    if (part.equals("..") || part.equals(".")) throw new IOException("Invalid archive path");
                }
                File file = new File(staging, name);
                requireOwned(staging, file);
                if (!paths.add(file.getCanonicalPath())) throw new IOException("Duplicate archive entry");
                if (entry.isDirectory()) {
                    if (zip.read() != -1) throw new IOException("Directory entry contains data");
                    if (!file.isDirectory() && !file.mkdirs()) throw new IOException("Cannot create scene directory");
                } else {
                    if (!file.getParentFile().isDirectory() && !file.getParentFile().mkdirs())
                        throw new IOException("Cannot create scene directory");
                    if (!file.createNewFile()) throw new IOException("Archive path collision");
                    long size = 0;
                    try (OutputStream out = new FileOutputStream(file)) {
                        int n;
                        while ((n = zip.read(buffer)) != -1) {
                            size += n;
                            total += n;
                            if (size > MAX_ENTRY_BYTES || total > MAX_TOTAL_BYTES)
                                throw new IOException("Archive exceeds scene size limits");
                            out.write(buffer, 0, n);
                        }
                    }
                }
                zip.closeEntry();
            }
            findScenes(staging, scenes);
            if (scenes.size() != 1) throw new IOException("Archive must contain exactly one .rj scene");
            File scene = scenes.get(0);
            if (!new File(scene, "_main.pd").isFile()) throw new IOException("Scene is missing _main.pd");
            readInfo(scene);
            File destination = new File(root, scene.getName());
            requireOwned(root, destination);
            if (destination.exists()) throw new IOException("A scene with this folder name already exists");
            if (!scene.renameTo(destination)) throw new IOException("Cannot publish scene");
            published = destination;
            return destination;
        } finally {
            try {
                if (staging != null && staging.exists()) deleteOwnedTree(root, staging);
            } catch (IOException cleanup) {
                if (published != null) deleteOwnedTree(root, published);
                throw cleanup;
            }
        }
    }

    private static void findScenes(File folder, List<File> scenes) throws IOException {
        File[] children = folder.listFiles();
        if (children == null) throw new IOException("Cannot inspect scene directory");
        for (File child : children) {
            if (child.isDirectory()) {
                if (child.getName().endsWith(".rj")) scenes.add(child);
                findScenes(child, scenes);
            }
        }
    }

    static void requireOwned(File root, File target) throws IOException {
        String prefix = root.getCanonicalPath() + File.separator;
        if (!target.getCanonicalPath().startsWith(prefix)) throw new IOException("Path is outside private storage");
        // Reject links even when they point elsewhere within the owned directory.
        File relative = target.getAbsoluteFile();
        while (relative != null && !relative.equals(root.getAbsoluteFile())) {
            if (!relative.getCanonicalFile().equals(new File(relative.getParentFile().getCanonicalFile(), relative.getName())))
                throw new IOException("Symbolic links are not scene files");
            relative = relative.getParentFile();
        }
    }

    public static void deleteOwnedTree(File root, File target) throws IOException {
        requireOwned(root, target);
        if (!target.exists()) return;
        if (target.isDirectory()) {
            File[] children = target.listFiles();
            if (children == null) throw new IOException("Cannot inspect private directory");
            for (File child : children) deleteOwnedTree(root, child);
        }
        if (!target.delete()) throw new IOException("Cannot delete private file");
    }

    public static Map<String, String> readInfo(File scene) throws IOException {
        File info = new File(scene, "Info.plist");
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        try (InputStream in = new FileInputStream(info)) {
            byte[] buffer = new byte[4096];
            int n;
            while ((n = in.read(buffer)) != -1) {
                if (bytes.size() + n > MAX_INFO_BYTES) throw new IOException("Scene metadata is too large");
                bytes.write(buffer, 0, n);
            }
        }
        String xml = new String(bytes.toByteArray(), StandardCharsets.UTF_8);
        // Plists use UTF-8; rejecting declarations prevents internal entity expansion too.
        if (xml.indexOf('\0') >= 0 || xml.contains("<!ENTITY")) throw new IOException("Unsupported scene metadata entities or encoding");
        final Map<String, String> values = new HashMap<String, String>();
        try {
            SAXParserFactory factory = SAXParserFactory.newInstance();
            factory.setNamespaceAware(true);
            factory.setFeature("http://xml.org/sax/features/external-general-entities", false);
            factory.setFeature("http://xml.org/sax/features/external-parameter-entities", false);
            XMLReader reader = factory.newSAXParser().getXMLReader();
            reader.setEntityResolver((publicId, systemId) -> new InputSource(new StringReader("")));
            reader.setContentHandler(new DefaultHandler() {
                String key;
                StringBuilder text = new StringBuilder();
                @Override public void startElement(String uri, String local, String qualified, Attributes attrs) {
                    text.setLength(0);
                    if (!local.equals("key") && !local.equals("string") && !local.equals("integer")) key = null;
                }
                @Override public void characters(char[] chars, int start, int length) { text.append(chars, start, length); }
                @Override public void endElement(String uri, String local, String qualified) {
                    if (local.equals("key")) key = text.toString().trim();
                    else if (key != null && (local.equals("string") || local.equals("integer"))) {
                        values.put(key, text.toString().trim());
                        key = null;
                    }
                    text.setLength(0);
                }
            });
            reader.parse(new InputSource(new StringReader(xml)));
        } catch (Exception e) { throw new IOException("Invalid scene metadata", e); }
        if (!values.containsKey("name") || values.get("name").isEmpty()) throw new IOException("Scene metadata is missing its name");
        if (!values.containsKey("author")) values.put("author", "Unknown artist");
        if (!values.containsKey("description")) values.put("description", "");
        return values;
    }
}
