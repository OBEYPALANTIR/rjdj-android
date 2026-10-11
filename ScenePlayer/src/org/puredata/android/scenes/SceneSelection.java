/**
 * 
 * @author Peter Brinkmann (peter.brinkmann@gmail.com)
 * 
 * For information on usage and redistribution, and for a DISCLAIMER OF ALL
 * WARRANTIES, see the file, "LICENSE.txt," in this distribution.
 * 
 */

package org.puredata.android.scenes;

import java.io.File;
import java.io.IOException;
import java.io.InputStream;
import java.lang.ref.WeakReference;
import org.puredata.android.scenes.SceneDataBase.SceneColumn;
import android.app.Activity;
import android.app.AlertDialog;
import android.content.Context;
import android.content.DialogInterface;
import android.content.Intent;
import android.database.Cursor;
import android.net.Uri;
import android.os.AsyncTask;
import android.os.Bundle;
import android.view.Menu;
import android.view.MenuInflater;
import android.view.MenuItem;
import android.view.View;
import android.view.View.OnClickListener;
import android.widget.AdapterView;
import android.widget.AdapterView.OnItemClickListener;
import android.widget.AdapterView.OnItemLongClickListener;
import android.widget.Button;
import android.widget.ListView;
import android.widget.Toast;

public class SceneSelection extends Activity implements OnItemClickListener, OnItemLongClickListener, OnClickListener {

	private static final String TAG = "Scene Selection";
	private static final int FILE_SELECT_CODE = 1;
	private ListView sceneView;
	private Button updateButton;
	private SceneDataBase db;
	private Cursor cursor = null;
	private boolean destroyed;
	private Toast toast = null;
	
	private void toast(final String msg) {
		runOnUiThread(new Runnable() {
			@Override
			public void run() {
				if (toast == null) {
					toast = Toast.makeText(getApplicationContext(), "", Toast.LENGTH_SHORT);
				}
				toast.setText(msg);
				toast.show();
			}
		});
	}
	
	@Override
	protected void onCreate(Bundle savedInstanceState) {
		super.onCreate(savedInstanceState);
		initGui();
        try { db = new SceneDataBase(this); }
        catch (RuntimeException e) {
            toast("Cannot open scene library: " + e.getMessage());
            finish();
            return;
        }
        new ImportTask(this, null).execute();
    }

	@Override
	protected void onResume() {
		super.onResume();
		updateList();
	}
	
	@Override
	protected void onDestroy() {
		destroyed = true;
        sceneView.setAdapter(null);
        if (cursor != null) cursor.close();
        if (db != null) db.close();
        super.onDestroy();
	}

	private void initGui() {
		setContentView(R.layout.scene_selection);
		sceneView = (ListView) findViewById(R.id.scene_selection);
		updateButton = new Button(this);
		updateButton.setText(getResources().getString(R.string.update_label));
		sceneView.addFooterView(updateButton);
		sceneView.setOnItemClickListener(this);
		sceneView.setOnItemLongClickListener(this);
		updateButton.setOnClickListener(this);
	}

	private void updateList() {
        if (destroyed || db == null) return;
        Cursor next = null;
        try {
            next = db.getAllScenes();
            if (next == null) throw new IllegalStateException("Cannot read scene library");
            sceneView.setAdapter(new SceneListCursorAdapter(this, next));
            Cursor previous = cursor;
            cursor = next;
            next = null;
            if (previous != null) previous.close();
        } catch (RuntimeException e) {
            toast("Cannot read scene library: " + e.getMessage());
        } finally {
            if (next != null) next.close();
        }
	}
	
	@Override
	public void onItemClick(AdapterView<?> arg0, View v, int position, long id) {
        if (id < 0) return;
		Intent intent = new Intent(this, ScenePlayer.class);
		intent.putExtra(SceneColumn.ID.getLabel(), id);
		startActivity(intent);
	}

	@Override
	public boolean onItemLongClick(AdapterView<?> arg0, View v, int position, final long id) {
        if (id < 0) return false;
		AlertDialog.Builder dialog = new AlertDialog.Builder(this);
		dialog.setIcon(android.R.drawable.ic_dialog_alert);
		dialog.setTitle(getResources().getString(R.string.delete_scene_title));
		dialog.setMessage(getResources().getString(R.string.delete_scene_message));
		dialog.setPositiveButton(getResources().getString(android.R.string.yes), new DialogInterface.OnClickListener() {
			@Override
			public void onClick(DialogInterface dialog, int which) {
				try {
					db.deleteScene(id);
				} catch (IOException | RuntimeException e) {
					toast(getResources().getString(R.string.delete_scene_fail));
				}
				updateList();
			}
		});
		dialog.setNegativeButton(getResources().getString(android.R.string.no), null);
		dialog.show();
		return true;
	}

