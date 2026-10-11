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
import java.util.LinkedHashMap;
import java.util.Map;

import org.puredata.android.scenes.SceneDataBase.SceneColumn;
import org.puredata.android.service.PdService;
import org.puredata.core.PdBase;
import org.puredata.core.PdListener;
import org.puredata.android.io.AudioParameters;
import org.puredata.android.io.PdAudio;
import org.puredata.android.utils.PdUiDispatcher;

import android.Manifest;
import android.content.BroadcastReceiver;
import android.content.IntentFilter;
import android.content.pm.PackageManager;
import android.content.res.Configuration;
import android.net.Uri;
import android.provider.Settings;
import android.media.AudioAttributes;
import android.media.AudioFocusRequest;
import android.media.AudioManager;
import android.os.Handler;
import android.os.Looper;
import android.app.Activity;
import android.app.AlertDialog;
import android.app.ProgressDialog;
import android.content.ComponentName;
import android.content.Context;
import android.content.Intent;
import android.content.ServiceConnection;
import android.database.Cursor;
import android.graphics.BitmapFactory;
import android.graphics.drawable.Drawable;
import android.hardware.Sensor;
import android.hardware.SensorEvent;
import android.hardware.SensorEventListener;
import android.hardware.SensorManager;
import android.os.Bundle;
import android.os.IBinder;
import android.util.Log;
import android.view.MotionEvent;
import android.view.View;
import android.view.View.OnClickListener;
import android.view.View.OnTouchListener;
import android.widget.ImageButton;
import android.widget.ImageView;
import android.widget.SeekBar;
import android.widget.SeekBar.OnSeekBarChangeListener;
import android.widget.TextView;
import android.widget.Toast;
import android.widget.ToggleButton;


public class ScenePlayer extends Activity implements SensorEventListener, OnTouchListener, OnClickListener, OnSeekBarChangeListener {

	private static final String TAG = "Pd Scene Player";
	private static final String RJ_IMAGE_ANDROID = "rj_image_android";
	private static final String RJ_TEXT_ANDROID = "rj_text_android";
	private static final String TRANSPORT = "#transport";
	private static final String ACCELERATE = "#accelerate";
	private static final String MICVOLUME = "#micvolume";
    private static final int MICROPHONE_REQUEST = 1;
    private final Handler main = new Handler(Looper.getMainLooper());
    private boolean destroyed, resumed, bound, wantsPlayback;
    private File playbackFolder;
    private Thread preparation;
    private AudioManager audioManager;
    private AudioFocusRequest focus;
    private final Map<String, Overlay> overlays = new LinkedHashMap<>();
    private final BroadcastReceiver noisy = new BroadcastReceiver() {
        @Override public void onReceive(Context context, Intent intent) { stopAudio(); }
    };
	private SceneDataBase db;
	private ProgressDialog progress = null;
	private SceneView sceneView;
	private ToggleButton play;
	private ToggleButton record;
	private ImageButton info;
	private SeekBar micVolume;
	private int micValue;
	private File sceneFolder;
	private File recDir = null;
	private String recFile = null;
	private long recStart;
	private long sceneId;
	private String artist;
	private String title;
	private String description;
	private PdService pdService = null;
	private int patch = 0;
	
	private final PdUiDispatcher dispatcher = new PdUiDispatcher() {
		@Override
		public void print(String s) {
			post(s);
		}
	};

	private void post(final String msg) {
		Log.i(TAG, msg);
	}

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

