# ros2probe

https://csi-dgist.github.io/ros2probe-page/

Host-level observability for ROS 2 DDS traffic — without creating ROS 2 subscriptions.

ros2probe attaches an eBPF socket filter to every non-loopback network interface, captures RTPS/DDS packets in the kernel, and reconstructs the full ROS graph, topic metrics, and message streams entirely in userspace. A CLI (`rp`) and a desktop GUI (`rp gui`) talk to the runtime over a Unix socket.

---

## How It Works

```
┌──────────────────────────────────────────────────────────┐
│  Linux Kernel                                            │
│  ┌─────────────────────────────────────────────────────┐ │
│  │  eBPF socket filter (ebpf/)                         │ │
│  │  • Attached to AF_PACKET sockets (one per iface)    │ │
│  │  • Passes only RTPS/DDS UDP packets, drops the rest │ │
│  │  • Checks GID filter map to allow specific topics   │ │
│  └──────────────────────┬──────────────────────────────┘ │
│                         │ TPACKET_V3 ring buffer         │
│  ┌──────────────────────▼──────────────────────────────┐ │
│  │  AF_PACKET socket (netring / TPACKET_V3)            │ │
│  │  • Zero-copy batch reads from kernel ring buffer    │ │
│  └──────────────────────┬──────────────────────────────┘ │
└─────────────────────────│────────────────────────────────┘
                          │ mmap'd ring buffer read
┌─────────────────────────▼────────────────────────────────┐
│  Runtime  (rp run)                                       │
│  • IPv4/IPv6 fragment reassembly                         │
│  • RTPS/SPDP/SEDP discovery reconstruction               │
│  • GID → node name mapping via ros_discovery_info        │
│  • Topic graph: publishers, subscribers, SHM vs network  │
│  • Live hz / bw / delay / echo observers                 │
│  • MCAP bag recorder                                     │
│  • Command socket  /tmp/ros2probe.sock                   │
└────────────┬─────────────────────────┬───────────────────┘
             │ Unix socket (JSON-RPC)  │
     ┌───────▼──────┐         ┌────────▼────────┐
     │  rp (CLI)    │         │  rp gui         │
     │  topic / bag │         │  Dashboard      │
     │  node / svc  │         │  Topic Monitor  │
     │  action      │         │  Bag Recorder   │
     └──────────────┘         └─────────────────┘
```

> **DDS middleware only.** Zenoh support is planned.

---

## Features

- **Zero ROS subscriptions** — traffic is observed passively at the socket level; ros2probe never joins the DDS graph
- **Full graph reconstruction** — participants, endpoints, node names, and publisher/subscriber relationships derived from SPDP/SEDP + `ros_discovery_info`
- **SHM vs network distinction** — topics communicated exclusively over shared memory (FastDDS/CycloneDDS SHM locators) are identified and excluded from recording
- **Live topic metrics** — publish rate (hz), bandwidth (bw), end-to-end delay, and message echo with sliding-window statistics
- **MCAP recording** — bag files with optional zstd/lz4 compression, pause/resume, and topic selection
- **Recordable topic classification** — tf, parameter_events, rosout, debug topics, and SHM-only topics are automatically separated from recordable topics in all UIs
- **CLI + GUI** — same data exposed through both interfaces

---

## Requirements

| Requirement | Notes |
|---|---|
| Linux 5.15+ | eBPF socket filter, AF_PACKET TPACKET_V3 |
| ROS 2 Humble+ | Iron, Jazzy, Rolling also supported |
| Python 3 + `rclpy` | Only for `rp topic echo` / `rp topic delay` |
| Optional | Jumbo frames, DDS XML profile that avoids IP fragmentation |

---

## Install

No Rust, no compiler, no build dependencies required — just download and run.

```sh
# CLI only (default)
curl -fsSL https://github.com/csi-dgist/ros2probe/releases/latest/download/install.sh | sh

# CLI + GUI
curl -fsSL https://github.com/csi-dgist/ros2probe/releases/latest/download/install.sh | sh -s -- --gui
```

Automatically detects your architecture (x86-64, aarch64) and installs `rp` to `/usr/local/bin`.

## Quick Start

**Host A**:

```sh
ros2 run demo_nodes_cpp talker
```

**Host B**:

```sh
ros2 run demo_nodes_cpp listener
```

**Host A or B** (run ros2probe on either host to observe network traffic):

```sh
# Start the runtime
rp run

# In another terminal, observe with rp
rp topic list
rp topic hz /chatter
rp topic bw /chatter
rp bag record /chatter -o session.mcap

# Or open the GUI
rp gui
```

---

## CLI Reference

### `rp run`

Starts the runtime daemon. Automatically re-executes with `sudo` if not already root (prompts for password if needed).

```sh
rp run
```

---

### `rp gui`

Launches the desktop GUI. The runtime (`rp run`) must already be running.

```sh
rp gui
```

---

### `rp topic`

| Command | Description |
|---|---|
| `rp topic list` | List all topics; recordable topics shown first, internal topics in a separate section |
| `rp topic info <topic>` | Publishers, subscribers, type, and QoS |
| `rp topic type <topic>` | Print topic type string |
| `rp topic find <type>` | Find topics matching a given type |
| `rp topic hz <topic>` | Live publish rate with mean/min/max/stddev |
| `rp topic bw <topic>` | Live bandwidth with mean/min/max message sizes |
| `rp topic delay <topic>` | End-to-end delay from `header.stamp` with statistics |
| `rp topic echo <topic>` | Stream decoded messages (requires Python + rclpy) |

