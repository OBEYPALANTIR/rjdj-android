/**
 * 
 * @author Martin Roth (mhroth@rjdj.me)
 * @author Peter Brinkmann (peter.brinkmann@gmail.com)
 * 
 * For information on usage and redistribution, and for a DISCLAIMER OF ALL
 * WARRANTIES, see the file, "LICENSE.txt," in this distribution.
 * 
 */

package org.puredata.android.scenes;

import java.io.File;

import org.puredata.android.scenes.SceneDataBase.RecordingColumn;
import org.puredata.android.scenes.SceneDataBase.SceneColumn;

import android.app.Activity;
import android.app.AlertDialog;
import android.content.DialogInterface;
import android.content.Intent;
import android.database.Cursor;
import android.graphics.BitmapFactory;
import android.media.AudioManager;
import android.media.AudioAttributes;
import android.media.AudioFocusRequest;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.IntentFilter;
import android.os.Handler;
import android.os.Looper;
import android.media.MediaPlayer;
import android.os.Bundle;
import android.text.format.DateFormat;
import android.view.View;
import android.view.View.OnClickListener;
import android.widget.CompoundButton;
import android.widget.CompoundButton.OnCheckedChangeListener;
import android.widget.EditText;
import android.widget.ImageButton;
import android.widget.ImageView;
import android.widget.SeekBar;
import android.widget.SeekBar.OnSeekBarChangeListener;
import android.widget.TextView;
import android.widget.Toast;
import android.widget.ToggleButton;

public class RecordingPlayer extends Activity implements OnSeekBarChangeListener, OnCheckedChangeListener, OnClickListener {

