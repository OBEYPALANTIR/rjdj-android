/**
 * 
 * @author Peter Brinkmann (peter.brinkmann@gmail.com)
 * 
 * For information on usage and redistribution, and for a DISCLAIMER OF ALL
 * WARRANTIES, see the file, "LICENSE.txt," in this distribution.
 * 
 */

package org.puredata.android.scenes;

import java.io.BufferedInputStream;
import java.io.File;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.StandardCopyOption;
import java.util.zip.ZipEntry;
import java.util.zip.ZipInputStream;


import android.app.TabActivity;
import android.content.Context;
import android.content.Intent;
import android.content.res.Resources;
import android.os.Bundle;
import android.util.Log;
import android.widget.TabHost;

public class SceneTabs extends TabActivity {

	private static final String RECORDINGS_TAG = "recordings";
	private static final String SCENES_TAG = "scenes";

	@Override
	protected void onCreate(Bundle savedInstanceState) {
		super.onCreate(savedInstanceState);
		unpackResources();
		Resources res = getResources();
		TabHost tabHost = getTabHost();
		TabHost.TabSpec spec;
		Intent intent;
		intent = new Intent().setClass(this, SceneSelection.class);
		spec = tabHost.newTabSpec(SCENES_TAG).setIndicator("Scenes", res.getDrawable(R.drawable.tab_scenes_selector)).setContent(intent);
		tabHost.addTab(spec);
		intent = new Intent().setClass(this, RecordingSelection.class);
		spec = tabHost.newTabSpec(RECORDINGS_TAG).setIndicator("Recordings", res.getDrawable(R.drawable.tab_recordings_selector)).setContent(intent);
		tabHost.addTab(spec);
		tabHost.setCurrentTab(0);
		SceneWindow.fitSystemBars(this);
	}
	
	private void unpackResources() {
		File libDir = getFilesDir();
		try (ZipInputStream zip = new ZipInputStream(new BufferedInputStream(
				getResources().openRawResource(R.raw.abstractions)))) {
			String canonicalRoot = libDir.getCanonicalPath();
			ZipEntry entry;
			while ((entry = zip.getNextEntry()) != null) {
				File file = new File(libDir, entry.getName());
				if (!file.getCanonicalPath().startsWith(canonicalRoot + File.separator)) {
					throw new IOException("Zip Path Traversal Vulnerability: Zip entry " + entry.getName()
							+ " has " + file.getCanonicalPath() + " as its target path. "
							+ "But it should not be outside target dir " + canonicalRoot);
				}
				if (entry.isDirectory()) {
					Files.createDirectories(file.toPath());
				} else {
					Files.createDirectories(file.getParentFile().toPath());
					Files.copy(zip, file.toPath(), StandardCopyOption.REPLACE_EXISTING);
				}
			}
		} catch (IOException e) {
			Log.e("Scene Player", e.toString());
		}
	}

	static String seedDefault(Context context, File scenesRoot) throws IOException {
		File cache = context.getCacheDir();
		File archive = File.createTempFile("atsuke-", ".zip", cache);
		try {
			try (InputStream input = context.getResources().openRawResource(R.raw.atsuke);
					FileOutputStream output = new FileOutputStream(archive)) {
				byte[] buffer = new byte[8192];
				int count;
				while ((count = input.read(buffer)) != -1) output.write(buffer, 0, count);
			}
			return RjdjCore.seedDefault(scenesRoot.getAbsolutePath(), archive.getAbsolutePath(), false);
		} finally {
			try { RjdjCore.deleteOwnedTree(cache.getAbsolutePath(), archive.getAbsolutePath()); }
			catch (IOException e) { Log.w("Scene Player", "Unable to remove default scene archive", e); }
		}
	}

}
