"""Read-only attach / mount / detach of the OSWorld qcow2 with leak detection.

  python -m sharedos_rootfs.image attach --image IMG --expected-sha256 H --mountpoint MNT --state STATE.json
  python -m sharedos_rootfs.image detach --state STATE.json

Nothing is written to the image: qemu-nbd --read-only, mount -o ro,noload,nodev,nosuid. If the filesystem cannot
be unmounted the NBD device is NOT disconnected and the failure is reported with evidence (never swallowed).
"""

from __future__ import annotations

import hashlib
import json
import os
import sys
import time

from drumsbench.contract import Command

NBD_DEFAULT = "/dev/nbd0"
ROOT_FSTYPES = ("ext4", "ext3", "ext2", "xfs", "btrfs")


class ImageError(Exception):
    pass


class CleanupError(ImageError):
    def __init__(self, msg, evidence):
        super().__init__(msg)
        self.evidence = evidence


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 22), b""):
            h.update(chunk)
    return h.hexdigest()


def verify_image(image: str, expected: str, cache_path: str, hasher=sha256_file) -> dict:
    """The image's sha256 must equal the pin. A cache keyed on (size, mtime) avoids rehashing ~24 GB each time."""
    if not (isinstance(expected, str) and len(expected) == 64 and all(c in "0123456789abcdef" for c in expected)):
        raise ImageError("expected sha256 must be 64 lowercase hex characters")
    st = os.stat(image)
    key = {"path": os.path.abspath(image), "size": st.st_size, "mtime_ns": st.st_mtime_ns}
    if os.path.exists(cache_path):
        try:
            c = json.load(open(cache_path))
            if all(c.get(k) == v for k, v in key.items()) and c.get("sha256") == expected:
                return {**key, "sha256": expected, "from_cache": True}
        except (OSError, ValueError):
            pass
    got = hasher(image)
    if got != expected:
        raise ImageError(f"image sha256 {got} != pinned {expected}")
    with open(cache_path, "w") as f:
        json.dump({**key, "sha256": got}, f)
    return {**key, "sha256": got, "from_cache": False}


def pick_root_partition(lsblk: dict) -> str:
    """Largest partition with a root-capable filesystem; refuses an empty or ambiguous (two near-equal) choice."""
    parts = []

    def walk(nodes):
        for n in nodes or []:
            if n.get("type") == "part" and (n.get("fstype") or "") in ROOT_FSTYPES:
                parts.append((int(n.get("size") or 0), n["name"]))
            walk(n.get("children"))
    walk(lsblk.get("blockdevices"))
    if not parts:
        raise ImageError(f"no partition with a root filesystem ({ROOT_FSTYPES}) found: {lsblk}")
    parts.sort(reverse=True)
    if len(parts) > 1 and parts[1][0] * 10 >= parts[0][0] * 9:
        raise ImageError(f"ambiguous root partition: {parts[:2]} are within 10% of each other")
    return parts[0][1]


