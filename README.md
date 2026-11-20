# NVIDIA Linux VRR Multi-Plane Atomic KMS Stutter Repro

Minimal, standalone atomic KMS reproducer (`drm-plane-bench`) for a VRR stutter bug in NVIDIA's Linux display driver (`nvidia-drm` + `nvidia-modeset`). Linear dumb buffers, no modeset, no GPU rendering, no compositor.

## Bug Summary

When a DRM atomic commit updates **two or more planes** on a VRR-enabled head (`primary + overlay` framebuffer swap/move, or `primary + cursor` framebuffer swap/move), two coupled symptoms occur:

1. **Wire-level HDMI VRR break (all content):** even on plain, uniform scenes (`hbar`, `bar`, `solid`), VRR blanking extension breaks on the wire and the display spikes from the paced rate (e.g. `79.2 Hz` at 80 fps) to `118.6 Hz` (a refresh at `T_min = 8.34 ms`) on 41–49% of OSD samples with visible judder. On plain scenes, NVIDIA's DRM `flip_complete` timestamps still report `~0.13 ms`, masking the break from software. Single-plane `--exclusive` commits stay 100% locked (`78.95–79.64 Hz`, 0% >100 Hz).
2. **DRM `flip_complete` stall (horizontal scanline detail):** when a plane contains fine horizontal pixel variation along scanlines (`vstripes` with period ≤ 16 px, width ≥ 1800 px), the DRM `flip_complete` event is additionally held to `T_min + scanout(y_end)` (`12.67 ms` for top half, `16.35 ms` for full screen = `1.96× T_min`, 100% of multi-plane updates at 80–100 fps).

`--suite` uses `vstripes` to make the multi-plane VRR break directly measurable in DRM `flip_complete` timestamps without optical capture hardware, alongside `solid`/`hstripes` baselines, cursor-move, and a single-plane `--exclusive` control in the same 6-case (~42 s) run.

## Build & Run

```bash
# Rust (dynamic binary)
cargo build --release

# Or Nix (static musl binary)
nix build
```

List detected GPUs, active connectors, and `vrr_capable` (safe inside a graphical session):

```bash
./target/release/drm-plane-bench --list
```

Three report modes, each printing a Markdown report (7 s per case, pace 83% of refresh — of the slower head in `--dual-head`; override with `--seconds` / `--pace-hz`):

| Mode | Cases | Purpose |
|---|---|---|
| `--suite` (default of `run-from-session.sh`, ~42 s) | 6 | Quick repro: `solid` / `hstripes` controls, `vstripes` full / top half with overlay swap, cursor move, single-plane `--exclusive` control |
| `--regression` (~77 s) | 11 | Single-head full matrix: the 6 `--suite` cases plus `vstripes` overlay move (`Window 0 + Window 1 + WindowIMM` interlock), overlay move `--exclusive` (standalone `WindowIMM`), cursor FB swap (`Core + Window 0`), cursor-only updates on an otherwise idle primary (`Core`-only unstall; primary-only column `—`), and a `VRR=0` fixed-refresh control |
| `--dual-head` (~28 s) | 4 | Two active CRTCs on one device updated in the **same** atomic commit (one flip event per CRTC): both VRR heads flipping, VRR flip + VRR cursor-only head (both ways), VRR head + fixed-refresh head. Head A is `--connector` (or auto), head B is `--connector-b` (default: the next active CRTC); per-head `dt`, `commit→flip` and delayed stats |

`VRR_ENABLED` and secondary planes are restored on every CRTC at exit. Run from a terminal inside a graphical session (`run-from-session.sh` uses `sudo` only for VT switch / fbcon / temporary DRM ACLs; the binary itself runs unprivileged and sandboxed with `bwrap` when available):

```bash
./run-from-session.sh
./run-from-session.sh --suite --connector HDMI-A-1
./run-from-session.sh --regression
./run-from-session.sh --dual-head --connector HDMI-A-1 --connector-b DP-1
```

Or run manually from a spare VT (`Ctrl+Alt+F3`, no compositor holding DRM master):

```bash
./target/release/drm-plane-bench --suite
./target/release/drm-plane-bench --regression
./target/release/drm-plane-bench --dual-head
./target/release/drm-plane-bench --draw hbar --pace-hz 80 --seconds 10
./target/release/drm-plane-bench --pattern vstripes --pattern-rect 0,0,3840,2160 --exclusive --pace-hz 80 --seconds 10
```

