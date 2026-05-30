# SysForge

Advanced system profiler, monitor, tracer and security auditor for HackerOS.

## Features

| Feature | Description |
|---|---|
| `cpu` | Real-time per-field CPU stats with configurable interval |
| `mem` | Memory breakdown with visual bar graph |
| `net` | Interface stats + active connections via `ss` |
| `disk` | I/O statistics from `/proc/diskstats` + `df` |
| `proc [pid]` | Top-15 by CPU or detailed PID profile |
| `trace [pid]` | eBPF syscall tracing via bpftrace/libbpf |
| `audit` | Security audit: sysctl params, SUID, listening ports |
| `container` | Docker/Podman/namespace/cgroup inspection |
| `dashboard` | Interactive TUI with live CPU/mem/proc bars |
| `export` | Metrics export: JSON or Prometheus format |
| `sysforge-daemon` | Background collection daemon with rotation |

## Installation

```sh
sudo hpm install sysforge
```

Dependencies installed automatically:
- `linux-perf` — for perf_event sampling
- `bpfcc-tools` — for eBPF/bpftrace (optional, trace command)

## Usage

```sh
# Basic monitoring
sysforge cpu              # CPU usage (1s interval, 5 samples)
sysforge cpu 0.5 20       # 20 samples at 0.5s
sysforge mem              # Memory breakdown
sysforge net              # Network interfaces + connections
sysforge disk             # Disk I/O + filesystem usage
sysforge proc             # Top 15 processes by CPU
sysforge proc 1234        # Profile specific PID

# Advanced (root required)
sudo sysforge trace 1234 10     # eBPF trace PID 1234 for 10s
sudo sysforge trace "" 5        # Trace all processes for 5s
sudo sysforge audit             # Full security audit

# Container/namespace inspection
sysforge container list         # List Docker/Podman containers
sysforge container namespaces   # All kernel namespaces
sysforge container cgroups      # cgroup v2 memory usage

# Dashboard
sysforge dashboard              # Interactive TUI (q=quit, r=refresh)

# Export
sysforge export json /tmp/metrics.json
sysforge export prometheus /tmp/metrics.txt

# Short alias
sfctl cpu
sfctl audit
```

## Daemon

```sh
sudo sysforge-daemon start      # Start background collection
sudo sysforge-daemon status     # Check status
sudo sysforge-daemon logs 100   # Last 100 log lines
sudo sysforge-daemon stop       # Stop
```

Config: `/etc/sysforge/daemon.conf`
Data:   `/var/lib/sysforge/metrics/`
Log:    `/var/log/sysforge/daemon.log`

## Bash completion

```sh
# After install, sysforge completion is automatic via hpm wrapper.
# To enable manually:
source /path/to/contents/completions/sysforge.bash
```

## Sandbox

SysForge runs with `dev=true` and access to `/proc`, `/sys`, `/run`, `/var/log`
via the hpm sandbox. Some features (eBPF tracing) require `disabled=false` + root.

## Tags

`@development` `@system` `@monitoring` `@profiling` `@security` `@advanced`

Install the whole monitoring bundle:
```sh
sudo hpm install @monitoring
```

## Authors

HackerOS Team <hackeros068@gmail.com>

## License

GPL-3.0