	@Override
	public void onClick(View v) {
        Intent intent = new Intent(Intent.ACTION_OPEN_DOCUMENT);
        intent.addCategory(Intent.CATEGORY_OPENABLE);
        // Scene archives have inconsistent MIME types; validate the selected content itself.
        intent.setType("*/*");
        intent.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        try { startActivityForResult(intent, FILE_SELECT_CODE); }
        catch (RuntimeException e) { toast("No document picker is available"); }
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == FILE_SELECT_CODE && resultCode == RESULT_OK) {
            Uri uri = data == null ? null : data.getData();
            if (uri == null || !"content".equals(uri.getScheme())) {
                toast("The document provider returned no readable archive");
                return;
            }
            new ImportTask(this, uri).execute();
        }
    }

	@Override
	public boolean onCreateOptionsMenu(Menu menu) {
		MenuInflater inflater = getMenuInflater();
		inflater.inflate(R.menu.selection_menu, menu);
		return true;
	}

	@Override
	public boolean onOptionsItemSelected(MenuItem item) {
		AlertDialog.Builder ad = new AlertDialog.Builder(this);
		int itemId = item.getItemId();
		if (itemId == R.id.selection_about_item) {
			ad.setTitle(R.string.selection_about_title);
			ad.setMessage(R.string.selection_about_msg);
		} else if (itemId == R.id.selection_help_item) {
			ad.setTitle(R.string.selection_help_title);
			ad.setMessage(R.string.selection_help_msg);
		}
		ad.setNeutralButton(android.R.string.ok, null);
		ad.setCancelable(true);
		ad.show();
		return true;
	}

    private static class ImportTask extends AsyncTask<Void, Void, String> {
        private final WeakReference<SceneSelection> owner;
        private final Context context;
        private final Uri uri;

        ImportTask(SceneSelection activity, Uri uri) {
            owner = new WeakReference<SceneSelection>(activity);
            context = activity.getApplicationContext();
            this.uri = uri;
        }

        @Override protected String doInBackground(Void... ignored) {
            boolean seed = uri == null;
            SceneDataBase recovery = null;
            try {
                recovery = new SceneDataBase(context);
                recovery.recoverScenes();
            } catch (Exception e) {
                return "Unable to restore scene library: " + e.getMessage();
            } finally {
                if (recovery != null) recovery.close();
            }
            if (seed && context.getSharedPreferences(TAG, MODE_PRIVATE).getBoolean("defaultSeeded", false)) return null;
            File scene = null;
            boolean imported = false;
            File root = context.getDir("scenes", MODE_PRIVATE);
            try (InputStream in = seed ? context.getResources().openRawResource(R.raw.atsuke)
                    : context.getContentResolver().openInputStream(uri)) {
                File sample = new File(root, "Atsuke.rj");
                if (seed && sample.isDirectory()) {
                    // Recover a process death between archive publication and database insertion.
                    scene = sample;
                } else {
                    scene = SceneArchive.importScene(in, root);
                    imported = true;
                }
                SceneDataBase database = new SceneDataBase(context);
                try { database.addScene(scene); }
                finally { database.close(); }
                if (seed) context.getSharedPreferences(TAG, MODE_PRIVATE).edit().putBoolean("defaultSeeded", true).apply();
                return seed ? null : "Scene imported";
            } catch (Exception e) {
                if (imported && scene != null) {
                    try { SceneArchive.deleteOwnedTree(root, scene); }
                    catch (IOException cleanup) { return "Import failed; private staging cleanup failed: " + cleanup.getMessage(); }
                }
                return "Unable to import scene: " + e.getMessage();
            }
        }

        @Override protected void onPostExecute(String result) {
            SceneSelection activity = owner.get();
            if (activity != null && !activity.destroyed) activity.updateList();
            if (result != null) Toast.makeText(context, result, Toast.LENGTH_LONG).show();
        }
    }
}