**Common flags:**

```
-w, --window <N>    Sliding window size for statistics (default: 10000 for hz/delay, 100 for bw)
--raw               Print raw bytes instead of decoded fields (echo)
--field <path>      Extract a single field, e.g. --field pose.pose.position (echo)
--once              Print one message then exit (echo)
--csv               CSV output for piping to plotting tools (echo)
```

> **Note:** Internal topics (tf, `parameter_events`, `rosout`, debug topics, SHM-only topics) are not supported by `hz`, `bw`, `delay`, or `echo` and will produce no output.

---

### `rp bag`

| Command | Description |
|---|---|
| `rp bag record [TOPICS...]` | Record listed topics to an MCAP file |
| `rp bag record --all` | Record all recordable topics |

**Flags:**

```
-a, --all                       Record all recordable topics
-o, --output <FILE>             Output file path (default: rosbag2_<timestamp>.mcap)
    --compression-format <FMT>  none | zstd | lz4 (default: none)
    --no-discovery              Skip ros_discovery_info topic
    --start-paused              Begin in paused state
```

**Interactive controls while recording:**

| Key | Action |
|---|---|
| `Space` | Pause / Resume |
| `Ctrl+C` | Stop and finalize |

> **Note:** Internal topics are never captured even with `--all`.

---

### `rp node`

| Command | Description |
|---|---|
| `rp node list` | List all discovered nodes |
| `rp node info <node>` | Publishers, subscribers, services, and actions for a node |

---

### `rp service`

| Command | Description |
|---|---|
| `rp service list` | List all services |
| `rp service type <service>` | Print service type |
| `rp service find <type>` | Find services by type |

---

### `rp action`

| Command | Description |
|---|---|
| `rp action list` | List all actions |
| `rp action info <action>` | Print action details |

---

### `rp discover`

Forces a `ros_discovery_info` broadcast over UDP, which triggers participant re-announcement from all running nodes. Useful when graph state appears stale.

```sh
rp discover
```

---

## GUI

Launch with:

```sh
rp gui
```

The GUI connects to the runtime on startup and refreshes state periodically. Three pages are available:

### Dashboard

- Live ROS resource counts (nodes, topics)
- System metrics (CPU, memory, network I/O) with scrolling history charts
- Interactive ROS graph with pan/zoom
- **Graph filters** (all enabled by default):
  - Hide tf topics
  - Hide parameter topics
  - Hide debug/rosout topics
  - Hide leaf topics (no subscribers)
  - Hide network-only (SHM-only) topics

### Topic Monitor

- Select any topic from the graph view
- Live **hz**, **bw**, and **delay** panels with history charts and statistics
- **Echo** panel showing the last 100 decoded messages
- Configurable window size
- Recordable topics shown at top; internal topics collapsed under a toggle

### Bag Recorder

- Topic list with recordable/internal separation
- Multi-topic selection, compression format, output path picker
- Live recording status: elapsed time, file size, messages per topic
- Pause/resume button

---

## Topic Classification

ros2probe classifies every topic into **recordable** or **internal**:

| Category | Criteria |
|---|---|
| **tf topics** | Name starts with `/tf` |
| **Parameter topics** | Name contains `/parameter_events` or `/rosout` |
| **Debug topics** | Name contains `/_` (hidden/debug convention) |
| **Leaf topics** | No active subscribers |
| **SHM-only topics** | No publisher or subscriber participant seen from a remote IP via SPDP |

A topic is **recordable** only if it passes all five filters. Internal topics remain visible in the GUI (under a collapsible section) so their info and graph connections can still be inspected.

---

## Project Layout

```
ros2probe/
├── ebpf/                 # eBPF kernel program (no_std)
│   ├── src/filter.rs     # Socket filter entry point
│   ├── src/rtps.rs       # RTPS header parsing in kernel
│   └── src/maps.rs       # Shared maps
│
├── common/               # Shared kernel↔userspace types (no_std)
│   ├── src/ebpf.rs       # Map key/value types
│   ├── src/rtps.rs       # RTPS structs
│   └── src/event.rs      # Event definitions
│
└── ros2probe/            # Userspace runtime + CLI + GUI
    └── src/
        ├── bin/          # rp (CLI + runtime + GUI entry points)
        ├── capture/      # Packet reassembly (IPv4/IPv6 fragments)
        ├── cli/          # rp subcommands
        ├── command/      # Unix socket server and protocol
        ├── discovery/    # RTPS/SPDP/SEDP + ros_discovery_info parsing
        ├── gui/          # egui/eframe pages
        ├── protocols/    # DDS/CDR deserialization
        ├── recorder/     # MCAP writer
        └── runtime/      # Main event loop, observers
```

---

## Contact

ros2probe is developed at the **DGIST CSI Lab**. For questions about the design,
collaboration proposals, bug reports that don't fit a GitHub issue, or anything
else related to the project or the accompanying paper, feel free to reach out:

- Sanghoon Lee — leesh2913@dgist.ac.kr
- Jisang Yu — julienyu@dgist.ac.kr

GitHub issues are still the preferred channel for reproducible bugs and feature
requests so the discussion stays public.
