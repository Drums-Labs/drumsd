#!/bin/bash
# Reproduce the T5 Windows headline on a FRESH m7i.metal-24xl (Ubuntu 24.04, ami-0d6dd9e8265d48906) with an extra
# >= 1 TB gp3 volume attached (WAA per-desktop storage copies), as user ubuntu with passwordless sudo, from a checkout
# of Drums-Labs/drumsd at branch sota/t5 in ~/drumsd. One command:
#     bash ~/drumsd/osvm/reproduce/windows.sh            (about 3 h, of which about 45 min is WAA's own image prepare)
# Steps: host packages + Windows 11 Enterprise Evaluation ISO + WAA @eb220c4 + winarena image; the pywin32 pin
# (osvm/incumbents/waa-pywin32-fix.sh); WAA's own golden-image prepare; the Drums clean state from that golden disk;
# drumsd build; measured runs (latency N=8, both arms; ladders); analysis.
set -euo pipefail
cd ~/drumsd
sudo bash osvm/hosts/windows-host-setup.sh > ~/repro-setup.log 2>&1
sudo bash osvm/incumbents/waa-pywin32-fix.sh
# WAA golden image, exactly as its README: ISO as setup.iso, run-local.sh --prepare-image true (no model key is used)
W=/opt/t5/WindowsAgentArena
sudo mkdir -p $W/src/win-arena-container/vm/image $W/src/win-arena-container/vm/storage
sudo ln -f /opt/t5/win11-eval.iso $W/src/win-arena-container/vm/image/setup.iso
echo '{"OPENAI_API_KEY": "dummy-not-used-no-model-in-this-study"}' | sudo tee $W/config.json >/dev/null
sudo chmod +x $W/scripts/*.sh
( cd $W/scripts && sudo bash ./run-local.sh --prepare-image true --skip-build true > ~/repro-waa-prepare.log 2>&1 ) || true
grep -q "Shutdown completed" ~/repro-waa-prepare.log || { echo "WAA prepare did not finish: see ~/repro-waa-prepare.log"; exit 1; }
sudo docker rm -f winarena >/dev/null 2>&1 || true
sudo mkdir -p /srv/t5/waa-golden && sudo cp --sparse=always -p $W/src/win-arena-container/vm/storage/* /srv/t5/waa-golden/ && sudo chmod -R a-w /srv/t5/waa-golden
# extra volume for WAA per-desktop copies
D=$(lsblk -dno NAME,SIZE,MOUNTPOINT | awk '$3=="" && $1 ~ /^nvme/ {print $1}' | head -1)
sudo mkdir -p /srv/t5/waa; mountpoint -q /srv/t5/waa || { sudo mkfs.ext4 -q -E nodiscard /dev/$D; sudo mount -o noatime /dev/$D /srv/t5/waa; }
# drumsd + Drums clean state
command -v cargo >/dev/null || { curl -sSf https://sh.rustup.rs | sh -s -- -y -q; }
. ~/.cargo/env
cargo build --release -p drumsd 2>&1 | tail -1; sudo install -m 755 target/release/drumsd /usr/local/bin/drumsd
[ -f /etc/drumsd-os.token ] || { head -c 32 /dev/urandom | xxd -p -c 64 | sudo tee /etc/drumsd-os.token >/dev/null; sudo chmod 600 /etc/drumsd-os.token; }
sudo cp /etc/drumsd-os.token ~/.drumsd-token; sudo chown ubuntu ~/.drumsd-token
sudo cp osvm/config/windows-host.json /etc/drumsd-os.json
sudo mkdir -p /srv/t5/win /srv/t5/drums-win /var/lib/drumsd-os/journal
sudo bash osvm/hosts/windows-clean-state.sh > ~/repro-clean-state.log 2>&1
mkdir -p ~/t5runs
bash osvm/hosts/redeploy.sh windows-host.json windows
bash osvm/harness/run_measured.sh windows latency
[ -f ~/repro-ladders.env ] && . ~/repro-ladders.env
for arm in ${LADDER_ARMS:-w1 w0}; do
  v=LADDER_$arm; bash osvm/harness/run_measured.sh windows ladder $arm ${!v:-4 8 12 16 24 32 40 48}
done
python3 osvm/harness/analyze.py ~/t5runs/measured --md ~/t5runs/measured/TABLES.md --json ~/t5runs/measured/summary.json
echo REPRODUCE_WINDOWS_DONE
