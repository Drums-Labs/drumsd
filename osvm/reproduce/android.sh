#!/bin/bash
# Reproduce the T5 Android headline on a FRESH m7i.metal-24xl (Ubuntu 24.04, ami-0d6dd9e8265d48906), as user ubuntu
# with passwordless sudo, from a checkout of Drums-Labs/drumsd at branch sota/t5 in ~/drumsd. One command:
#     bash ~/drumsd/osvm/reproduce/android.sh            (about 2.5 h; host cost about USD 3 at spot)
# Steps: host packages + redroid image + Android SDK/AVD + AndroidWorld @55ddaf5; golden /data and emulator snapshot;
# AVD template for the copy-on-write pool; AndroidWorld Docker image (3 documented build fixes); drumsd build;
# measured runs (latency N=16, all four arms; ladder for the chosen backend and AndroidWorld); analysis.
set -euo pipefail
cd ~/drumsd
sudo bash osvm/hosts/android-host-setup.sh > ~/repro-setup.log 2>&1
sudo bash osvm/hosts/android-golden.sh > ~/repro-golden.log 2>&1
# AVD template on the reflink filesystem (copied after the snapshot exists)
sudo rm -rf /srv/t5/xfs/avd-template/AndroidWorldAvd.avd
sudo cp -a /opt/t5/avd/AndroidWorldAvd.avd /srv/t5/xfs/avd-template/ && sudo rm -f /srv/t5/xfs/avd-template/AndroidWorldAvd.avd/*.lock
# AndroidWorld Docker image (osvm/incumbents/androidworld-dockerfile.md)
( cd /opt/t5/android_world && sudo sh -c 'sed -e "s#^FROM openjdk:18-jdk-slim#FROM eclipse-temurin:17-jdk-jammy#" -e "s#^COPY \. /\$#WORKDIR /aw\nCOPY . /aw#" -e "s#^RUN uv pip install \. --system\$#RUN uv pip install . --system \&\& uv pip install --system pillow#" Dockerfile > Dockerfile.t5' \
  && sudo docker build -q -f Dockerfile.t5 -t android_world:55ddaf5 . )
# drumsd
command -v cargo >/dev/null || { curl -sSf https://sh.rustup.rs | sh -s -- -y -q; }
. ~/.cargo/env
[ -f /etc/drumsd-os.token ] || { head -c 32 /dev/urandom | xxd -p -c 64 | sudo tee /etc/drumsd-os.token >/dev/null; sudo chmod 600 /etc/drumsd-os.token; }
sudo cp /etc/drumsd-os.token ~/.drumsd-token; sudo chown ubuntu ~/.drumsd-token
sudo mkdir -p /var/lib/drumsd-os/journal /srv/t5/drums-redroid /srv/t5/drums-emu /srv/t5/drums-emu-cow
mkdir -p ~/t5runs
bash osvm/hosts/redeploy.sh android-host.json android
# measured runs (PLAN.md): latency phase, then the headline ladders
bash osvm/harness/run_measured.sh android latency
bash osvm/harness/run_measured.sh android ladder ${CHOSEN:-a3} 8 16 32 48 64 96 128
bash osvm/harness/run_measured.sh android ladder a0 8 16 32 48 64 96 128
python3 osvm/harness/analyze.py ~/t5runs/measured --md ~/t5runs/measured/TABLES.md --json ~/t5runs/measured/summary.json
echo REPRODUCE_ANDROID_DONE
