# WxStore Benchmark Report

Date: 2026-04-29 local

## Built Service

`C:\Users\drew\wxstore` is a clean Rust service for the private WxStore direction:

- no Open-Meteo file dependency;
- no Open-Meteo code dependency;
- no service dependency on a CLI crate;
- run/lane/field manifests;
- canonical immutable gridpoint endpoints;
- Open-Meteo-shaped point forecast output for surface/spatial fields;
- compact JSON temporal sounding output;
- custom binary point payloads;
- full-grid binary map-source extraction;
- read-only multi-model spatial lane adapter;
- mmap/zstd reader for the custom point-temporal pressure-profile lane;
- precomputed diagnostic lane integration.

## Loaded Data

Temporal sounding lane:

```text
model:       hrrr
domain:      conus
cycle:       2026-04-29T06:00:00Z
horizon:     f000-f048
grid:        1799 x 1059
levels:      40 pressure levels
profile:     TMP, SPFH, UGRD, VGRD, HGT
diagnostics: precomputed diagnostic artifact, f000-f048
```

Surface/spatial lanes:

```text
hrrr:       20260405_18z, many hourly leads
gfs:        20260405_12z, many hourly leads
ecmwf_ifs:  20260411_12z, local sparse leads f000/f003
ecmwf_ens:  20260412_00z, member 001, local sparse lead f003
```

The ECMWF local files are sparse because that is what is present on disk. The API advertises available hours through `/v1/variables`.

## Service Command

```powershell
C:\Users\drew\wxstore\target\release\wxstore.exe serve `
  --profile-store C:\Users\drew\orwx-wx-profile-nvme\hrrr_20260429_06z_f000_f048_core_chunk8 `
  --diagnostic-store C:\Users\drew\rustwx\proof\aether_temporal_profile_mvp\diagnostic_conus_20260429_06z_f000_f048 `
  --spatial-root C:\Users\drew\open-rust-wx\data\spatial `
  --host 127.0.0.1 `
  --port 8897
```

## Smoke Tests

All returned 200:

```text
/v1/status
/v1/models
/v1/variables?model=hrrr&run=20260405_18z
/v1/forecast?latitude=35.22&longitude=-97.44&model=hrrr&run=20260405_18z&hourly=temperature_2m,dew_point_2m,wind_gusts_10m&forecast_hours=0-2
/v1/grid?model=hrrr&run=20260405_18z&variable=temperature_2m&forecast_hour=0&format=bin
/v1/point.bin?lat=35.22&lon=-97.44&hours=0-48&diagnostics=basic
/v1/temporal-sounding?lat=35.22&lon=-97.44&hours=0-2&diagnostics=basic
```

Surface forecast sample after unit normalization:

```text
HRRR / Oklahoma point / f000-f002:
temperature_2m = 14.55, 15.39, 16.02 degC
dew_point_2m   = 1.50, 1.11, 1.17 degC
wind_gusts_10m = 8.95, 8.86, 8.67 m/s
```

## Sequential HTTP Benchmarks

Tool:

```text
C:\Users\drew\open-rust-wx\target\release\raw-http-bench.exe
```

Point forecast products:

| Scenario | Requests | Concurrency | Failures | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR surface forecast, 3 vars x 3h | 30,000 | 192 | 0 | 19,317.7 | 4.69 ms | 5.40 ms | 5.89 ms | 1.06 KB |
| GFS surface forecast, 3 vars x 3h | 30,000 | 192 | 0 | 19,634.2 | 4.80 ms | 5.81 ms | 6.22 ms | 1.04 KB |
| ECMWF IFS surface forecast, 3 vars x 2h | 30,000 | 192 | 0 | 19,334.7 | 4.72 ms | 5.69 ms | 6.18 ms | 0.96 KB |
| ECMWF ENS member 001 surface forecast, 3 vars x 1h | 30,000 | 192 | 0 | 19,284.5 | 4.30 ms | 5.10 ms | 5.93 ms | 0.90 KB |
| HRRR 48h temporal sounding binary + basic diagnostics | 30,000 | 192 | 0 | 10,925.0 | 15.51 ms | 27.10 ms | 37.94 ms | 30.3 KB |
| HRRR 48h temporal sounding compact JSON + basic diagnostics | 10,000 | 96 | 0 | 3,852.4 | 24.60 ms | 32.76 ms | 36.25 ms | 112.6 KB |

Full-grid binary map-source products:

| Scenario | Requests | Concurrency | Failures | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR CONUS temperature grid `f000` | 300 | 16 | 0 | 535.1 | 29.39 ms | 40.67 ms | 47.47 ms | 7.27 MB |
| GFS global temperature grid `f000` | 300 | 16 | 0 | 998.8 | 15.40 ms | 22.91 ms | 27.08 ms | 3.96 MB |
| ECMWF IFS global temperature grid `f000` | 300 | 16 | 0 | 952.0 | 15.06 ms | 22.96 ms | 27.88 ms | 3.96 MB |
| ECMWF ENS member 001 global temperature grid `f003` | 300 | 16 | 0 | 995.6 | 14.98 ms | 24.91 ms | 30.89 ms | 3.96 MB |

## Profile Lane Build Matrix

All stores were built from the same HRRR `f000-f048` pressure-profile source.

| Store | Chunk shape | Bytes | GiB | Build time | Random 48h binary req/s |
| --- | --- | ---: | ---: | ---: | ---: |
| `chunk50` | `y=1,x=50,levels=40,hours=49` | 16,734,724,023 | 15.59 | 253.2 s | 5,202.0 |
| `chunk16` | `y=1,x=16,levels=40,hours=49` | 17,211,717,508 | 16.03 | 126.2 s | 7,188.9 |
| `chunk8` | `y=1,x=8,levels=40,hours=49` | 17,981,715,436 | 16.75 | 111.2 s | 10,925.0 to 12,148.9 |

`chunk8` remains the local winner for arbitrary point temporal soundings.

## Interpretation

The service now proves the combined product surface:

```text
surface point forecasts:
  model/run/variable/hour -> nearest gridpoint -> Open-Meteo-shaped hourly JSON

temporal soundings:
  HRRR gridpoint -> all hours -> all pressure levels -> compact JSON or binary

map/grid source:
  model/run/variable/hour -> full f32 grid as binary
```

The major product result is that surface forecast calls across the available local model families are all around `19k req/s` on this workstation after the spatial arrays are warm, and HRRR 48h binary temporal soundings are above `10k req/s` with the current diagnostic lane attached.

`latest-benchmark-results.json` is the machine-readable output from the latest run.