## Root Cause & Fix

Verified via A/B kernel builds on `615.78.08` (`open-gpu-kernel-modules`):

- **`nvkms-evo3.c:nvEvoUpdateC3`:** emits `RELEASE_ELV_TRUE` on **every** mutually-interlocked channel (`Window 0 + Window 1`, `Core + Window 0`, `Window + WindowIMM`) under one-shot VRR instead of once per interlocked group, breaking one-shot stall-lock and triggering an unsolicited `T_min` repeat scanout.
- *(Note: position-only cursor moves hit the `Core + Window 0` interlock because `nvidia-drm-crtc.c:cursor_plane_req_config_update` always sets `req_config->flags.surfaceChanged = NV_TRUE`, pulling `Core` into the commit.)*

Deduplicating `RELEASE_ELV_TRUE` strictly within hardware interlock groups on active one-shot Gaming VRR heads in `nvkms-evo3.c` alone (single-file fix, no `nvidia-drm` changes) eliminates both the `16.35 ms` DRM stalls (`100% → 0%`) and the `118.6 Hz` TV-OSD VRR spikes (`41–49% → 0 / 416` single-head, `0 / 15` dual-head).

## Verified Results

### 1. Wire-Level Optical Verification (LG C1 VRR Information OSD at 80 fps)

Elgato Facecam OCR of the LG C1 `freesync-info` OSD (`HDMI-A-1`, `3840x2160 @ 119.88 Hz`, VRR on, `80.0 Hz` pace = `12.50 ms`). Stock TV-OSD runs were captured on stock proprietary `615.78.08` (whose DRM `--suite` timings match stock open+GSP within `0.01 ms`); patched runs were captured on `615.78.08` open+GSP with the single-file `nvkms-evo3.c` patch:

| Driver / Mode | Case | DRM Delayed | TV OSD Range (`p50`) | TV `>100 Hz` (`118.6 Hz` spikes) |
|---|---|---:|---|---:|
| Stock `615.78.08` | `hbar` + overlay swap (2 planes) | 0 / 80 (0.0%) | 70.30–118.68 Hz (79.61 Hz) | **43 / 94 (45.7%)** |
| Stock `615.78.08` | `hbar` + cursor move (2 planes) | 0 / 80 (0.0%) | 70.66–118.68 Hz (79.67 Hz) | **40 / 81 (49.4%)** |
| Stock `615.78.08` | `vstripes` + overlay swap (2 planes) | **80 / 80 (100.0%)** | 78.64–118.63 Hz (79.41 Hz) | **42 / 103 (40.8%)** |
| Stock `615.78.08` | `vstripes` + `--exclusive` (1 plane) | 0 / 80 (0.0%) | 78.95–79.64 Hz (79.18 Hz) | **0 / 101 (0.0%)** |
| Patched `615.78.08` open+GSP | `hbar` + overlay swap (`Win0 ↔ Win1`) | 1 / 120 (0.8%) | 78.25–80.96 Hz (79.22 Hz) | **0 / 81 (0.0%)** |
| Patched `615.78.08` open+GSP | `hbar` + overlay move (`Win0 ↔ Win1 + Win1IMM`) | 0 / 120 (0.0%) | 78.92–79.90 Hz (79.23 Hz) | **0 / 83 (0.0%)** |
| Patched `615.78.08` open+GSP | `hbar` + cursor move (`Core ↔ Win0`) | 0 / 120 (0.0%) | 78.46–80.37 Hz (79.22 Hz) | **0 / 82 (0.0%)** |
| Patched `615.78.08` open+GSP | `hbar` + cursor FB swap (`Core ↔ Win0`) | 0 / 120 (0.0%) | 79.00–79.90 Hz (79.25 Hz) | **0 / 80 (0.0%)** |
| Patched `615.78.08` open+GSP | `vstripes` + overlay swap (`Win0 ↔ Win1`) | 0 / 120 (0.0%) | 78.92–79.85 Hz (79.23 Hz) | **0 / 90 (0.0%)** |
| Patched `615.78.08` open+GSP | `dual: both VRR flips` (`HDMI-A-1` OSD, ~1.5 s) | 0 / 80 (0.0%) | 78.95–79.60 Hz (79.25 Hz) | **0 / 15 (0.0%)** |