	private SceneDataBase db;
	private long recordingId;
	private long sceneId;
	private String description;
	private ToggleButton playButton;
	private ImageButton sceneButton;
	private ImageButton editButton;
	private TextView descriptionView;
	private SeekBar seekBar;
	private MediaPlayer mediaPlayer;
	private boolean playbackState;
	private final Handler handler = new Handler(Looper.getMainLooper());
    private boolean prepared, resumed, sceneAvailable;
    private AudioManager audioManager;
    private AudioFocusRequest focus;
    private final BroadcastReceiver noisy = new BroadcastReceiver() {
        @Override public void onReceive(Context context, Intent intent) { pausePlayback(); }
    };
    private final Runnable update = new Runnable() {
        @Override public void run() {
            if (prepared && mediaPlayer.isPlaying()) {
                seekBar.setProgress(mediaPlayer.getCurrentPosition());
                handler.postDelayed(this, 250);
            }
        }
    };
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
	public void onCreate(Bundle savedInstanceState) {
		super.onCreate(savedInstanceState);
		this.setContentView(R.layout.recording_player);
        SceneWindow.fitSystemBars(this);
        audioManager = (AudioManager) getSystemService(AUDIO_SERVICE);
        focus = new AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                .setAudioAttributes(new AudioAttributes.Builder().setUsage(AudioAttributes.USAGE_MEDIA)
                        .setContentType(AudioAttributes.CONTENT_TYPE_MUSIC).build())
                .setOnAudioFocusChangeListener(change -> {
                    if (change != AudioManager.AUDIOFOCUS_GAIN) pausePlayback();
                }, handler).build();
        registerReceiver(noisy, new IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY), RECEIVER_NOT_EXPORTED);
		db = new SceneDataBase(this);
		recordingId = this.getIntent().getLongExtra(RecordingColumn.ID.getLabel(), -1);
		if (recordingId >= 0) {
			Cursor cursor = db.getRecording(recordingId);
            if (cursor == null || !cursor.moveToFirst()) {
                if (cursor != null) cursor.close();
                toast(getString(R.string.no_such_recording)); finish(); return;
            }
			sceneId = SceneDataBase.getLong(cursor, RecordingColumn.SCENE_ID);
			displaySceneInfo(sceneId);
			seekBar = (SeekBar) findViewById(R.id.recording_play_seekbar);
			seekBar.setMax((int) SceneDataBase.getLong(cursor, RecordingColumn.RECORDING_DURATION));
			seekBar.setOnSeekBarChangeListener(this);
			descriptionView = (TextView) findViewById(R.id.recording_play_description_text);
			description = SceneDataBase.getString(cursor, RecordingColumn.RECORDING_DESCRIPTION);
			descriptionView.setText(description);
			TextView textView = (TextView) findViewById(R.id.recording_play_duration_text);
			String durationFormat = this.getString(R.string.duration_format);
			textView.setText(DateFormat.format(durationFormat, SceneDataBase.getLong(cursor, RecordingColumn.RECORDING_DURATION)));
			textView = (TextView) findViewById(R.id.recording_play_date_text);
			String dateFormat = this.getString(R.string.date_format);
			textView.setText(DateFormat.format(dateFormat, SceneDataBase.getLong(cursor, RecordingColumn.RECORDING_TIMESTAMP)));

			playButton = (ToggleButton) findViewById(R.id.recording_play_play_button);
			playButton.setChecked(false);
            playButton.setEnabled(false);
            playButton.setOnCheckedChangeListener(this);
			sceneButton = (ImageButton) findViewById(R.id.recording_play_scene_button);
			sceneButton.setOnClickListener(this);
            sceneButton.setEnabled(sceneAvailable);
			editButton = (ImageButton) findViewById(R.id.recording_play_rename_button);
			editButton.setOnClickListener(this);
			mediaPlayer = new MediaPlayer();
			mediaPlayer.setAudioStreamType(AudioManager.STREAM_MUSIC);
			mediaPlayer.setLooping(true);
			String recPath = SceneDataBase.getString(cursor, RecordingColumn.RECORDING_PATH);
			cursor.close();
			try {
				mediaPlayer.setDataSource(recPath);
				mediaPlayer.setOnPreparedListener(player -> {
                    if (isFinishing() || isDestroyed()) return;
                    prepared = true;
                    seekBar.setMax(player.getDuration());
                    playButton.setEnabled(true);
                });
                mediaPlayer.setOnErrorListener((player, what, extra) -> {
                    prepared = false;
                    handler.removeCallbacks(update);
                    audioManager.abandonAudioFocusRequest(focus);
                    playButton.setChecked(false);
                    toast(getString(R.string.open_recording_fail));
                    finish(); return true;
                });
                mediaPlayer.prepareAsync();
			} catch (Exception e) {
				toast(getResources().getString(R.string.open_recording_fail));
				finish();
			}
		} else {
			toast(getResources().getString(R.string.no_such_recording));
			finish();
		}
	}
	
	private void displaySceneInfo(long sceneId) {
		Cursor cursor = db.getScene(sceneId);
		if (cursor != null && cursor.moveToFirst()) {
            sceneAvailable = true;
			TextView textView = (TextView) findViewById(R.id.recording_play_title);
			textView.setText(SceneDataBase.getString(cursor, SceneColumn.SCENE_TITLE));
			textView = (TextView) findViewById(R.id.recording_play_artist);
			textView.setText(SceneDataBase.getString(cursor, SceneColumn.SCENE_ARTIST));
			String scenePath = SceneDataBase.getString(cursor, SceneColumn.SCENE_DIRECTORY);
			ImageView imageView = (ImageView) findViewById(R.id.recording_play_image);
			imageView.setImageBitmap(BitmapFactory.decodeFile(new File(scenePath, "image.jpg").getAbsolutePath()));
        } else {
            ((TextView) findViewById(R.id.recording_play_title)).setText("Scene deleted");
        }
        if (cursor != null) cursor.close();
    }

	@Override
	protected void onResume() {
		super.onResume();
        resumed = true;
	}
	
	@Override
	protected void onPause() {
		super.onPause();
		resumed = false;
        pausePlayback();
	}
	
	@Override
	protected void onDestroy() {
		super.onDestroy();
        handler.removeCallbacks(update);
        if (mediaPlayer != null) { mediaPlayer.release(); mediaPlayer = null; }
        prepared = false;
        audioManager.abandonAudioFocusRequest(focus);
        unregisterReceiver(noisy);
		db.close();
	}

    private void startPlayback() {
        if (!prepared || !resumed) { if (playButton != null) playButton.setChecked(false); return; }
        if (audioManager.requestAudioFocus(focus) != AudioManager.AUDIOFOCUS_REQUEST_GRANTED) {
            playButton.setChecked(false); toast("Audio focus unavailable. Tap Play to retry."); return;
        }
        mediaPlayer.start();
        handler.removeCallbacks(update);
        handler.post(update);
    }

    private void stopPlayback() {
        handler.removeCallbacks(update);
        if (prepared && mediaPlayer.isPlaying()) mediaPlayer.pause();
        audioManager.abandonAudioFocusRequest(focus);
    }

    private void pausePlayback() {
        playbackState = false;
        stopPlayback();
        if (playButton != null) playButton.setChecked(false);
    }

	public void onProgressChanged(SeekBar seekBar, int progress, boolean fromUser) {
		if (fromUser && prepared) {
			mediaPlayer.seekTo(progress);
		}
	}

	public void onStartTrackingTouch(SeekBar seekBar) {
		playbackState = prepared && mediaPlayer.isPlaying();
		stopPlayback();
	}

	public void onStopTrackingTouch(SeekBar seekBar) {
		if (playbackState) {
			startPlayback();
		}
	}
	
	public void onCheckedChanged(CompoundButton buttonView, boolean isChecked) {
		if (isChecked) {
			startPlayback();
		} else {
			stopPlayback();
		}
	}

	public void onClick(View v) {
		if (v.equals(sceneButton)) {
			Intent intent = new Intent(RecordingPlayer.this, ScenePlayer.class);
			intent.putExtra(SceneColumn.ID.getLabel(), sceneId);
			startActivity(intent);
		} else if (v.equals(editButton)) {
			editDescription();
		}
	}
	
	private void editDescription() {
		AlertDialog.Builder dialog = new AlertDialog.Builder(this);
		dialog.setIcon(R.drawable.transport_edit_description);
		dialog.setTitle(getResources().getString(R.string.description));
		dialog.setMessage(getResources().getString(R.string.edit_description));
		final EditText editText = new EditText(this);
		editText.setText(description);
		dialog.setView(editText);
		dialog.setPositiveButton(getResources().getString(android.R.string.ok), new DialogInterface.OnClickListener() {
			@Override
			public void onClick(DialogInterface dialog, int which) {
				description = editText.getText().toString();
				descriptionView.setText(description);
				db.setRecordingDescription(recordingId, description);
			}
		});
		dialog.setNegativeButton(getResources().getString(android.R.string.cancel), null);
		dialog.show();
	}

}
