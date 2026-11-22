# python/ - the runtime's Python side

These packages are the Python side of the drumsd runtime, imported from drums-lab at commit
`f17644b41de7a85f5a00baf4ee332b41f356de60` (branch `sota/t2b`, the commit `install/lock.json` pinned until v0.2.1). drumsd
invokes `sharedos_rootfs.{rootfs_inspect,image,proxy,capture,candidate,tree}` and the template server (`tmplsrv`) with
`PYTHONPATH` set to this directory (`py_src` in drumsd.json). `sharedos_spike`, `drumsbench` and `osworld_smoke` are the
smallest import closure `sharedos_rootfs` needs; the package names are kept exactly as they were in drums-lab so that the
imports (and `python3 -m sharedos_rootfs.<module>`) stay compatible. The earlier drums-lab history is kept in a private archive and
is not part of this repository. Tests: `PYTHONPATH=python python3 -m pytest python/tests`
(stdlib + pytest; written for Linux, a few tests need mount/cgroup privileges and skip or fail without them).
