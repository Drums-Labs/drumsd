#!/usr/bin/env python3
"""Write raw/MANIFEST.json pins for one T5 host (charter: commits, image digests, AMI, kernel, instance type, region,
task split sha256, seeds). Usage: manifest.py OS OUT.json  (env COMMIT = drumsd git commit the host was built from)."""
import glob, hashlib, json, os, subprocess, sys, urllib.request

def sh(c):
    try:
        return subprocess.run(c, shell=True, capture_output=True, text=True, timeout=600).stdout.strip()
    except Exception as e:
        return f"error: {e}"

def imds(path):
    try:
        t = urllib.request.urlopen(urllib.request.Request("http://169.254.169.254/latest/api/token", method="PUT", headers={"X-aws-ec2-metadata-token-ttl-seconds": "60"}), timeout=2).read().decode()
        return urllib.request.urlopen(urllib.request.Request("http://169.254.169.254/latest/meta-data/" + path, headers={"X-aws-ec2-metadata-token": t}), timeout=2).read().decode()
    except Exception:
        return None

def fsha(p):
    if not os.path.exists(p):
        return None
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 22), b""):
            h.update(b)
    return h.hexdigest()

def tasks_sha(d):
    h = hashlib.sha256()
    for f in sorted(glob.glob(os.path.join(d, "*.json"))):
        h.update(os.path.basename(f).encode()); h.update(open(f, "rb").read())
    return h.hexdigest(), len(glob.glob(os.path.join(d, "*.json")))

osname, out = sys.argv[1], sys.argv[2]
root = os.path.expanduser("~/drumsd")
m = {"os": osname, "drumsd_commit": os.environ.get("COMMIT"), "instance_type": imds("instance-type"), "instance_id": imds("instance-id"),
     "ami": imds("ami-id"), "az": imds("placement/availability-zone"), "kernel": sh("uname -r"), "os_release": sh(". /etc/os-release; echo $PRETTY_NAME"),
     "cpu": sh("lscpu | grep 'Model name' | sed 's/.*: *//'"), "vcpus": os.cpu_count(), "mem_kib": sh("grep MemTotal /proc/meminfo | awk '{print $2}'"),
     "drumsd_binary_sha256": fsha("/usr/local/bin/drumsd"), "docker": sh("docker --version"), "seeds": {"task_order": "deterministic (i+k) mod T", "bootstrap": 0},
     "config_sha256": fsha("/etc/drumsd-os.json"), "host_versions": json.load(open("/opt/t5/versions.json")) if os.path.exists("/opt/t5/versions.json") else None}
m["tasks_sha256"], m["tasks_n"] = tasks_sha(os.path.join(root, "osvm/tasks", osname))
if osname == "android":
    m["apks"] = sh("cat /opt/t5/apks/SHA256SUMS")
    m["android_world_image_id"] = sh("docker inspect --format '{{.Id}}' android_world:55ddaf5")
    m["android_world_pillow"] = sh("docker run --rm --entrypoint python3 android_world:55ddaf5 -c 'import PIL; print(PIL.__version__)'")
    m["redroid_golden_data_du"] = sh("du -sb /srv/t5/redroid-golden/data | cut -f1")
    m["avd_snapshot_ram_sha256"] = fsha("/opt/t5/avd/AndroidWorldAvd.avd/snapshots/clean/ram.bin")
    m["emulator_version"] = sh("/opt/t5/android-sdk/emulator/emulator -version | head -1")
else:
    m["winarena_images"] = sh("docker images --digests windowsarena/winarena --format '{{.Tag}} {{.Digest}} {{.ID}}'")
    for f in ["clean.vars", "windows.rom", "clean.qcow2", "clean.state", "golden.raw"]:
        m["win_" + f.replace(".", "_") + "_sha256"] = fsha("/srv/t5/win/" + f)
    m["qemu_host"] = sh("qemu-system-x86_64 --version | head -1")
json.dump(m, open(out, "w"), indent=1)
print(json.dumps(m, indent=1)[:2000])
