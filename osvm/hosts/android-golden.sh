#!/bin/bash
# Build the clean Android images used by the T5 study, with identical contents for every arm:
#  - redroid: a golden /data tree (/srv/t5/redroid-golden/data), booted once, apps installed, settings applied
#  - emulator: snapshot "clean" in the AndroidWorldAvd (API 33 google_apis x86_64, Pixel 6), same apps and settings
# Apps: AndroidWorld's APKs for Markor, Pro Expense and Simple Calendar Pro (gs://gresearch/android_world, pinned by
# sha256 in /opt/t5/apks/SHA256SUMS). Settings: AndroidWorld's docker setup (animations off, hidden api policy) plus
# stay-awake so the screen never sleeps during an episode.
set -euxo pipefail
SELF=$(readlink -f "$0")
export ANDROID_HOME=/opt/t5/android-sdk ANDROID_SDK_ROOT=/opt/t5/android-sdk ANDROID_AVD_HOME=/opt/t5/avd
A=$ANDROID_HOME/platform-tools/adb
REDROID_IMAGE=${REDROID_IMAGE:-redroid/redroid@sha256:5a42a569ee1d7c71796c0385e906cbaa4c3e0a162a56d9f26b29bdb1befac13b}
APKS="net.gsantner.markor_146.apk com.arduia.expense_11.apk com.simplemobiletools.calendar.pro_238.apk"
if [ -z "${ONLY_EMU:-}" ]; then
mkdir -p /opt/t5/apks && cd /opt/t5/apks
for f in $APKS; do [ -s $f ] || curl -sSfLo $f https://storage.googleapis.com/gresearch/android_world/$f; done
sha256sum $APKS | tee SHA256SUMS
fi

configure() { # $1 = serial
  local s=$1
  for f in $APKS; do $A -s $s install -r -g /opt/t5/apks/$f; done
  $A -s $s shell "settings put global window_animation_scale 0.0; settings put global transition_animation_scale 0.0; settings put global animator_duration_scale 0.0"
  $A -s $s shell "settings put global hidden_api_policy_pre_p_apps 1; settings put global hidden_api_policy_p_apps 1; settings put global hidden_api_policy 1"
  $A -s $s shell "svc power stayon true; settings put system screen_off_timeout 1800000; settings put system screen_brightness 102; settings put system screen_brightness_mode 0; settings put system accelerometer_rotation 0"
  $A -s $s shell "mkdir -p /sdcard/Documents/Markor /sdcard/Download /sdcard/Documents"
  # open each app once so its databases exist (the checkers read them), then close it
  for p in net.gsantner.markor com.arduia.expense com.simplemobiletools.calendar.pro com.android.contacts com.android.settings; do
    $A -s $s shell "am start -n \"\$(cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER $p | tail -1)\"" || echo "WARN: launch $p failed"; sleep 6; $A -s $s shell "am force-stop $p" || true
  done
  $A -s $s shell "ls /data/data/com.arduia.expense/databases/ /data/data/com.simplemobiletools.calendar.pro/databases/" || true
  $A -s $s shell "input keyevent 82; input keyevent 3"
  sleep 10
}

if [ -z "${ONLY_EMU:-}" ]; then
# --- redroid golden /data
G=/srv/t5/redroid-golden
docker rm -f golden-redroid 2>/dev/null || true
rm -rf $G && mkdir -p $G/data
docker run -d --privileged --name golden-redroid -v $G/data:/data -p 127.0.0.1:6099:5555 $REDROID_IMAGE \
  androidboot.redroid_width=1080 androidboot.redroid_height=2400 androidboot.redroid_dpi=420 androidboot.redroid_gpu_mode=guest
for i in $(seq 1 300); do $A connect 127.0.0.1:6099 >/dev/null 2>&1 || true; [ "$($A -s 127.0.0.1:6099 shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ] && break; sleep 1; done
sleep 20
configure 127.0.0.1:6099
docker stop -t 30 golden-redroid && docker rm golden-redroid
$A disconnect 127.0.0.1:6099 || true
du -sh $G/data; $A kill-server || true
fi

# --- emulator snapshot "clean" (the AVD and adb keys belong to ubuntu: run as ubuntu with its own HOME)
if [ "$(id -un)" != ubuntu ]; then
  cp "$SELF" /tmp/android-golden-emu.sh; chmod 755 /tmp/android-golden-emu.sh
  sudo -u ubuntu -H env ONLY_EMU=1 bash /tmp/android-golden-emu.sh
  echo GOLDEN_DONE; exit 0
fi
: > ~/.emulator_console_auth_token; $A kill-server || true
$ANDROID_HOME/emulator/emulator @AndroidWorldAvd -no-window -no-audio -no-boot-anim -no-snapshot-load -gpu swiftshader_indirect -memory 2048 -port 5580 > /tmp/golden-emu.log 2>&1 &
$A -s emulator-5580 wait-for-device
until [ "$($A -s emulator-5580 shell getprop sys.boot_completed 2>/dev/null | tr -d '\r')" = 1 ]; do sleep 1; done
sleep 20
$A -s emulator-5580 root; sleep 3; $A -s emulator-5580 wait-for-device
configure emulator-5580
$A -s emulator-5580 emu avd snapshot save clean
$A -s emulator-5580 emu kill || true
sleep 5
ls -la /opt/t5/avd/AndroidWorldAvd.avd/snapshots/
echo GOLDEN_EMU_DONE
