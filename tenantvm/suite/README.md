# Two-tenant escape and side-channel suite

Each probe asks one question: can tenant A, acting from inside its desktop env (an untrusted agent), observe or affect
tenant B, or reach the host/metadata? Every probe returns PASS (isolated) or FAIL (leak) with the evidence it read.

The same probes run in two modes so the VM boundary's effect is a measured difference, not an assertion:
- `bare`  : A and B are two Drums envs on one bare host (the incumbent-equivalent shared-kernel boundary).
- `vm`    : A and B are two Drums envs, each inside its own tenant VM on one metal host.

Dimensions: filesystem, network, process table, /proc, cgroup, device, metadata, and timing/covert-channel basics.

Driver: `run_suite.py MODE A_BASE_URL A_TOKEN B_BASE_URL B_TOKEN OUT.json`. In `bare` the two URLs are the same drumsd; in
`vm` they are the two guests' drumsd (reached over the per-tenant tap). The probes act only through the Drums SDK (env.step
with execute/pyautogui), i.e. exactly what an agent can do; nothing runs as host root inside a tenant.