class ImageMount:
    def __init__(self, image: str, mountpoint: str, executor, nbd: str = NBD_DEFAULT, sleep=time.sleep,
                 settle_attempts: int = 20):
        self.image, self.mountpoint, self.ex, self.nbd, self.sleep = image, mountpoint, executor, nbd, sleep
        self.settle_attempts = settle_attempts
        self.attached = self.mounted = False
        self.partition = None
        self.log: list = []

    def _run(self, argv, purpose, ok=(0,), timeout=300.0):
        res = self.ex.run(Command("image", tuple(argv), purpose, timeout_s=timeout), timeout)
        self.log.append({"purpose": purpose, "argv": list(argv), "rc": res.rc, "error": res.error,
                         "stdout": res.stdout[-800:], "stderr": res.stderr[-800:]})
        if res.rc not in ok or res.error:
            raise ImageError(f"{purpose} failed rc={res.rc} error={res.error} stderr={res.stderr[-300:]!r}")
        return res

    def _busy(self) -> bool:
        return self.ex.run(Command("image", ("test", "-e", f"/sys/block/{os.path.basename(self.nbd)}/pid"), "nbd busy?"), 30).rc == 0

    def attach(self) -> dict:
        self._run(("modprobe", "nbd", "max_part=16"), "load nbd")
        if self._busy():
            raise ImageError(f"{self.nbd} is already connected: refusing to touch it")
        self._run(("qemu-nbd", "--read-only", "--format=qcow2", "--cache=none", f"--connect={self.nbd}", self.image),
                  "attach image read-only")
        self.attached = True
        self._run(("blockdev", "--rereadpt", self.nbd), "re-read partition table", ok=(0, 1))
        listing, err = None, None
        for _ in range(self.settle_attempts):
            res = self._run(("lsblk", "-J", "-b", "-o", "NAME,SIZE,FSTYPE,TYPE", self.nbd), "list partitions")
            try:
                listing = json.loads(res.stdout)
                self.partition = pick_root_partition(listing)
                break
            except (ValueError, ImageError) as e:
                err = e
                self.sleep(0.5)
        if self.partition is None:
            raise ImageError(f"no usable root partition appeared on {self.nbd}: {err}")
        dev = f"/dev/{self.partition}"
        self._run(("mkdir", "-p", self.mountpoint), "mountpoint")
        self._run(("mount", "-o", "ro,noload,nodev,nosuid", dev, self.mountpoint), "mount read-only")
        self.mounted = True
        opts = self._run(("findmnt", "-n", "-o", "OPTIONS", self.mountpoint), "read back mount options").stdout
        if "ro" not in opts.strip().split(",") and not opts.strip().startswith("ro"):
            raise ImageError(f"mount is not read-only: {opts.strip()!r}")
        return {"image": self.image, "nbd": self.nbd, "partition": dev, "mountpoint": self.mountpoint,
                "mount_options": opts.strip()}

    def detach(self) -> list:
        """Unmount, then disconnect. If the unmount fails the device is left connected and CleanupError is raised."""
        evidence = []
        if self.mounted:
            r = self.ex.run(Command("image", ("umount", self.mountpoint), "unmount"), 120)
            self.log.append({"purpose": "unmount", "rc": r.rc, "stderr": r.stderr[-300:]})
            still = self.ex.run(Command("image", ("findmnt", "-n", self.mountpoint), "still mounted?"), 30).rc == 0
            if r.rc != 0 or still:
                evidence.append(f"unmount rc={r.rc} still_mounted={still}: {r.stderr[-200:]!r}")
                raise CleanupError(f"cannot unmount {self.mountpoint}; {self.nbd} left connected", evidence)
            self.mounted = False
        if self.attached:
            r = self.ex.run(Command("image", ("qemu-nbd", "--disconnect", self.nbd), "disconnect"), 120)
            self.log.append({"purpose": "disconnect", "rc": r.rc, "stderr": r.stderr[-300:]})
            for _ in range(20):
                if not self._busy():
                    break
                self.sleep(0.5)
            if r.rc != 0 or self._busy():
                evidence.append(f"disconnect rc={r.rc} device still busy={self._busy()}")
                raise CleanupError(f"{self.nbd} is still connected after disconnect", evidence)
            self.attached = False
        return evidence

    def __enter__(self):
        try:
            self.attach()
        except BaseException:
            try:
                self.detach()   # a failed attach must not leave a connected device behind
            except CleanupError:
                pass
            raise
        return self

    def __exit__(self, *exc):
        self.detach()


def main(argv: list) -> int:
    import argparse
    from drumsbench.executor import SubprocessExecutor
    ap = argparse.ArgumentParser(prog="sharedos_rootfs.image")
    sub = ap.add_subparsers(dest="cmd", required=True)
    a = sub.add_parser("attach")
    a.add_argument("--image", required=True)
    a.add_argument("--expected-sha256", required=True)
    a.add_argument("--mountpoint", required=True)
    a.add_argument("--state", required=True)
    a.add_argument("--nbd", default=NBD_DEFAULT)
    d = sub.add_parser("detach")
    d.add_argument("--state", required=True)
    args = ap.parse_args(argv)
    ex = SubprocessExecutor()
    try:
        if args.cmd == "attach":
            v = verify_image(args.image, args.expected_sha256, args.state + ".sha-cache")
            m = ImageMount(args.image, args.mountpoint, ex, args.nbd)
            try:
                info = m.attach()
            except BaseException:
                try:
                    m.detach()
                except CleanupError as e:
                    print(f"CLEANUP FAILED: {e} {e.evidence}", file=sys.stderr)
                raise
            with open(args.state, "x") as f:
                json.dump({**info, "image_sha256": v["sha256"], "sha_from_cache": v["from_cache"]}, f, indent=2)
            print(json.dumps(info))
            return 0
        st = json.load(open(args.state))
        m = ImageMount(st["image"], st["mountpoint"], ex, st["nbd"])
        m.attached = m.mounted = True
        m.detach()
        os.remove(args.state)
        return 0
    except (ImageError, OSError, ValueError) as e:
        print(f"image error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
