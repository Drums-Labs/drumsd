// SPDX-License-Identifier: GPL-2.0
// drumsd procview: sysinfo(2) for tasks of a Drums slot reports the slot's memory, swap and task count.
//
// glibc's sysconf(_SC_PHYS_PAGES) / sysconf(_SC_AVPHYS_PAGES) are sysinfo(2), so without this an app in a slot sizes
// itself for the host (GIMP: tile cache = half of "physical memory"; Chrome: SysInfo::AmountOfPhysicalMemory).
// An fexit program on the syscall: after the kernel has copied its struct sysinfo to the caller, the memory fields
// are overwritten for a caller whose cgroup chain contains a slot cgroup (name drumsbench-*.scope other than the
// pool). Values come from the memory cgroup that sets the effective limit (the minimum memory.max on the chain).
// Every other caller is left untouched. Keyed on the cgroup at call time: nothing per process, so fork, CRIU
// restore and nested namespaces need nothing.
//
// Build: bpftool btf dump file /sys/kernel/btf/vmlinux format c > vmlinux.h
//        clang -O2 -g -target bpf -D__TARGET_ARCH_x86 -c sysinfo.bpf.c -o sysinfo.bpf.o   (see build.sh)
#include "vmlinux.h"
#include <bpf/bpf_helpers.h>
#include <bpf/bpf_tracing.h>
#include <bpf/bpf_core_read.h>

char LICENSE[] SEC("license") = "GPL";

#define PAGE 4096ULL
#define MAX_DEPTH 10

struct cfg {
	__u64 host_mem_total;  // bytes
	__u64 host_swap_total; // bytes
};

struct {
	__uint(type, BPF_MAP_TYPE_ARRAY);
	__uint(max_entries, 1);
	__type(key, __u32);
	__type(value, struct cfg);
} CFG SEC(".maps");

// [0] rewritten, [1] not a slot, [2] read error, [3] write error
struct {
	__uint(type, BPF_MAP_TYPE_PERCPU_ARRAY);
	__uint(max_entries, 4);
	__type(key, __u32);
	__type(value, __u64);
} STATS SEC(".maps");

// struct sysinfo from totalram (offset 32) to mem_unit (offset 104..108), x86_64 layout
struct si_tail {
	__u64 totalram, freeram, sharedram, bufferram, totalswap, freeswap;
	__u16 procs;
	__u16 pad1;
	__u32 pad2;
	__u64 totalhigh, freehigh;
	__u32 mem_unit;
} __attribute__((packed));

static __always_inline void count(__u32 k)
{
	__u64 *v = bpf_map_lookup_elem(&STATS, &k);
	if (v)
		*v += 1;
}

static __always_inline int slot_name(struct cgroup *cg)
{
	char n[32] = {};
	const char *p = BPF_CORE_READ(cg, kn, name);
	if (!p || bpf_probe_read_kernel_str(n, sizeof(n), p) < 0)
		return 0;
	const char want[] = "drumsbench-";
#pragma unroll
	for (int i = 0; i < 11; i++)
		if (n[i] != want[i])
			return 0;
	// the pool itself ("drumsbench-pool.scope") is not a slot
	const char pool[] = "pool.scope";
	int is_pool = 1;
#pragma unroll
	for (int i = 0; i < 11; i++)
		if (n[11 + i] != pool[i])
			is_pool = 0;
	return !is_pool;
}

SEC("fexit/__x64_sys_sysinfo")
int BPF_PROG(on_sysinfo, const struct pt_regs *regs, long ret)
{
	if (ret != 0)
		return 0;
	struct sysinfo *u = (struct sysinfo *)BPF_CORE_READ(regs, di);
	struct task_struct *t = bpf_get_current_task_btf();
	int mid = bpf_core_enum_value(enum cgroup_subsys_id, memory_cgrp_id);
	int pid_id = bpf_core_enum_value(enum cgroup_subsys_id, pids_cgrp_id);
	struct css_set *cs = BPF_CORE_READ(t, cgroups);
	struct cgroup_subsys_state *css = BPF_CORE_READ(cs, subsys[mid & 15]);
	__u64 off = bpf_core_field_offset(struct mem_cgroup, css);
	int slot = 0;
	__u64 lim = ~0ULL, use = 0, swl = ~0ULL, swu = 0;
	struct cgroup *at = 0;

#pragma unroll
	for (int i = 0; i < MAX_DEPTH; i++) {
		if (!css)
			break;
		struct mem_cgroup *m = (struct mem_cgroup *)((char *)css - off);
		struct cgroup *cg = BPF_CORE_READ(css, cgroup);
		__u64 max = BPF_CORE_READ(m, memory.max);
		if (max < lim) {
			lim = max;
			use = BPF_CORE_READ(m, memory.usage.counter);
			swu = BPF_CORE_READ(m, swap.usage.counter);
			at = cg;
		}
		__u64 smax = BPF_CORE_READ(m, swap.max);
		if (smax < swl)
			swl = smax;
		if (cg && slot_name(cg))
			slot = 1;
		css = BPF_CORE_READ(css, parent);
	}
	if (!slot) {
		count(1);
		return 0;
	}
	__u32 z = 0;
	struct cfg *c = bpf_map_lookup_elem(&CFG, &z);
	if (!c || !at || lim == ~0ULL) {
		count(2);
		return 0;
	}
	struct si_tail s = {};
	__u64 total = lim * PAGE;
	if (c->host_mem_total && total > c->host_mem_total)
		total = c->host_mem_total;
	__u64 used = use * PAGE;
	s.totalram = total;
	s.freeram = used < total ? total - used : 0;
	__u64 st = swl == ~0ULL ? 0 : swl * PAGE;
	if (st > c->host_swap_total)
		st = c->host_swap_total;
	__u64 su = swu * PAGE;
	s.totalswap = st;
	s.freeswap = su < st ? st - su : 0;
	s.procs = 1;
	struct cgroup_subsys_state *pcss = BPF_CORE_READ(at, subsys[pid_id & 15]);
	if (pcss) {
		struct pids_cgroup *pc = (struct pids_cgroup *)((char *)pcss - bpf_core_field_offset(struct pids_cgroup, css));
		__s64 n = BPF_CORE_READ(pc, counter.counter);
		s.procs = n > 65535 ? 65535 : (n < 1 ? 1 : n);
	}
	s.mem_unit = 1;
	if (bpf_probe_write_user((char *)u + 32, &s, sizeof(s)))
		count(3);
	else
		count(0);
	return 0;
}
