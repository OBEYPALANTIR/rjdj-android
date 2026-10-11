package org.puredata.android.scenes;
import java.io.*;
import java.nio.file.*;
import java.util.*;
import java.util.zip.*;

/** Standalone self-check: run main with the bundled atsuke.zip path. */
public final class SceneArchiveTest {
    private static final String INFO = "<?xml version=\"1.0\"?><!DOCTYPE plist PUBLIC \"Apple\" \"http://127.0.0.1:1/never-load.dtd\"><plist><dict><key>info</key><dict><key>name</key><string>Test &amp; scene</string><key>author</key><string>Artist</string><key>description</key><string>Text</string></dict></dict></plist>";
    private static int passed;
    public static void main(String[] args) throws Exception {
        Path work=Files.createTempDirectory("scene-check-");
        try {
            File root=work.resolve("scenes").toFile();
            byte[] valid=archive("wrapper/Test.rj/_main.pd", "patch", "wrapper/Test.rj/Info.plist", INFO);
            Path original=work.resolve("source.rjz"); Files.write(original, valid);
            File scene=SceneArchive.importScene(new FileInputStream(original.toFile()),root);
            check(scene.equals(new File(root,"Test.rj")),"single wrapped scene recognized");
            check(Arrays.equals(valid,Files.readAllBytes(original)),"source preserved");
            Map<String,String> info=SceneArchive.readInfo(scene);
            check("Test & scene".equals(info.get("name")) && "Artist".equals(info.get("author")) && "Text".equals(info.get("description")),"nested metadata and inert external DTD");
            reject(valid,root,"collision preserves existing scene");
            check("patch".equals(new String(Files.readAllBytes(new File(scene,"_main.pd").toPath()),"UTF-8")),"existing patch unchanged");
            for(String name:new String[]{"../escape","/absolute","a\\escape","C:/escape","a/./escape"}) reject(archive(name,"bad"),root,"unsafe path "+name);
            reject(archive("A.rj/Info.plist",INFO),root,"missing main patch");
            reject(archive("A.rj/_main.pd","patch"),root,"missing metadata");
            reject(archive("plain/_main.pd","patch","plain/Info.plist",INFO),root,"missing .rj scene directory");
            reject(archive("A.rj/_main.pd","p","A.rj/Info.plist",INFO,"B.rj/_main.pd","p","B.rj/Info.plist",INFO),root,"multiple scenes");
            reject(archive("A.rj/B.rj/_main.pd","p","A.rj/Info.plist",INFO),root,"nested scenes");
            for(String entity:new String[]{"SYSTEM 'file:///etc/passwd'","'expand'"}) reject(archive("A.rj/_main.pd","p","A.rj/Info.plist","<!DOCTYPE plist [<!ENTITY a "+entity+">]><plist><key>name</key><string>&a;</string></plist>"),root,"entity declaration "+entity);
            reject(new byte[]{1,2,3},root,"non archive");
            reject(archive("A.rj/","unexpected"),root,"directory with payload");
            String[] pairs=new String[(SceneArchive.MAX_ENTRIES+1)*2];
            for(int i=0;i<pairs.length;i+=2){pairs[i]="A.rj/"+i;pairs[i+1]="";}
            reject(archive(pairs),root,"entry count limit", "too many entries");
            reject(largeArchive(1,SceneArchive.MAX_ENTRY_BYTES+1),root,"per-entry decompressed limit", "size limits");
            reject(largeArchive(5,SceneArchive.MAX_TOTAL_BYTES/5+1),root,"total decompressed limit", "size limits");
            Path outside=work.resolve("outside");Files.write(outside,new byte[]{7});
            denyDelete(root,outside.toFile(),"outside deletion"); denyDelete(root,root,"root deletion");
            Path link=new File(scene,"link").toPath();Files.createSymbolicLink(link,outside);
            denyDelete(root,link.toFile(),"symlink deletion");Files.delete(link);
            SceneArchive.deleteOwnedTree(root,scene);
            check(!scene.exists() && Files.exists(original) && Files.exists(outside),"owned deletion preserves originals");
            if(args.length>0){File bundled=SceneArchive.importScene(new FileInputStream(args[0]),root);check("Atsuke".equals(SceneArchive.readInfo(bundled).get("name")),"bundled Atsuke imports");}
            System.out.println("PASS: "+passed+" importer/metadata/storage assertions");
        } finally {
            try(java.util.stream.Stream<Path> files=Files.walk(work)){files.sorted(Comparator.reverseOrder()).forEach(path->{try{Files.delete(path);}catch(IOException e){throw new UncheckedIOException(e);}});}
        }
    }
    private static void check(boolean ok,String scenario){if(!ok)throw new AssertionError(scenario);passed++;System.out.println("PASS "+scenario);}
    private static void denyDelete(File root,File target,String scenario)throws Exception{
        try{SceneArchive.deleteOwnedTree(root,target);throw new AssertionError(scenario);}catch(IOException expected){check(target.exists(),scenario+" denied");}
    }
    private static void reject(byte[] data,File root,String scenario)throws Exception{ reject(data,root,scenario,null); }
    private static void reject(byte[] data,File root,String scenario,String reason)throws Exception{
        int before=root.list().length;
        try{SceneArchive.importScene(new ByteArrayInputStream(data),root);throw new AssertionError(scenario);}catch(IOException expected){check(root.list().length==before && (reason==null || expected.getMessage().contains(reason)),scenario+" rejected; staging cleaned ("+expected.getMessage()+")");}
    }
    private static byte[] archive(String... pairs)throws Exception{
        ByteArrayOutputStream bytes=new ByteArrayOutputStream();
        try(ZipOutputStream zip=new ZipOutputStream(bytes)){for(int i=0;i<pairs.length;i+=2){zip.putNextEntry(new ZipEntry(pairs[i]));zip.write(pairs[i+1].getBytes("UTF-8"));zip.closeEntry();}}
        return bytes.toByteArray();
    }
    private static byte[] largeArchive(int entries,long size)throws Exception{
        ByteArrayOutputStream bytes=new ByteArrayOutputStream();
        try(ZipOutputStream zip=new ZipOutputStream(bytes)){byte[] block=new byte[8192];for(int i=0;i<entries;i++){zip.putNextEntry(new ZipEntry("A.rj/large"+i));for(long count=0;count<size;){int n=(int)Math.min(block.length,size-count);zip.write(block,0,n);count+=n;}zip.closeEntry();}}
        return bytes.toByteArray();
    }
}