	private final PdListener overlayListener = new PdListener.Adapter() {

		@Override
		public void receiveList(String source, Object... args) {
			if (destroyed || args.length < 2 || !(args[0] instanceof String) || !(args[1] instanceof String)) return;
			String key = (String) args[0];
			String cmd = (String) args[1];
			if (overlays.containsKey(key)) {
				Overlay overlay = overlays.get(key);
				if (cmd.equals("visible")) {
					if (args.length < 3 || !(args[2] instanceof Float))	return;
					boolean flag = ((Float) args[2]).floatValue() > 0.5f;
					overlay.setVisible(flag);
				} else if (cmd.equals("move")) {
					if (args.length < 4 || !(args[2] instanceof Float) || !(args[3] instanceof Float)) return;
					float x = ((Float) args[2]).floatValue();
					float y = ((Float) args[3]).floatValue();
					overlay.setPosition(x, y);
				} else {
					if (overlay instanceof TextOverlay) {
						TextOverlay textOverlay = (TextOverlay) overlay;
						if (cmd.equals("text")) {
							if (args.length < 3 || !(args[2] instanceof String)) return;
							textOverlay.setText((String) args[2]);
						} else if (cmd.equals("size")) {
							if (args.length < 3 || !(args[2] instanceof Float)) return;
							textOverlay.setSize(((Float) args[2]).floatValue());
						}
					} else {
						if (args.length < 3 || !(args[2] instanceof Float)) return;
						ImageOverlay imgOverlay = (ImageOverlay) overlay;
						float val = ((Float) args[2]).floatValue();
						if (cmd.equals("ref")) {
							boolean flag = val > 0.5f;
							imgOverlay.setCentered(flag);
						} else if (cmd.equals("scale")) {
							if (args.length < 4 || !(args[3] instanceof Float)) return;
							float sy = ((Float) args[3]).floatValue();
							imgOverlay.setScale(val, sy);
						} else if (cmd.equals("rotate")) {
							imgOverlay.setAngle(val);
						} else if (cmd.equals("alpha")) {
							imgOverlay.setAlpha(val);
						}
					}
				}
			} else {
				if (args.length < 3 || !(args[2] instanceof String)) return;
				String arg = (String) args[2];
				Overlay overlay;
				if (cmd.equals("load")) {
					try {
                        overlay = new ImageOverlay(ScenePlayback.containedFile(playbackFolder, arg).getAbsolutePath());
                    } catch (IOException e) { post("Rejected scene image: " + arg); return; }
				} else if (cmd.equals("text")) {
					overlay = new TextOverlay(arg);
				} else return;
				sceneView.addOverlay(overlay);
				overlays.put(key, overlay);
			}
		}
	};

	private final ServiceConnection serviceConnection = new ServiceConnection() {
		@Override
		public void onServiceConnected(ComponentName name, IBinder service) {
            if (destroyed) return;
            pdService = ((PdService.PdBinder) service).getService();
            PdBase.setReceiver(dispatcher);
            dispatcher.addListener(RJ_IMAGE_ANDROID, overlayListener);
            dispatcher.addListener(RJ_TEXT_ANDROID, overlayListener);
            if (wantsPlayback && resumed) startAudio();
		}

		@Override
		public void onServiceDisconnected(ComponentName name) {
			pdService = null;
            stopAudio();
		}
	};

	@Override
	protected void onCreate(Bundle savedInstanceState) {
		super.onCreate(savedInstanceState);
		db = new SceneDataBase(this);
		Intent intent = getIntent();
		sceneId = intent.getLongExtra(SceneColumn.ID.getLabel(), -1);
		if (sceneId >= 0) {
			Cursor cursor = db.getScene(sceneId);
			if (cursor == null || !cursor.moveToFirst()) {
                if (cursor != null) cursor.close();
                toast("Scene is no longer available"); finish(); return;
            }
            String scenePath = SceneDataBase.getString(cursor, SceneColumn.SCENE_DIRECTORY);
			artist = SceneDataBase.getString(cursor, SceneColumn.SCENE_ARTIST);
			title = SceneDataBase.getString(cursor, SceneColumn.SCENE_TITLE);
			description = SceneDataBase.getString(cursor, SceneColumn.SCENE_INFO);
			cursor.close();
			micValue = getPreferences(MODE_PRIVATE).getInt(MICVOLUME, 100);
			progress = new ProgressDialog(this);
			progress.setCancelable(false);
			progress.setIndeterminate(true);
			progress.setMessage("Loading scene...");
			progress.show();
			sceneFolder = new File(scenePath);
			recDir = new File(getFilesDir(), "recordings");
			if (recDir.isFile() || (!recDir.exists() && !recDir.mkdirs())) recDir = null;
			initGui();
			initSystemServices();
			initPdService();
		} else {
			Log.e(TAG, "launch intent without scene ID");
			finish();
		}
	}

