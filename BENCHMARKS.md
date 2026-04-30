# WxStore Benchmark Report

Date: 2026-04-29 local / 2026-04-30 UTC

## What Was Built

Created `C:\Users\drew\wxstore`, a clean Rust service for the private WxStore direction:

- no Open-Meteo file dependency;
- no Open-Meteo code dependency;
- no `rustwx-cli` service dependency;
- model/domain/run/lane oriented manifests;
- canonical immutable gridpoint endpoints;
- lat/lon resolver endpoints;
- compact JSON temporal sounding response;
- custom binary point response;
- mmap/zstd reader for the custom `.wxp` pressure-profile lane;
- wrapper for the existing precomputed diagnostic brick as `diag_scalar_basic/sparse_v0`.

Current loaded data:

```text
model:       hrrr
domain:      conus
cycle:       2026-04-29T06:00:00Z
horizon:     f000-f048
grid:        1799 x 1059
levels:      40 pressure levels
profile:     TMP, SPFH, UGRD, VGRD, HGT
diagnostics: sparse precomputed brick, 1620 points, f000-f048
```

## Profile Lane Build Matrix

All stores were built from:

```text
C:\Users\drew\orwx-native-profile-nvme\hrrr_20260429_06z_f000_f048_core
```

Build command shape:

```powershell
C:\Users\drew\open-rust-wx\target\release\orwx.exe build-wx-profile `
  --source-native-profile-store C:\Users\drew\orwx-native-profile-nvme\hrrr_20260429_06z_f000_f048_core `
  --out-dir <out> `
  --variables TMP,SPFH,UGRD,VGRD,HGT `
  --hours 0-48 `
  --chunk-x <N> `
  --parallelism 5
```

| Store | Chunk shape | Bytes | GiB | Build time |
| --- | --- | ---: | ---: | ---: |
| `chunk50` | `y=1,x=50,levels=40,hours=49` | 16,734,724,023 | 15.59 | 253.2 s |
| `chunk16` | `y=1,x=16,levels=40,hours=49` | 17,211,717,508 | 16.03 | 126.2 s |
| `chunk8` | `y=1,x=8,levels=40,hours=49` | 17,981,715,436 | 16.75 | 111.2 s |

`chunk8` is the current winner for arbitrary point serving.

## Service

Current process:

```text
http://127.0.0.1:8897
profile store: C:\Users\drew\orwx-wx-profile-nvme\hrrr_20260429_06z_f000_f048_core_chunk8
diagnostic store: C:\Users\drew\rustwx\proof\aether_temporal_profile_mvp\diagnostic_conus_20260429_06z_f000_f048
```

Run command:

```powershell
C:\Users\drew\wxstore\target\release\wxstore.exe serve `
  --profile-store C:\Users\drew\orwx-wx-profile-nvme\hrrr_20260429_06z_f000_f048_core_chunk8 `
  --diagnostic-store C:\Users\drew\rustwx\proof\aether_temporal_profile_mvp\diagnostic_conus_20260429_06z_f000_f048 `
  --host 127.0.0.1 `
  --port 8897
```

## Sequential HTTP Benchmarks

Tool:

```text
C:\Users\drew\open-rust-wx\target\release\raw-http-bench.exe
```

### Winning Store: `chunk_x=8`

| Scenario | Requests | Concurrency | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Random lat/lon, 48h binary, profile+basic diag | 30,000 | 192 | 12,148.9 | 14.4 ms | 22.9 ms | 27.6 ms | 30.3 KB |
| Random lat/lon, 48h binary, profile only | 30,000 | 192 | 3,521.2 | 53.1 ms | 71.9 ms | 81.3 ms | 23.1 KB |
| Random lat/lon, 48h compact JSON, profile+basic diag | 30,000 | 192 | 2,457.3 | 59.5 ms | 149.8 ms | 574.3 ms | 112.6 KB |
| Random lat/lon, 1h binary, profile+basic diag | 30,000 | 192 | 3,615.6 | 52.2 ms | 71.0 ms | 80.3 ms | 4.8 KB |
| Random lat/lon, 0-18h binary, profile+basic diag | 30,000 | 192 | 4,059.8 | 46.2 ms | 63.3 ms | 71.9 ms | 14.4 KB |
| Hot canonical gridpoint, 48h binary, cached bytes | 30,000 | 192 | 19,252.5 | 4.6 ms | 5.4 ms | 13.3 ms | 30.3 KB |
| Hot canonical gridpoint, 48h compact JSON, cached bytes | 30,000 | 192 | 19,361.0 | 4.8 ms | 5.8 ms | 9.6 ms | 111.5 KB |

Notes:

- The `profile only` and shorter-hour rows were run after other stress passes, so treat them as supporting numbers, not the headline.
- Single-hour is not much cheaper than 48h because the current profile lane is intentionally all-hours-per-point. A separate single-hour/spatial lane is needed for cheap one-hour-only calls.
- Cached canonical endpoints serve final bytes and therefore are limited mostly by HTTP and local loopback throughput.

### Chunk Layout Comparison

Same scenario for each store:

```text
/v1/point.bin?lat={lat}&lon={lon}&hours=0-48&diagnostics=basic
```

| chunk_x | Requests | Concurrency | Req/s | P50 | P95 | P99 | Avg payload |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 50 | 15,000 | 96 | 5,202.0 | 16.8 ms | 31.9 ms | 44.6 ms | 30.3 KB |
| 16 | 30,000 | 192 | 7,188.9 | 24.2 ms | 39.1 ms | 50.4 ms | 30.3 KB |
| 8 | 30,000 | 192 | 12,148.9 | 14.4 ms | 22.9 ms | 27.6 ms | 30.3 KB |

## Interpretation

The physical serving idea is validated:

```text
point -> all forecast hours -> all pressure levels
```

For arbitrary 48-hour point soundings, the winning local profile lane reads one small temporal-profile chunk per variable, then assembles either compact JSON or a binary payload. `chunk_x=8` is the best tested profile chunk size because it wastes far less decompression work per random point than `chunk_x=50` while only adding about 1.25 GB over the original store.

The real production shape should keep these separate:

- `profile_pressure_core`: point-temporal profile lane, current winner.
- `surface_ts`: surface time-series lane, not built in this repo yet.
- `diag_scalar_basic/severe/parcel`: dense diagnostic lanes, not sparse.
- `raster_tiles`: map/spatial lane.
- `response_blobs`: optional pre-shaped cached point products.

## Bottom Line

On this local Windows workstation, the clean WxStore service can already serve random unique 48-hour HRRR temporal-sounding binary responses with basic diagnostics at about `12.1k req/s` and hot canonical cached responses at about `19.3k req/s`.

The remaining work is not proving the core point-temporal store. The remaining work is building the missing production lanes: dense diagnostics, surface time-series, maps/raster tiles, run publishing, and multi-model builders.
