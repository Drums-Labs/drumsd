#!/bin/bash
# WindowsAgentArena @eb220c4 + windowsarena/winarena@sha256:869afffe...: its setup.ps1 installs pywinauto, which pulls
# the latest pywin32; with that pywin32 the in-guest server dies at import ("DLL load failed while importing win32ui"),
# so --prepare-image never finishes. Measured 2026-11-08. This derived image pins pywin32==306 (current when WAA was
# released) right after pywinauto, in WAA's own setup.ps1, and is tagged as the name WAA's scripts run
# (windowsarena/winarena:latest). Nothing else changes. Used for both the WAA arm and the Drums golden disk.
set -euo pipefail
BASE=${BASE:-windowsarena/winarena@sha256:869afffe384f0e0356dbfcdf78486611e31d62e692544cbfd9a6c08606f07440}
d=$(mktemp -d)
cat > $d/Dockerfile <<DF
FROM $BASE
RUN sed -i 's/^Install-PythonPackages -Package "pywinauto"\r\?$/Install-PythonPackages -Package "pywinauto"\nInstall-PythonPackages -Package "pywin32==306"/' /oem/setup.ps1 && grep -n 'pywin32==306' /oem/setup.ps1
DF
docker build -q -t windowsarena/winarena:latest -t windowsarena/winarena:t5-pywin32-306 $d
docker inspect --format '{{.Id}}' windowsarena/winarena:t5-pywin32-306
