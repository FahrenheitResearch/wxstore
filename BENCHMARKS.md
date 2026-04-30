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
- native WXA dense2d spatial product materialization and serving;
- full-grid binary map-source extraction;
- read-only multi-model Zarr spatial source adapter for data not yet materialized to WXA;
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

Native WXA product files were materialized from those local lanes for currently available dependencies. The ECMWF local files are sparse because that is what is present on disk. The API advertises available hours through `/v1/variables`.

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
/v1/mapbox/layers/hrrr/20260405_18z/vpd_2m?hours=0-2&palette=magma&range=0,5
/v1/mapbox/tiles/hrrr/20260405_18z/vpd_2m/f000/4/3/6.png?palette=magma&range=0,5
/v1/mapbox/tiles/hrrr/hrrr_20260429_060000/500mb_temperature/f000/4/3/6.png?palette=temperature&range=-40,20
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
| HRRR WXA derived forecast, 3 vars x 3h | 30,000 | 192 | 0 | 19,485.6 | 4.76 ms | 5.41 ms | 8.96 ms | 1.07 KB |
| GFS WXA derived forecast, 3 vars x 3h | 30,000 | 192 | 0 | 19,469.0 | 4.65 ms | 5.32 ms | 24.84 ms | 1.05 KB |
| ECMWF IFS WXA dewpoint depression, 2h | 30,000 | 192 | 0 | 19,293.4 | 4.61 ms | 5.47 ms | 25.35 ms | 0.83 KB |
| ECMWF ENS member 001 WXA dewpoint depression, 1h | 30,000 | 192 | 0 | 19,405.3 | 4.65 ms | 5.44 ms | 16.76 ms | 0.80 KB |
| HRRR WXA 24h windowed forecast, 3 vars x 1h | 30,000 | 192 | 0 | 19,508.5 | 4.83 ms | 5.70 ms | 26.51 ms | 0.94 KB |
| HRRR 48h temporal sounding binary + basic diagnostics | 30,000 | 192 | 0 | 11,313.2 | 14.91 ms | 27.07 ms | 34.87 ms | 30.3 KB |
| HRRR 48h temporal sounding compact JSON + basic diagnostics | 10,000 | 128 | 0 | 4,433.0 | 27.85 ms | 40.74 ms | 47.20 ms | 112.6 KB |

Mapbox-compatible raster PNG tiles:

| Scenario | Requests | Concurrency | Failures | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR WXA VPD tile z4 | 3,000 | 96 | 0 | 3,589.6 | 19.82 ms | 65.79 ms | 98.79 ms | 86.8 KB |
| HRRR profile 500mb temperature tile z4 | 3,000 | 96 | 0 | 3,601.4 | 23.42 ms | 48.50 ms | 76.82 ms | 43.4 KB |
| HRRR profile 500mb RH tile z4 | 1,000 | 64 | 0 | 3,416.7 | 17.29 ms | 28.00 ms | 41.46 ms | 96.1 KB |

Full-grid binary map-source products:

| Scenario | Requests | Concurrency | Failures | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR WXA VPD grid `f000` | 300 | 16 | 0 | 540.2 | 28.37 ms | 40.05 ms | 46.45 ms | 7.27 MB |
| GFS WXA VPD grid `f000` | 300 | 16 | 0 | 1,046.4 | 14.76 ms | 21.47 ms | 23.76 ms | 3.96 MB |
| HRRR WXA 24h max temp grid `f000` | 300 | 16 | 0 | 528.2 | 29.01 ms | 41.57 ms | 47.19 ms | 7.27 MB |

## WXA Materialization Timing And Storage

| Model | Run | Member | Products | Hours | Product-hour grids | Time |
| --- | --- | --- | --- | --- | ---: | ---: |
| HRRR | `20260405_18z` | control | `dewpoint_depression_2m,vpd_2m,heat_index_2m` | `0-2` | 9 | 0.467 s |
| GFS | `20260405_12z` | control | `dewpoint_depression_2m,vpd_2m,heat_index_2m` | `0-2` | 9 | 0.254 s |
| ECMWF IFS | `20260411_12z` | control | `dewpoint_depression_2m` | `0,3` | 2 | 0.060 s |
| ECMWF ENS | `20260412_00z` | `001` | `dewpoint_depression_2m` | `3` | 1 | 0.029 s |
| HRRR | `20260405_18z` | control | 6 windowed 24h products | `0` | 6 | 0.350 s |
| GFS | `20260405_12z` | control | 6 windowed 24h products | `0` | 6 | 0.187 s |

WXA proof storage:

```text
files:     20
total:     78.5 MiB
HRRR:      45.8 MiB
GFS:       28.5 MiB
ECMWF IFS: 2.8 MiB
ECMWF ENS: 1.4 MiB
```

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

Mapbox layers:
  model/run/variable/frame/z/x/y -> transparent PNG raster tile
```

The major product result is that surface forecast calls across the available local model families are all around `19k req/s` on this workstation after the spatial arrays are warm, and HRRR 48h binary temporal soundings are above `10k req/s` with the current diagnostic lane attached.

`latest-benchmark-results.json` is the machine-readable output from the latest run.

## Current Blocker For Complete Rustwx Product Coverage

WxStore can now serve WXA spatial grids, temporal profile-derived pressure grids, and Mapbox raster layers. The remaining blocker for "all rustwx products except ECAPE" is upstream export plumbing: rustwx has internal `Field2D` producers for direct, derived, and windowed HRRR products, but the current public CLIs render PNG/report artifacts rather than exporting every product as raw f32 grids with manifests. A rustwx-side grid export builder is required to materialize the full catalog into WXA without reimplementing meteorology inside WxStore.
