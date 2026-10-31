#!/bin/sh
# Rebuild sysinfo.bpf.o (committed; the release does not need clang). Needs clang, libbpf-dev and bpftool
# (linux-tools-$(uname -r)). The object carries BTF; aya applies its CO-RE relocations against the running
# kernel's /sys/kernel/btf/vmlinux at load, so it is not tied to the build host's kernel.
set -eu
cd "$(dirname "$0")"
BPFTOOL=${BPFTOOL:-$(ls /usr/lib/linux-tools/*/bpftool 2>/dev/null | head -1)}
"$BPFTOOL" btf dump file /sys/kernel/btf/vmlinux format c > vmlinux.h
clang -O2 -g -target bpf -D__TARGET_ARCH_x86 -I. -c sysinfo.bpf.c -o sysinfo.bpf.o
llvm-strip -g sysinfo.bpf.o 2>/dev/null || true
rm -f vmlinux.h
sha256sum sysinfo.bpf.c sysinfo.bpf.o
