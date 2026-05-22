# WxStore Benchmark Notes

Date: 2026-04-29 local proof run

These numbers came from a local Windows workstation running a release build on
`127.0.0.1:8897`. They are useful for regression comparison, but they are not a
hosted-service SLO.

## Service Shape

The benchmarked build served:

- native WXA dense2d spatial products;
- read-only Zarr-v2 spatial source fallback where WXA was not materialized;
- HRRR point-temporal `.wxp` pressure-profile stores;
- precomputed diagnostic stores;
- Mapbox-compatible raster tile endpoints;
- point forecast, sample, grid, and temporal sounding APIs.

Example local command shape:

```powershell
.\target\release\wxstore.exe serve `
  --profile-store C:\path\to\hrrr_profile_store `
  --diagnostic-store C:\path\to\diagnostic_store `
  --spatial-root C:\path\to\wxstore\data\spatial `
  --host 127.0.0.1 `
  --port 8897
```

## Loaded Proof Data

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

Native WXA product files were materialized from the local lanes available during
the proof run. Sparse ECMWF coverage reflects local disk availability, not an
API limitation.

## Smoke Tests

Representative routes returned 200:

```text
/v1/status
/v1/models
/v1/variables?model=hrrr&run=20260405_18z
/v1/forecast?latitude=35.22&longitude=-97.44&model=hrrr&run=20260405_18z&hourly=temperature_2m,dew_point_2m,wind_gusts_10m&forecast_hours=0-2
/v1/grid?model=hrrr&run=20260405_18z&variable=temperature_2m&forecast_hour=0&format=bin
/v1/mapbox/layers/hrrr/20260405_18z/vpd_2m?hours=0-2&palette=magma&range=0,5
/v1/mapbox/tiles/hrrr/20260405_18z/vpd_2m/f000/4/3/6.png?palette=magma&range=0,5
/v1/point.bin?lat=35.22&lon=-97.44&hours=0-48&diagnostics=basic
/v1/temporal-sounding?lat=35.22&lon=-97.44&hours=0-2&diagnostics=basic
```

## Sequential HTTP Benchmarks

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

`latest-benchmark-results.json` is the machine-readable output from the latest
run kept in this repository.

## WXA Materialization

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

## Storage Math

HRRR CONUS:

```text
grid cells: 1799 * 1059 = 1,905,141
one raw f32 grid: 7.27 MiB
49 forecast hours, one field raw f32: 356 MiB
75 fields, 49h raw f32: ~26.1 GiB
75 fields, 49h raw i16: ~13.0 GiB before compression
```

This is only for scalar 2D map/grid fields. Temporal profile lanes are separate:

```text
current HRRR 5-var profile core .wxp: 16.75 GiB
HRRR source VolumeStore has 11 pressure vars available
all-11 pressure profile .wxp estimate: ~35-40 GiB
native flat intermediate estimate: ~150 GiB, so it should not be retained
```
