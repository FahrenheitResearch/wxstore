# WxStore

WxStore is the clean private storage/API layer for model-agnostic weather products.
It does not depend on Open-Meteo file formats or code.

This first implementation serves an HRRR CONUS f000-f048 pressure-profile lane from
custom `.wxp` files and exposes the final product shape:

- run/lane/field manifests;
- canonical model/domain/run/gridpoint URLs;
- compact JSON temporal soundings;
- binary point payloads for high-throughput clients;
- optional precomputed diagnostic brick integration.

The current HRRR profile lane is the first concrete lane, not the whole architecture.
Future lanes should add surface time series, dense diagnostics, maps, polygons, ensembles,
AI model outputs, and response blob caches under the same manifest contracts.

## Run

```powershell
cargo run --release -- serve `
  --profile-store C:\Users\drew\orwx-wx-profile-nvme\hrrr_20260429_06z_f000_f048_core_chunk8 `
  --diagnostic-store C:\Users\drew\rustwx\proof\aether_temporal_profile_mvp\diagnostic_conus_20260429_06z_f000_f048 `
  --port 8897
```

## Endpoints

- `GET /v1/status`
- `GET /v1/latest/{model}/{domain}`
- `GET /v1/resolve?lat=35.22&lon=-97.44`
- `GET /v1/temporal-sounding?lat=35.22&lon=-97.44&hours=0-48&diagnostics=basic`
- `GET /v1/point.bin?lat=35.22&lon=-97.44&hours=0-48&diagnostics=basic`
- `GET /v1/runs/{model}/{domain}/{run}/grid/{x}/{y}/temporal-sounding?hours=0-48`
- `GET /v1/runs/{model}/{domain}/{run}/grid/{x}/{y}/temporal-sounding.bin?hours=0-48`

## Current Local Benchmark

Loaded run:

- `model=hrrr`, `domain=conus`, `cycle=2026-04-29T06:00:00Z`
- HRRR CONUS grid: `1799 x 1059`
- Forecast horizon: `f000-f048`
- Profile lane: `TMP, SPFH, UGRD, VGRD, HGT`, 40 pressure levels
- Winning local profile layout: `chunk_x=8`, `chunk_y=1`, `hours=49`, `levels=40`
- Profile store size: `17,981,715,436 bytes` / `16.75 GiB`
- Diagnostic lane: existing sparse precomputed diagnostic brick wrapped as `diag_scalar_basic/sparse_v0`

Sequential raw HTTP benchmark on this local Windows workstation, release build, service on `127.0.0.1:8897`:

| Scenario | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: |
| Random lat/lon, 48h binary, profile+basic diag | 12,148.9 | 14.4 ms | 22.9 ms | 27.6 ms | 30.3 KB |
| Random lat/lon, 48h compact JSON, profile+basic diag | 2,457.3 | 59.5 ms | 149.8 ms | 574.3 ms | 112.6 KB |
| Hot canonical gridpoint, 48h binary, cached bytes | 19,252.5 | 4.6 ms | 5.4 ms | 13.3 ms | 30.3 KB |

Chunk comparison for random 48h binary profile+basic diagnostics:

| Profile chunk_x | Store bytes | Build time | Req/s |
| ---: | ---: | ---: | ---: |
| 50 | 16,734,724,023 | 253.2 s | 5,202.0 |
| 16 | 17,211,717,508 | 126.2 s | 7,188.9 |
| 8 | 17,981,715,436 | 111.2 s | 12,148.9 |

The product lesson is clear: for arbitrary point temporal soundings, smaller point-temporal chunks are worth the modest disk increase. Binary is the scalable default for heavy users; JSON remains a compatibility/debug shape.

## Current Gaps

- Dense CONUS diagnostic lanes are not built yet. The API contract is ready, but this local run still uses the existing sparse diagnostic brick.
- Surface time-series lanes are represented in the manifest model but not loaded on this node.
- The current `.wxp` reader is a v0 compatibility lane; the next container should be promoted to generic `.wxa` lane files with checksums, per-chunk stats, and richer codec metadata.
