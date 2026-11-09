#!/bin/bash
# Android host provisioning for the T5 OS-coverage study (Ubuntu 24.04, x86_64, /dev/kvm present).
# Installs: docker + redroid image (Drums android backend), Android SDK emulator + AndroidWorld AVD (incumbent arm),
# AndroidWorld at a pinned commit. Idempotent. Writes versions to /opt/t5/versions.json.
set -euxo pipefail
REDROID_IMAGE=${REDROID_IMAGE:-redroid/redroid:13.0.0_64only-latest}
AW_COMMIT=${AW_COMMIT:-main}
mkdir -p /opt/t5 && cd /opt/t5
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q docker.io unzip openjdk-17-jre-headless python3-venv python3-pip sqlite3 jq git curl \
  qemu-utils libpulse0 libgl1 libnss3 libxcomposite1 libxcursor1 libxi6 libxtst6 libasound2t64 libxkbfile1 cpu-checker
# binder for redroid (Android 12+ uses memfd; no ashmem needed)
modprobe binder_linux devices="binder,hwbinder,vndbinder"
echo 'binder_linux' > /etc/modules-load.d/binder.conf
echo 'options binder_linux devices="binder,hwbinder,vndbinder"' > /etc/modprobe.d/binder.conf
usermod -aG docker,kvm ubuntu || true
docker pull "$REDROID_IMAGE"
REDROID_DIGEST=$(docker inspect --format '{{index .RepoDigests 0}}' "$REDROID_IMAGE")
# Android SDK (cmdline-tools version from Google's repository index)
export ANDROID_HOME=/opt/t5/android-sdk
if [ ! -x $ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager ]; then
  z=$(curl -s https://dl.google.com/android/repository/repository2-3.xml | grep -o 'commandlinetools-linux-[0-9]*_latest.zip' | sort -t- -k3 -n | tail -1)
  curl -sSLo cl.zip https://dl.google.com/android/repository/$z
  mkdir -p $ANDROID_HOME/cmdline-tools && unzip -q -o cl.zip -d $ANDROID_HOME/cmdline-tools && rm -rf $ANDROID_HOME/cmdline-tools/latest && mv $ANDROID_HOME/cmdline-tools/cmdline-tools $ANDROID_HOME/cmdline-tools/latest
  echo "$z" > cmdline-tools.txt
fi
SDK=$ANDROID_HOME/cmdline-tools/latest/bin/sdkmanager
yes | $SDK --licenses >/dev/null || true
$SDK "platform-tools" "emulator" "platforms;android-33" "system-images;android-33;google_apis;x86_64"
# AndroidWorld's AVD: Pixel 6, API 33 (Tiramisu), name AndroidWorldAvd (android_world README)
echo no | $ANDROID_HOME/cmdline-tools/latest/bin/avdmanager create avd -f -n AndroidWorldAvd -k "system-images;android-33;google_apis;x86_64" -d pixel_6
chown -R ubuntu:ubuntu /opt/t5 /home/ubuntu/.android 2>/dev/null || true
# AndroidWorld, pinned
[ -d android_world ] || git clone -q https://github.com/google-research/android_world.git
(cd android_world && git checkout -q "$AW_COMMIT" && git rev-parse HEAD > ../aw_commit.txt)
python3 -m venv /opt/t5/awenv
/opt/t5/awenv/bin/pip -q install -U pip
/opt/t5/awenv/bin/pip -q install -r android_world/requirements.txt || true
(cd android_world && /opt/t5/awenv/bin/pip -q install -e . ) || true
jq -n --arg redroid "$REDROID_DIGEST" --arg aw "$(cat aw_commit.txt)" --arg cl "$(cat cmdline-tools.txt 2>/dev/null)" \
  --arg emu "$($ANDROID_HOME/emulator/emulator -version 2>/dev/null | head -1)" --arg kernel "$(uname -r)" \
  --arg sysimg "$(grep -h Pkg.Revision $ANDROID_HOME/system-images/android-33/google_apis/x86_64/source.properties)" \
  '{redroid_image:$redroid, android_world_commit:$aw, cmdline_tools:$cl, emulator:$emu, kernel:$kernel, system_image_api33:$sysimg}' > versions.json
cat versions.json
echo SETUP_DONE
# --- added for drumsd's emulator pools (T5): empty console token (drumsd runs as root and talks to the console),
# reflink-capable XFS for per-desktop AVD clones (android-emulator-cow pool)
: > /root/.emulator_console_auth_token
modprobe xfs; mkdir -p /srv/t5/xfs
[ -f /srv/t5/xfs.img ] || { truncate -s 350G /srv/t5/xfs.img; mkfs.xfs -q -f -m reflink=1 /srv/t5/xfs.img; }
mountpoint -q /srv/t5/xfs || mount -o loop,noatime /srv/t5/xfs.img /srv/t5/xfs
mkdir -p /srv/t5/xfs/avd-template /srv/t5/xfs/clones
