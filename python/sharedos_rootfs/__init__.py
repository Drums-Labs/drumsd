"""Shared-OS arm B: the SAME OSWorld image's own userland on the host kernel, one overlay slot per attempt.

Mechanism (public Linux only): the pinned Ubuntu.qcow2 is attached read-only with qemu-nbd and its root
filesystem mounted read-only; each slot is an overlayfs on that lower layer inside private pid/mount/net/ipc/uts
namespaces, under cgroup v2 CPU/memory limits equal to the baseline guest; the image's own dbus / X display /
desktop and its own guest server run inside; a netns-aware proxy exposes the slot's server port on the host so
the unchanged OSWorld smoke can run; CRIU does the live checkpoint/restore.

NOTHING HERE HAS BEEN RUN AGAINST THE REAL IMAGE. No comparison claim is allowed until the sameness gate
(sameness.py) has no blocking mismatch AND the real gates have passed on a real host. The reduced Xvfb+xterm
diagnostic (sharedos_spike) is a different, smaller thing and stays a diagnostic.
"""

PUBLIC_SOURCES = [
    "https://man7.org/linux/man-pages/man8/qemu-nbd.8.html",
    "https://www.kernel.org/doc/html/latest/filesystems/overlayfs.html",
    "https://man7.org/linux/man-pages/man7/mount_namespaces.7.html",
    "https://man7.org/linux/man-pages/man7/cgroups.7.html",
    "https://www.freedesktop.org/software/systemd/man/latest/systemd-run.html",
    "https://criu.org/Main_Page",
    "https://github.com/xlang-ai/OSWorld/blob/04718eab9327856efe8192cc58161acc7a7627e2/desktop_env/server/main.py",
]

# sha256 of desktop_env/server/main.py at the pinned OSWorld commit 04718eab... (read from the public source).
PINNED_SERVER_MAIN_SHA256 = "efd969e8b3b272b3694cb66450ddb7c90818970c98bd153f5a2050363cdb88c7"

FIDELITY_DIFFERENCES = [
    "The host kernel runs the image's userland; the image's own kernel, modules, initramfs and bootloader are not used.",
    "No init system: the tree init starts only the services in the service plan, not the image's systemd boot sequence.",
    "The display is headless: the image's own Xorg with the Xorg dummy driver (added to the slot's upper layer from a "
    "pinned jammy deb, recorded in the slot's display-stage.json and the effective inspection), or an Xvfb/dummy the "
    "image already had. The baseline uses the QEMU virtual GPU with its real Xorg driver. The graphics path is NOT "
    "identical and is never claimed to be.",
    "CPU and memory are cgroup v2 limits (CPU quota, memory max), not dedicated vCPUs and a fixed RAM size; page "
    "cache is accounted differently and there is no ballooning.",
    "Disk is an overlayfs on the host filesystem over a read-only qcow2 mount, not a qcow2 copy-on-write overlay.",
    "Networking is a private namespace with a host proxy to the server port, not QEMU user-mode networking.",
    "The OSWorld guest server runs from the image's own copy (hash recorded and compared to the pinned public "
    "source); with Flask debug=True it also runs a reloader child.",
]
