#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
classes=$(mktemp -d)
trap 'rm -rf "$classes"' EXIT HUP INT TERM
javac_cmd=javac
java_cmd=java
if [ -n "${JAVA_HOME:-}" ]; then
    javac_cmd="$JAVA_HOME/bin/javac"
    java_cmd="$JAVA_HOME/bin/java"
fi
"$javac_cmd" -d "$classes" \
    src/org/puredata/android/scenes/SceneArchive.java \
    src/org/puredata/android/scenes/ScenePlayback.java \
    test/org/puredata/android/scenes/SceneArchiveTest.java \
    test/org/puredata/android/scenes/ScenePlaybackCheck.java
"$java_cmd" -ea -cp "$classes" org.puredata.android.scenes.SceneArchiveTest res/raw/atsuke.zip
"$java_cmd" -ea -cp "$classes" org.puredata.android.scenes.ScenePlaybackCheck
