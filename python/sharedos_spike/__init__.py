"""Shared-OS runtime SPIKE built only from public mechanisms (reduced-scope diagnostic).

Mechanism: one host kernel; each "slot" is a private pid/mount/net/ipc/uts namespace tree
(util-linux unshare) running Xvfb + xterm + a small stateful probe app; live save/restore is
CRIU (criu dump / criu restore). It is NOT the OSWorld GNOME image and makes no claim about
OSWorld fidelity, cost, or Drums. A gate that fails is a recorded result, not a bug to hide.
Clean-room: written from the public documents below; nothing from any excluded work.
"""

PUBLIC_SOURCES = [
    "https://criu.org/Main_Page",
    "https://man.archlinux.org/man/criu.8.en",
    "https://www.criu.org/What_cannot_be_checkpointed",
    "https://github.com/mikaelhg/criu-x11-poc",
    "https://man7.org/linux/man-pages/man1/unshare.1.html",
    "https://man7.org/linux/man-pages/man1/nsenter.1.html",
    "https://man7.org/linux/man-pages/man7/pid_namespaces.7.html",
    "https://gvisor.dev/docs/user_guide/checkpoint_restore/",
]

FIDELITY_DIFFERENCES = [
    "Not the OSWorld Ubuntu image: no guest kernel, systemd, GNOME session, Nautilus or OSWorld guest server; "
    "the workload is Xvfb + xterm + a probe app, so no OSWorld task semantics are exercised.",
    "Shares the host kernel (a VM baseline has its own); kernel-level state is not per attempt.",
    "Xvfb software framebuffer, no GPU/DRM/compositor; display state lives in the Xvfb process memory.",
    "Namespaces isolate pid/mount/net/ipc/uts only: no user namespace, no cgroup CPU/memory limits, so "
    "equal-guest-resources with the VM baseline is NOT satisfied.",
    "Only /tmp is private (tmpfs); the rest of the filesystem is the host's, with no per-attempt copy-on-write disk.",
    "CRIU restore needs privileges and the same kernel and CRIU build as the dump host.",
    "The GUI-state gate is a framebuffer hash, a coarse proxy for application state.",
]