### 2. Linux A/B: Stock vs Patched `615.78.08` open+GSP (`--suite`)

RTX 4090, Linux `7.3.4`, LG C1 `HDMI-A-1` (`3840x2160 @ 119.88 Hz`, `T_min = 8.34 ms`, VRR on), paced at `99.50 Hz` (`10.05 ms`), plane update every 8 frames. AMD RX 6900 XT on the same LG C1 shows no overlay-correlated delays (`60 Hz` and `120 Hz`, DRM benchmark only).

| Case | Plane update | Stock open+GSP: `dt` p50 (`×T_min`) | Stock: `commit→flip` p50/p95 | Stock: Delayed | Patched open+GSP: `dt` p50 (`×T_min`) | Patched: `commit→flip` p50/p95 | Patched: Delayed |
|---|---|---:|---:|---:|---:|---:|---:|
| `vstripes` (full) | overlay swap | **16.35 ms (1.96×)** | **6.45 / 6.46 ms** | **69 / 69 (100.0%)** | **10.03 ms (1.20×)** | **0.11 / 0.13 ms** | **0 / 75 (0.0%)** |
| `vstripes` (top half) | overlay swap | **12.67 ms (1.52×)** | **2.76 / 2.77 ms** | **72 / 72 (100.0%)** | **10.03 ms (1.20×)** | **0.12 / 0.13 ms** | **0 / 75 (0.0%)** |
| `vstripes` (cursor) | cursor move | **16.34 ms (1.96×)** | **6.43 / 6.45 ms** | **69 / 69 (100.0%)** | **10.04 ms (1.20×)** | **0.12 / 0.13 ms** | **0 / 75 (0.0%)** |
| `solid` (DRM-blind content) | overlay swap | 10.04 ms (1.20×) | 0.13 / 0.14 ms | 0 / 75 (0.0%) | 10.04 ms (1.20×) | 0.12 / 0.14 ms | 0 / 75 (0.0%) |
| `hstripes` (DRM-blind content) | overlay swap | 10.04 ms (1.20×) | 0.13 / 0.14 ms | 0 / 75 (0.0%) | 10.04 ms (1.20×) | 0.12 / 0.14 ms | 0 / 75 (0.0%) |

*(All remaining `--regression` cases — `--exclusive`, overlay move, overlay move `--exclusive`, cursor swap, cursor-only idle, `VRR=0` control — and simultaneous `--dual-head` `both VRR flips` also pass with `0 / 75` delayed on patched `615.78.08` open+GSP.)*

### 3. Windows 11 Control on the Same LG C1 (`HDMI_3`)

RTX 4090 Laptop GPU (driver `616.86`) driving the exact same LG C1 (`HDMI_3`, `3840x2160 @ 120 Hz` G-Sync), captured with synchronized PresentMon 2.2.0 + `Microsoft-Windows-DxgKrnl` ETW + Facecam TV-OSD OCR:

| Case | PresentMon `dt` p50 / p95 (`>15 ms`) | TV OSD `p50 / p90 / max` | TV `>116.5 Hz` | ETW `MMIOFlipMultiPlaneOverlay3` |
|---|---:|---|---:|---|
| `80 fps` `vstripes` + 10 Hz MPO | 12.50 / 13.07 ms (0) | 78.43 / 79.71 / 87.20 Hz | **0.0%** | `L0=61, L1=447, both=3` |
| `80 fps` `vstripes` + cursor move | 12.51 / 12.84 ms (0) | 78.31 / 80.78 / 82.60 Hz | **0.0%** | — |
| `100 fps` `vstripes` + 12 Hz MPO | 10.00 / 10.96 ms (1) | 100.60 / 108.94 / 113.40 Hz | **0.0%** | `L0=75, L1=559, both=3` |
| `100 fps` `vstripes` + cursor move | 10.01 / 10.42 ms (0) | 99.62 / 103.19 / 103.80 Hz | **0.0%** | — |

WDDM submits MPO plane flips as separate `PlaneCount = 1` calls (`L0` and `L1` independently, mirroring Linux `--exclusive`), and cursor moves do not appear in multi-plane `MMIOFlipMultiPlaneOverlay3` flips.
