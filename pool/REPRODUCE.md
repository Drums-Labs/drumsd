# Reproduce the T3 scale test (2,000+ desktops, >= 8 spot hosts, 2 regions, one host killed)

Two commands after a one-time image build. Expected wall time about 2 h 15 min per run; expected cost about 30-45 USD per
run at November 2026 spot prices (9 x 64-vCPU spot hosts + one on-demand c8i.8xlarge coordinator).

## 0. What you need
- An AWS account with spot quota "All Standard Spot Instance Requests" >= 320 vCPU in us-east-2 and >= 256 in us-west-2,
  on-demand >= 32 vCPU in us-east-2; boto3 credentials allowed to run/describe/terminate instances, create launch
  templates, fleets, security groups, images (ec2:*Image*, ec2:CopyImage).
- This repository at the commit in raw/MANIFEST.json (`drums_pool_commit`), and the Drums release tarball built from the
  pinned commits: `DRUMS_CRIU=<drums-criu at b4fbfddf1> DRUMS_LAB=<drums-lab at cb3145d> install/make-release.sh OUT` run
  from drumsd at bd4348387400 (the tarball's own sha256 varies between builds; its inner SHA256SUMS does not).
- An ssh key pair imported as `drums-sota-t3` in both regions, and the coordinator key pair `t3-coord` (its public half
  is baked into the image at /opt/drums-t3/coordinator.pub and ~ubuntu/.ssh/authorized_keys).

## 1. Host image (once)
On a fresh Ubuntu 24.04 (ami-0fc53b217ec1ad816 in us-east-2, kernel 7.0.0-1014-aws) instance, any size >= 16 vCPU:
```
tar xzf drums-release-bd4348387400.tar.gz && cd drums-release-bd4348387400 && sudo ./install.sh --remote     # 7-15 min
sudo install -d /opt/drums-t3 && sudo install -m 0755 <repo>/pool/infra/{guard.sh,host-firstboot.sh,ami-prep.sh} /opt/drums-t3/
cat t3-coord.pub | sudo tee /opt/drums-t3/coordinator.pub >> ~/.ssh/authorized_keys
sudo bash /opt/drums-t3/ami-prep.sh
```
Then CreateImage **with NoReboot=true** (a reboot is a shutdown; with shutdown behavior "terminate" the instance is lost).
Put the AMI id in your plan file (`pool/infra/plan-r1.json` is ours).

## 2. Launch the fleet (laptop)
```
python3 pool/infra/fleet.py pool/infra/plan-r1.json up        # copies the AMI to us-west-2, launches coordinator + 5 + 4 hosts
python3 pool/infra/fleet.py pool/infra/plan-r1.json status > hosts.json
```
Allow ssh from the coordinator's public IP to the us-west-2 security group (`drums-sota-t3-<run>`). Copy the repo,
the release tarball (as ~/drums-release.tar.gz), hosts.json and the coordinator private key (~/.ssh/t3-coord) to the coordinator.

## 3. Run (coordinator) -- the one command
```
sudo bash pool/run/coordinator-setup.sh ~/drums-release.tar.gz && \
/opt/drums-pool/venv/bin/python pool/run/run_scale.py --hosts hosts.json --out raw/run --workers 2100 --procs 12 --warmup-s 900 --measure-s 3600 \
    --kill-at-s 1800 --kill-region us-east-2 --warm-per-host 4 --max-envs 280 --seed 20261108
python3 pool/analysis/analyze.py raw/run            # tables.md, summary.json, fig-*.svg from raw/run only
```
Grafana (SLO dashboard "drums-pool SLOs") is on the coordinator at 127.0.0.1:3000: `ssh -L 3000:127.0.0.1:3000 coordinator`.

## 4. Expected numbers and tolerance
From PREREGISTRATION.md (rules 1-5) and the measured runs (REPORT.md):
- live desktops >= 2,000 for the whole window outside the recovery interval; valid-reward rate >= 0.999 with 0 IRV;
- host-dead detection <= 20 s after the power-off; no 60 s bin with 0 valid rewards; recovery R <= 300 s and within
  max(60 s, 50 %) of the report's R; pool step overhead p99 within 2x of the report's.

## 5. Tear down
```
python3 pool/infra/fleet.py pool/infra/plan-r1.json down && python3 pool/infra/fleet.py pool/infra/plan-r1.json status   # expect []
```
Every instance also powers itself off (and terminates) after 30 idle minutes and at a 300-minute hard ceiling.
