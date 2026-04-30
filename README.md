# WxStore

WxStore is the private storage/API layer for a model-agnostic weather API. It does not depend on Open-Meteo file formats or Open-Meteo code.

This service currently loads:

- HRRR CONUS temporal pressure-profile lane: `TMP, SPFH, UGRD, VGRD, HGT`, 40 pressure levels, `f000-f048`.
- HRRR precomputed diagnostic lane: basic/severe/parcel scalar diagnostics from the existing precomputed diagnostic artifact.
- Multi-model surface/spatial lane: point forecasts and full-grid map sources for the local `hrrr`, `gfs`, `ecmwf_ifs`, and `ecmwf_ens` model-run arrays.

## Run

```powershell
C:\Users\drew\wxstore\target\release\wxstore.exe serve `
  --profile-store C:\Users\drew\orwx-wx-profile-nvme\hrrr_20260429_06z_f000_f048_core_chunk8 `
  --diagnostic-store C:\Users\drew\rustwx\proof\aether_temporal_profile_mvp\diagnostic_conus_20260429_06z_f000_f048 `
  --spatial-root C:\Users\drew\open-rust-wx\data\spatial `
  --host 127.0.0.1 `
  --port 8897
```

## Endpoints

- `GET /v1/status`
- `GET /v1/models`
- `GET /v1/variables?model=hrrr&run=latest`
- `GET /v1/forecast?latitude=35.22&longitude=-97.44&model=hrrr&hourly=temperature_2m,dew_point_2m&forecast_hours=0-2`
- `GET /v1/grid?model=hrrr&variable=temperature_2m&forecast_hour=0&format=bin`
- `GET /v1/latest/{model}/{domain}`
- `GET /v1/resolve?lat=35.22&lon=-97.44`
- `GET /v1/temporal-sounding?lat=35.22&lon=-97.44&hours=0-48&diagnostics=basic`
- `GET /v1/point.bin?lat=35.22&lon=-97.44&hours=0-48&diagnostics=basic`
- `GET /v1/runs/{model}/{domain}/{run}/grid/{x}/{y}/temporal-sounding?hours=0-48`
- `GET /v1/runs/{model}/{domain}/{run}/grid/{x}/{y}/temporal-sounding.bin?hours=0-48`

## Loaded Model Runs

| Model | Run | Product coverage used in local proof |
| --- | --- | --- |
| `hrrr` | `20260405_18z` | surface/spatial fields, many hourly leads |
| `gfs` | `20260405_12z` | surface/spatial fields, many hourly leads |
| `ecmwf_ifs` | `20260411_12z` | local sparse lead coverage: `f000`, `f003` |
| `ecmwf_ens` | `20260412_00z`, member `001` | local sparse lead coverage: `f003` |
| `hrrr` | `20260429T06Z` | temporal pressure profile + diagnostics, `f000-f048` |

## Current Benchmark

Sequential raw HTTP benchmark on this local Windows workstation, release build, service on `127.0.0.1:8897`.

| Scenario | Requests | Concurrency | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR surface forecast, 3 vars x 3h | 30,000 | 192 | 19,317.7 | 4.69 ms | 5.40 ms | 5.89 ms | 1.06 KB |
| GFS surface forecast, 3 vars x 3h | 30,000 | 192 | 19,634.2 | 4.80 ms | 5.81 ms | 6.22 ms | 1.04 KB |
| ECMWF IFS surface forecast, 3 vars x 2h | 30,000 | 192 | 19,334.7 | 4.72 ms | 5.69 ms | 6.18 ms | 0.96 KB |
| ECMWF ENS member 001 surface forecast, 3 vars x 1h | 30,000 | 192 | 19,284.5 | 4.30 ms | 5.10 ms | 5.93 ms | 0.90 KB |
| HRRR 48h temporal sounding binary + basic diagnostics | 30,000 | 192 | 10,925.0 | 15.51 ms | 27.10 ms | 37.94 ms | 30.3 KB |
| HRRR 48h temporal sounding compact JSON + basic diagnostics | 10,000 | 96 | 3,852.4 | 24.60 ms | 32.76 ms | 36.25 ms | 112.6 KB |

Full-grid binary map-source extraction:

| Scenario | Requests | Concurrency | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR CONUS temperature grid `f000` | 300 | 16 | 535.1 | 29.39 ms | 40.67 ms | 47.47 ms | 7.27 MB |
| GFS global temperature grid `f000` | 300 | 16 | 998.8 | 15.40 ms | 22.91 ms | 27.08 ms | 3.96 MB |
| ECMWF IFS global temperature grid `f000` | 300 | 16 | 952.0 | 15.06 ms | 22.96 ms | 27.88 ms | 3.96 MB |
| ECMWF ENS member 001 global temperature grid `f003` | 300 | 16 | 995.6 | 14.98 ms | 24.91 ms | 30.89 ms | 3.96 MB |

## Storage Notes

- Temporal sounding uses the custom point-temporal `.wxp` lane with `chunk_x=8`, `chunk_y=1`, all 49 hours and 40 pressure levels per chunk.
- Surface forecast and grid endpoints use a read-only spatial lane adapter over the existing local model-run arrays and normalize temperature/pressure units at the lane boundary.
- The API shape is WxStore-native. Native WXA spatial/diagnostic containers can replace the adapter without changing public endpoint contracts.
- `latest-benchmark-results.json` contains the latest machine-readable benchmark table.
