# Reproduce the T3 scale test (2,000+ desktops, >= 8 spot hosts, 2 regions, one host killed)

Two commands after a one-time image build. Expected wall time about 2 h per run; expected cost about 35-45 USD per run at
November 2026 spot prices (10 x 64-vCPU spot hosts, or a mix with 32-vCPU hosts, + one on-demand c8i.8xlarge coordinator).
Spot capacity is the main risk: on 2026-11-09 us-east-2 and us-east-1 reclaimed 14 hosts in 3 h. Size the fleet with headroom.

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
On a fresh Ubuntu 24.04 (ami-0fc53b217ec1ad816 in us-east-2, kernel 7.0.0-1014-aws) instance, any size >= 16 vCPU
(on-demand: three spot builders were lost to the guard ceiling, a reboot during CreateImage and a spot reclaim):
```
bash pool/infra/build-image.sh drums-release-bd4348387400.tar.gz t3-coord.pub     # install (~8 min) + T3 scripts + ami-prep
```
Then CreateImage **with NoReboot=true** (a reboot is a shutdown; with shutdown behavior "terminate" the instance is lost).
Put the AMI id in your plan file (`pool/infra/plan-m1.json` / `plan-m2.json` are ours: ami-04da8ed660348fe4f us-east-2,
copies ami-079ffb7ca4772534d us-west-2 and ami-05d4c50387f38e3cf us-east-1).

## 2. Launch the fleet (laptop)
```
python3 pool/infra/fleet.py pool/infra/plan-m1.json up        # copies the AMI per region, launches coordinator + 5 + 5 hosts
python3 pool/infra/fleet.py pool/infra/plan-m1.json status > hosts.json
```
Allow ssh from the coordinator's public IP to the us-west-2 security group (`drums-sota-t3-<run>`). Copy the repo,
the release tarball (as ~/drums-release.tar.gz), hosts.json and the coordinator private key (~/.ssh/t3-coord) to the coordinator.

## 3. Run (coordinator) -- the one command
```
sudo bash pool/run/coordinator-setup.sh ~/drums-release.tar.gz && \
/opt/drums-pool/venv/bin/python pool/run/run_scale.py --hosts hosts.json --out raw/run --workers 2100 --procs 12 --warmup-s 900 --measure-s 3600 \
    --kill-at-s 1800 --kill-region us-east-2 --warm-per-host 4 --max-envs-per-vcpu 4.375 --seed 20261108
python3 pool/analysis/analyze.py raw/run            # tables.md, summary.json, fig-*.svg from raw/run only
```
run_scale accepts each host (one real task desktop, valid gates, no-op reward 0.0) before it joins; a host that fails is
left out and named in phases.jsonl. To replace capacity during a run: launch hosts from the same launch template, then
`bash hotadd.sh NAME PRIVATE_IP VCPUS` on the coordinator (acceptance, then POST /pool/v1/hosts).
Grafana (SLO dashboard "drums-pool SLOs") is on the coordinator at 127.0.0.1:3000: `ssh -L 3000:127.0.0.1:3000 coordinator`.

## 4. Expected numbers and tolerance
From PREREGISTRATION.md (rules 1-5). Measured (report): m1 median 2,091 live, 97.77 % valid (0 IRV), detection 11.1 s,
R 110 s, pool step overhead p99 <= 2.5 ms; see the report for m2. The pre-registered bars:
- live desktops >= 2,000 for the whole window outside the recovery interval; valid-reward rate >= 0.999 with 0 IRV;
- host-dead detection <= 20 s after the power-off; no 60 s bin with 0 valid rewards; recovery R <= 300 s and within
  max(60 s, 50 %) of the report's R; pool step overhead p99 within 2x of the report's.

## 5. Tear down
```
python3 pool/infra/fleet.py pool/infra/plan-m1.json down && python3 pool/infra/fleet.py pool/infra/plan-m1.json status   # expect []
```
Every instance also powers itself off (and terminates) after 30 idle minutes and at a 300-minute hard ceiling.
