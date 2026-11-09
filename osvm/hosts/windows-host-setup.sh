#!/bin/bash
# Windows host provisioning for the T5 OS-coverage study (Ubuntu 24.04, x86_64, /dev/kvm).
# Installs QEMU/KVM + OVMF + swtpm (Drums windows backend), docker + WindowsAgentArena (incumbent arm) at a pinned
# commit and image digest, and downloads the Windows 11 Enterprise Evaluation ISO (research use only; see LICENSING).
set -euxo pipefail
WIN_ISO_URL=${WIN_ISO_URL:-https://software-static.download.prss.microsoft.com/dbazure/888969d5-f34g-4e03-ac9d-1f9786c66749/26100.1742.240906-0331.ge_release_svc_refresh_CLIENTENTERPRISEEVAL_OEMRET_x64FRE_en-us.iso}
WAA_COMMIT=${WAA_COMMIT:-eb220c4dcf64babefc1bfc287edda91ab04132ed}
WAA_IMAGE=${WAA_IMAGE:-windowsarena/winarena@sha256:869afffe384f0e0356dbfcdf78486611e31d62e692544cbfd9a6c08606f07440}
mkdir -p /opt/t5 && cd /opt/t5
export DEBIAN_FRONTEND=noninteractive
apt-get update -q
apt-get install -y -q qemu-system-x86 qemu-utils ovmf swtpm swtpm-tools docker.io git jq curl python3-venv python3-pip \
  genisoimage p7zip-full wimtools xorriso socat imagemagick
usermod -aG docker,kvm ubuntu || true
[ -s win11-eval.iso ] || curl -sSL -o win11-eval.iso "$WIN_ISO_URL"
sha256sum win11-eval.iso | tee win11-eval.iso.sha256
[ -d WindowsAgentArena ] || git clone -q https://github.com/microsoft/WindowsAgentArena.git
(cd WindowsAgentArena && git checkout -q "$WAA_COMMIT" && git rev-parse HEAD > ../waa_commit.txt)
docker pull "$WAA_IMAGE"
WAA_DIGEST=$(docker inspect --format '{{index .RepoDigests 0}}' "$WAA_IMAGE")
jq -n --arg iso "$WIN_ISO_URL" --arg isosha "$(cut -d' ' -f1 win11-eval.iso.sha256)" --arg waa "$(cat waa_commit.txt)" \
  --arg waaimg "$WAA_DIGEST" --arg qemu "$(qemu-system-x86_64 --version | head -1)" --arg kernel "$(uname -r)" \
  --arg ovmf "$(dpkg-query -W -f='${Version}' ovmf)" \
  '{win_iso_url:$iso, win_iso_sha256:$isosha, waa_commit:$waa, waa_image:$waaimg, qemu:$qemu, kernel:$kernel, ovmf:$ovmf}' > versions.json
cat versions.json
echo SETUP_DONE