    private void initPdService() {
        preparation = new Thread(() -> {
            try {
                final File copy = ScenePlayback.prepare(sceneFolder, getCacheDir());
                main.post(() -> {
                    if (destroyed) { ScenePlayback.delete(copy); return; }
                    playbackFolder = copy;
                    play.setEnabled(true);
                    dismissProgressDialog();
                });
            } catch (IOException e) {
                main.post(() -> {
                    if (!destroyed) { toast("Unable to prepare scene: " + e.getMessage()); finish(); }
                });
            }
        }, "scene-copy");
        preparation.start();
    }

    private void initSystemServices() {
        audioManager = (AudioManager) getSystemService(AUDIO_SERVICE);
        focus = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                .setAudioAttributes(new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC).build())
                .setOnAudioFocusChangeListener(change -> {
                    if (change != AudioManager.AUDIOFOCUS_GAIN) stopAudio();
                }, main).build();
        if (android.os.Build.VERSION.SDK_INT >= 33) {
            registerReceiver(noisy, new IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY), RECEIVER_NOT_EXPORTED);
        } else {
            registerReceiver(noisy, new IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY));
        }
    }

	@Override
	public void onSensorChanged(SensorEvent event) {
		final float q = 1.0f / SensorManager.GRAVITY_EARTH;  // convert acceleration units from m/s^2 to g
		if (pdService != null && pdService.isRunning()) PdBase.sendList(ACCELERATE, event.values[0] * q, -event.values[1] * q, -event.values[2] * q);
		/**
		 * Explanation:  Observation of RjDj patches suggests that the z-axis points
		 * downward on iPhones.  Since I'm pretty sure that the coordinate system is
		 * supposed to be right-handed and that the x-axis points right, I've concluded
		 * that the way to convert between Android and iPhone accelerometer values is to
		 * flip the sign of the y and z coordinates.
		 */
	}

	@Override
	public void onAccuracyChanged(Sensor sensor, int accuracy) {
		// don't care
	}

	@Override
	public boolean onTouch(View v, MotionEvent event) {
		if (v == sceneView && event.getActionMasked() == MotionEvent.ACTION_UP) {
			v.performClick();
		}
		return (v == sceneView) && VersionedTouch.evaluateTouch(event, sceneView.getWidth(), sceneView.getHeight());
	}

	@Override
	protected void onPause() {
		resumed = false;
        if (pdService == null || !pdService.isRunning()) {
            wantsPlayback = false;
            if (play != null) play.setChecked(false);
        }
		getPreferences(MODE_PRIVATE).edit().putInt(MICVOLUME, micValue).apply();
		dismissProgressDialog();
		super.onPause();
	}

    @Override protected void onResume() { super.onResume(); resumed = true; }

    @Override public void onConfigurationChanged(Configuration configuration) {
        super.onConfigurationChanged(configuration);
        if (sceneFolder != null && !destroyed) initGui();
    }

    @Override protected void onStop() {
        super.onStop();
        if (isFinishing()) stopAudio();
    }

    @Override public void onRequestPermissionsResult(int requestCode, String[] permissions, int[] results) {
        super.onRequestPermissionsResult(requestCode, permissions, results);
        if (requestCode == MICROPHONE_REQUEST) {
            wantsPlayback = false;
            play.setChecked(false);
            boolean granted = checkSelfPermission(Manifest.permission.RECORD_AUDIO) == PackageManager.PERMISSION_GRANTED;
            if (results.length > 0) getPreferences(MODE_PRIVATE).edit().putBoolean("microphoneDenied", !granted).apply();
            if (!granted && results.length > 0 && !shouldShowRequestPermissionRationale(Manifest.permission.RECORD_AUDIO)) {
                showMicrophoneSettings();
            } else {
                toast(granted ? "Microphone allowed. Tap Play to start." : "Microphone is required. Tap Play to try again.");
            }
        }
    }

	@Override
	protected void onDestroy() {
		destroyed = true;
        dismissProgressDialog();
        if (preparation != null) preparation.interrupt();
		cleanup();
        super.onDestroy();
		db.close();
	}

	private void initGui() {
		setContentView(R.layout.scene_player);
        SceneWindow.fitSystemBars(this);
		TextView tv = (TextView) findViewById(R.id.sceneplayer_title);
		tv.setText(title);
		tv = (TextView) findViewById(R.id.sceneplayer_artist);
		tv.setText(artist);
		sceneView = (SceneView) findViewById(R.id.sceneplayer_pic);
		sceneView.setOnTouchListener(this);
		sceneView.setImageBitmap(BitmapFactory.decodeFile(new File(sceneFolder, "image.jpg").getAbsolutePath()));
		for (Overlay overlay : overlays.values()) sceneView.addOverlay(overlay);
		play = (ToggleButton) findViewById(R.id.sceneplayer_pause);
		play.setChecked(wantsPlayback || (pdService != null && pdService.isRunning()));
        play.setEnabled(playbackFolder != null);
		play.setOnClickListener(this);
		record = (ToggleButton) findViewById(R.id.sceneplayer_record);
		record.setChecked(recFile != null);
		record.setOnClickListener(this);
		info = (ImageButton) findViewById(R.id.sceneplayer_info);
		info.setOnClickListener(this);
		micVolume = (SeekBar) findViewById(R.id.mic_volume);
		micVolume.setProgress(micValue);
		micVolume.setOnSeekBarChangeListener(this);
	}


	private void dismissProgressDialog() {
		if (progress!= null) progress.dismiss();
	}

    private void cleanup() {
        stopAudio();
        if (patch != 0) { PdBase.closePatch(patch); patch = 0; }
        dispatcher.release();
        if (bound) { unbindService(serviceConnection); bound = false; }
        pdService = null;
        if (audioManager != null) unregisterReceiver(noisy);
        ScenePlayback.delete(playbackFolder);
    }

	@Override
	public void onClick(View v) {
		if (v.equals(play)) {
			if (play.isChecked()) {
				startAudio();
			} else {
				stopAudio();
			}
		} else if (v.equals(record)) {
			if (record.isChecked()) {
				startRecording();
			} else {
				stopRecording();
			}
		} else if (v.equals(info)) {
			showInfo();
		}
	}

	private void startRecording() {
		if (recDir == null || pdService == null || !pdService.isRunning()) {
			record.setChecked(false);
			return;
		}
		recStart = System.currentTimeMillis();
		String fileName = "recording_" + recStart + ".wav";
		recFile = new File(recDir, fileName).getAbsolutePath();
		PdBase.sendMessage(TRANSPORT, "scene", recFile);
		PdBase.sendMessage(TRANSPORT, "record", 1);
		post("Recording...");
	}

	private void stopRecording() {
		if (recFile == null) return;
		PdBase.sendMessage(TRANSPORT, "record", 0);
		long duration = System.currentTimeMillis() - recStart;
        long recordingId = db.addRecording(recFile, recStart, duration, 0.0, 0.0, sceneId);
        record.setChecked(false);
		recFile = null;
        if (recordingId < 0) toast(getString(R.string.recording_not_saved));
        else post("Finished recording");
	}

    private void startAudio() {
        if (destroyed || !resumed || playbackFolder == null) { play.setChecked(false); return; }
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED
                && getPreferences(MODE_PRIVATE).getBoolean("microphoneDenied", false)
                && !shouldShowRequestPermissionRationale(Manifest.permission.RECORD_AUDIO)) {
            play.setChecked(false);
            showMicrophoneSettings();
            return;
        }
        if (android.os.Build.VERSION.SDK_INT >= 33
                && checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) != PackageManager.PERMISSION_GRANTED
                && !getPreferences(MODE_PRIVATE).getBoolean("notificationRequested", false)) {
            getPreferences(MODE_PRIVATE).edit().putBoolean("notificationRequested", true).apply();
            play.setChecked(false);
            requestPermissions(new String[] { Manifest.permission.RECORD_AUDIO, Manifest.permission.POST_NOTIFICATIONS }, MICROPHONE_REQUEST);
            return;
        }
        if (checkSelfPermission(Manifest.permission.RECORD_AUDIO) != PackageManager.PERMISSION_GRANTED) {
            play.setChecked(false);
            requestPermissions(new String[] { Manifest.permission.RECORD_AUDIO }, MICROPHONE_REQUEST);
            return;
        }
        wantsPlayback = true;
        if (pdService == null) {
            if (!bound) bound = bindService(new Intent(this, PdService.class), serviceConnection, BIND_AUTO_CREATE);
            if (!bound) { stopAudio(); toast("Unable to connect audio service"); }
            return;
        }
        if (pdService.isRunning()) return;
        if (audioManager.requestAudioFocus(focus) != AudioManager.AUDIOFOCUS_REQUEST_GRANTED) {
            stopAudio(); toast("Audio focus unavailable. Tap Play to retry."); return;
        }
        try {
            int rate = AudioParameters.suggestSampleRate();
            if (rate <= 0 || !AudioParameters.checkParameters(rate, 1, 2))
                throw new IOException("Device does not support microphone and stereo audio");
            pdService.initAudio(rate, 1, 2, -1);
            if (patch == 0) patch = PdBase.openPatch(new File(playbackFolder, "_main.pd"));
            adjustMicVolume(micValue);
            Intent returnIntent = new Intent(this, ScenePlayer.class)
                    .addFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP | Intent.FLAG_ACTIVITY_CLEAR_TOP)
                    .putExtra(SceneColumn.ID.getLabel(), sceneId);
            pdService.startAudio(returnIntent, R.drawable.sceneplayer_notify, title + " by " + artist, "Return to scene");
            PdBase.sendMessage(TRANSPORT, "play", 1);
            SensorManager sm = (SensorManager) getSystemService(SENSOR_SERVICE);
            sm.registerListener(this, sm.getDefaultSensor(Sensor.TYPE_ACCELEROMETER), SensorManager.SENSOR_DELAY_GAME);
            play.setChecked(true);
        } catch (IOException | RuntimeException e) {
            stopAudio(); toast("Unable to start microphone audio: " + e.getMessage());
        }
    }

    private void showMicrophoneSettings() {
        new AlertDialog.Builder(this)
                .setTitle(R.string.microphone_access)
                .setMessage(R.string.microphone_settings_message)
                .setNegativeButton(android.R.string.cancel, null)
                .setPositiveButton(R.string.open_settings, (dialog, which) -> {
                    try {
                        startActivity(new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                                Uri.parse("package:" + getPackageName())));
                    } catch (RuntimeException e) { toast(getString(R.string.settings_unavailable)); }
                }).show();
    }

    private void stopAudio() {
        wantsPlayback = false;
        stopRecording();
        if (pdService != null) {
            PdBase.sendMessage(TRANSPORT, "play", 0);
            pdService.stopAudio();
            PdAudio.release();
        }
        ((SensorManager) getSystemService(SENSOR_SERVICE)).unregisterListener(this);
        if (audioManager != null) audioManager.abandonAudioFocusRequest(focus);
        if (play != null) play.setChecked(false);
    }

	private void showInfo() {
		AlertDialog.Builder ad = new AlertDialog.Builder(this);
		View header = View.inflate(this, R.layout.two_line_dialog_title, null);
		((TextView) header.findViewById(android.R.id.text1)).setText(title);
		((TextView) header.findViewById(android.R.id.text2)).setText(artist);
		File pic = new File(sceneFolder, "thumb.jpg");
		if (!pic.exists()) pic = new File(sceneFolder, "image.jpg");
		if (pic.exists()) {
			ImageView sceneThumbnail = (ImageView) header.findViewById(android.R.id.selectedIcon);
			sceneThumbnail.setImageDrawable(Drawable.createFromPath(pic.getAbsolutePath()));
		}
		ad.setCustomTitle(header);
		ad.setMessage(description);
		ad.setNeutralButton(android.R.string.ok, null);
		ad.setCancelable(true);
		ad.show();
	}

	@Override
	public void onProgressChanged(SeekBar seekBar, int progress, boolean fromUser) {
		micValue = progress;
		adjustMicVolume(progress);
	}

	public void adjustMicVolume(int vol) {
		float q = vol * 0.01f;
		float volume = q * q * q * q;  // fourth power of mic volume slider value; somewhere between linear and exponential
		PdBase.sendFloat(MICVOLUME, volume);
	}

	@Override
	public void onStartTrackingTouch(SeekBar seekBar) {
		// don't care
	}

	@Override
	public void onStopTrackingTouch(SeekBar seekBar) {
		// don't care
	}
}
