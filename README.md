# 🕵️‍♂️ Local NEP Microinverter Gateway & Telemetry Parser

[![License: GPLv3](https://img.shields.io/badge/License-GPLv3-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/rust-2021-orange.svg)](https://www.rust-lang.org/)
[![Docker](https://img.shields.io/badge/docker-alpine-blue.svg)](Dockerfile)

A local, privacy-focused gateway (`nep-gw`) and protocol parser (`nep-protocol`) designed to intercept and parse telemetry payloads sent by rebranded **NEP BDM-400** and **NEP BDM-800** microinverters. 

It operates by spoofing the cloud endpoint `http://www.nepviewer.net/i.php`, parsing the unencrypted binary telemetry packets locally, and forwarding the decoded measurements directly to **Home Assistant (via MQTT)** and **Prometheus**.

---

## 🚀 Features

* **100% Local**: Run your solar balcony kit without cloud dependencies.
* **Axum-based HTTP Gateway**: Light and performant HTTP server implementing Time Sync responses expected by the inverter RTC.
* **Auto-Discovery in Home Assistant**: Automatic sensor creation in Home Assistant via MQTT Discovery (voltage, power, daily energy, temperature, frequency, reactive power, status).
* **Prometheus Metrics**: Scraping endpoint `/metrics` for custom Grafana dashboards.
* **NOM Parser**: Fast, safe binary parsing of the 45-byte payload implemented in Rust.
* **BDM-1200-LV Support (`/t.php`)**: Newer NEP firmware (WiFi fw 3.01.25+, e.g. the plug-in BDM-1200-LV) posts a **69-byte** payload to **`/t.php`** instead of the 45-byte `/i.php`. The gateway decodes AC power (÷25.6 W), frequency (÷256 Hz), DSP temperature (÷100 °C) and **per-input AC power** for all three PV inputs (offsets 43/49/55, summing to the total), validated against the NEP cloud on a live unit. The byte-53 word (÷51.2) is exposed as a diagnostic *internal* voltage — it is not grid RMS (it changes with the active PV input). Set `INVERTER_MODEL=BDM-1200-LV`. Per-input DC currents and the daily-energy word are not located yet (report 0 — derive energy in HA by integrating power).
* **BDM-400 & BDM-800 Support**: Handles both the single-input BDM-400 and the dual-MPPT BDM-800, including independent per-channel DC current readings on the BDM-800 (verified against a live packet capture). Set `INVERTER_MODEL` / `--model` to your model — it selects the payload field scales, not just Home Assistant metadata: the BDM-800's AC-power and daily-energy words use different scaling, calibrated against a reference meter (with BDM-400 scales a BDM-800 under-reads power by ~27%). See `CLAUDE.md` for the calibration data.
* **Optional Cloud Passthrough**: Dual-delivery mode (`--forward-upstream`) relays packets to the real NEP cloud so the official app keeps working alongside local monitoring.
* **DC Voltage & Power Reconstruction**: Dynamically estimates DC PV voltage and panel power using standard inverter efficiency curves (since the inverter natively omits DC PV voltage from its uploads). ⚠️ The reconstruction assumes the BDM-400's panel topology — on the BDM-800 the AC-side values and per-channel DC currents are reliable, but treat reconstructed DC voltage/power/efficiency with skepticism.

---

## 📐 The Telemetry Protocol

Older firmware uploads unencrypted HTTP `POST` requests to `/i.php` with a fixed-length **45-byte** binary payload (documented below). Newer firmware (e.g. BDM-1200-LV) posts a **69-byte** payload to **`/t.php`** — same `0x79`/`0x4014` framing and dual checksums, extended data section; see [REVERSE_ENGINEERING.md](REVERSE_ENGINEERING.md) for its field map. The 45-byte structure:

### 45-Byte Packet Layout

| Offset (dec) | Size (bytes) | Type | Scale / Format | Metric Description |
| :--- | :--- | :--- | :--- | :--- |
| `0` | 1 | `uint8` | Constant `0x79` | Packet framing start signature |
| `1–2` | 2 | `uint16` (LE) | `38` | Payload length from offset 5 to 42 |
| `3–4` | 2 | `uint16` (BE) | `0x4014` | Fixed command identifier |
| `5–12` | 8 | `bytes` | model-dependent | Gateway/AP identifier (`0xFF` padding on BDM-400, other values on BDM-800; not matched by the parser) |
| `13–14` | 2 | `uint16` (LE) | `28` | Length of data section (offset 15 to 42) |
| `15–18` | 4 | `bytes` | `0xC3C3C3C3` | Data section synchronization header (`0xFFFFFFFF` observed on the first report after an inverter reset; not matched by the parser — integrity comes from the checksums) |
| `19–22` | 4 | `uint32` (LE) | Hex Integer | Inverter Serial Number |
| `23–24` | 2 | `uint16` (LE) | `0` | General status / padding |
| `25–26` | 2 | `uint16` (LE) | `/ 100.0` (W) on BDM-400, `/ 25π ≈ 78.54` (W) on BDM-800 | AC Active Power Output in Watts (BDM-800 scale calibrated against a reference meter, flat ×(4/π) vs the BDM-400 scale across 20–620 W) |
| `27–28` | 2 | `uint16` (LE) | `/ 25.6` (V) | Internal voltage measurement — ⚠️ not grid RMS voltage: on a live BDM-800 it rests at V_peak/2 when idle and droops with output power when generating, coinciding with grid voltage only around 400–550 W (see `CLAUDE.md`) |
| `29–30` | 2 | `uint16` (LE) | Bitmask / Vref | Internal flags and DSP Reference Voltage |
| `31–32` | 2 | `2 × uint8` | `/ 10.0` (A) each | DC Input Current per MPPT channel (byte 31 = CH1, byte 32 = CH2; CH1 always `0` on the single-input BDM-400 — channel order inferred from a single BDM-800 capture) |
| `33–34` | 2 | `uint16` (LE) | `/ 256.0` (Hz) | Grid AC Frequency in Hertz (Q8 Hz) |
| `35–36` | 2 | `uint16` (LE) | `/ 100.0` (°C) | DSP Temperature in Celsius |
| `37–38` | 2 | `uint16` (LE) | `× 0.2` (Wh) on BDM-400, `× 0.2308` (Wh) on BDM-800 | Daily Energy Accumulator in Wh (resets at dawn, not midnight; on a manual inverter reset it can warp back to a stale flash-persisted value) |
| `39–40` | 2 | `uint16` (LE) | Bitmask | Firmware Version & Relay/State bits |
| `41–42` | 2 | `int16` (LE) | `/ 100.0` (VAR) | AC Reactive Power in signed Volt-Amperes Reactive |
| `43` | 1 | `uint8` | sum % 256 | **Additive Checksum** (bytes 1 to 42) |
| `44` | 1 | `uint8` | XOR reduction | **XOR Checksum** (bytes 1 to 42) |

---

## 🛠️ Getting Started

### Prerequisites

* Rust compiler toolchain (Edition 2021) or Docker installed.
* An MQTT broker (e.g. Mosquitto) for Home Assistant integration.
* DNS hijacking/spoofing setup on your local network (e.g., DNS spoofing in MikroTik or dnsmasq) to redirect `www.nepviewer.net` to the machine hosting `nep-gw`.

### Environment Configuration

The gateway is configured via the following environment variables:

| Variable | Description | Default |
| :--- | :--- | :--- |
| `PORT` | The port the HTTP server binds to | `80` |
| `MQTT_HOST` | Hostname/IP of your MQTT broker | `::1` |
| `MQTT_PORT` | Port of your MQTT broker | `1883` |
| `MQTT_USERNAME` | Username for MQTT broker authentication (omit for anonymous) | *(none)* |
| `MQTT_PASSWORD` | Password for MQTT broker authentication (requires `MQTT_USERNAME`) | *(empty)* |
| `RUST_LOG` | Tracing logging level (`info`, `debug`, `error`) | `info` |
| `INVERTER_MODEL` | Inverter model: selects payload field scales and Home Assistant metadata (`BDM-400`, `BDM-800`, or `BDM-1200-LV`); also `--model <name>`. `/t.php` traffic always uses BDM-1200-LV scales regardless of this setting. | `BDM-400` |
| `FORWARD_UPSTREAM` | Set to `true` to also relay inverter POSTs to the real NEP cloud (dual-delivery mode) | `false` |
| `UPSTREAM_URL` | Upstream endpoint used in dual-delivery mode (plain HTTP only) | `http://www.nepviewer.net/i.php` |

### Dual-Delivery Mode (optional cloud passthrough)

By default `nep-gw` fully replaces the NEP cloud: the official app/portal stops
updating once the inverter's DNS points here. To keep the cloud working too,
enable dual-delivery with the `--forward-upstream` CLI flag (or
`FORWARD_UPSTREAM=true`):

```bash
nep-gw --forward-upstream
# or pin the upstream explicitly:
nep-gw --forward-upstream --upstream-url http://1.2.3.4/i.php
```

Every raw inverter POST is then relayed in the background to the real
`www.nepviewer.net` — even packets the local parser doesn't understand. The
inverter always gets the local time-sync response immediately, so a cloud
outage never affects local operation. Relay outcomes are counted in the
`nep_upstream_forwards_total` / `nep_upstream_forward_errors_total` Prometheus
metrics.

**Requirement:** the machine running `nep-gw` must resolve the *real* IP of
`www.nepviewer.net` (scope your DNS spoof to the inverter, or give the gateway
host an honest resolver). If the host's DNS is also spoofed, either pin the
real IP with `--upstream-url http://<real-ip>/i.php` (the correct `Host` header
is always sent), or leave it — relayed requests carry an `X-NEP-GW-Relay`
marker and the gateway refuses to re-forward them, so a DNS loop degrades into
a logged warning rather than an infinite relay loop.

---

## ⚙️ Running as a systemd Service

A hardened unit file is provided as `nep-gw.service`. It runs the gateway as an
unprivileged dynamic user with `AmbientCapabilities=CAP_NET_BIND_SERVICE`, so it
can bind port 80 without root and without `setcap` on the binary:

```bash
cargo build --release
sudo install -m 755 target/release/nep-gw /usr/local/bin/nep-gw
sudo install -m 644 nep-gw.service /etc/systemd/system/nep-gw.service

# Broker address and credentials live in a root-only env file:
sudo install -m 600 /dev/null /etc/nep-gw.env
echo -e 'MQTT_HOST=192.168.1.50\nMQTT_USERNAME=homeassistant\nMQTT_PASSWORD=secret' | sudo tee /etc/nep-gw.env > /dev/null

sudo systemctl daemon-reload
sudo systemctl enable --now nep-gw
journalctl -u nep-gw -f
```

The unit enables dual-delivery by default (`--forward-upstream` in
`ExecStart`); remove the flag from the unit to run purely locally.
After rebuilding, re-run the `install` step and `sudo systemctl restart nep-gw`.

---

## 🐳 Docker Deployment

The gateway is packaged in an optimized Alpine-based multi-stage Dockerfile that supports compilation for multi-arch targets (including `aarch64` ARM targets like Raspberry Pi).

### Build Image
```bash
docker build -t nep-gw .
```

### Run Container
```bash
docker run -d \
  --name nep-gw \
  -p 80:80 \
  -e PORT=80 \
  -e MQTT_HOST=192.168.1.50 \
  -e MQTT_PORT=1883 \
  -e RUST_LOG=info \
  --restart unless-stopped \
  nep-gw
```

---

## 🦀 Rust Workspace Structure

The project is structured as a Cargo workspace containing:
1. **`nep-protocol`**: A standalone Rust library that uses `nom` to parse the 45-byte telemetry payloads, validate the dual checksums, and perform scaling conversions.
2. **`nep-gw`**: The HTTP gateway daemon written with `axum`. It intercepts `/i.php`, calls the parser library, updates Prometheus metrics, and manages the MQTT client loop for Home Assistant discovery.

### Building locally
```bash
cargo build --release
```

### Running unit tests
```bash
cargo test
```

---

## 🔌 Integrations

### Home Assistant MQTT Discovery
On receiving telemetry from a new microinverter serial number, the gateway automatically publishes MQTT Discovery configuration topics to `homeassistant/sensor/nep_<serial_number>/...`.
Sensors exposed:
* `AC Power` (W)
* `AC Voltage` (V)
* `Grid Frequency` (Hz)
* `DC Current` (A)
* `DC Voltage` (V, reconstructed)
* `DC Power` (W, reconstructed)
* `Temperature` (°C)
* `Daily Energy` (Wh)
* `Reactive Power` (VAR)
* `Inverter Status` (operating state)

### Prometheus Scraping
The gateway exposes a standard Prometheus `/metrics` endpoint on the configured HTTP port. Metrics exported:
* `nep_packets_received_total`
* `nep_ac_power_watts`
* `nep_ac_voltage_volts`
* `nep_dc_current_amperes`
* `nep_ac_frequency_hertz`
* `nep_dc_voltage_volts`
* `nep_temperature_celsius`
* `nep_daily_energy_watthours`
* `nep_reactive_power_var`

---

## 📜 License

This project is licensed under the **GNU GPLv3** - see the [LICENSE](LICENSE) file for details. (The badge and LICENSE file are GPLv3; earlier README text mistakenly said MIT.)

## 🤝 Credits & Lineage

This is a fork chain: **[Nic0w/nep-local-gw](https://github.com/Nic0w/nep-local-gw)** (original local NEP gateway, GPLv3) → **[cronnelly/nep-local-gw](https://github.com/cronnelly/nep-local-gw)** (BDM-800 support, optional cloud passthrough, MQTT auth) → this fork (**BDM-1200-LV `/t.php`** support, byte-exact raw-TCP upstream forwarder for the picky cloud endpoint, native multi-arch Docker build).


Special thanks to the community efforts, particularly **[BlinxFox/nep-gw](https://github.com/BlinxFox/nep-gw)**, for providing the initial hardware references and inspiration for local BDM-600/MMI-600 telemetry redirection.
