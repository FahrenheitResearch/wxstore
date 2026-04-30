# WxStore

WxStore is the private storage/API layer for a model-agnostic weather API. It does not depend on Open-Meteo file formats or Open-Meteo code.

This service currently loads:

- HRRR CONUS temporal pressure-profile lane: `TMP, SPFH, UGRD, VGRD, HGT`, 40 pressure levels, `f000-f048`.
- HRRR precomputed diagnostic lane: basic/severe/parcel scalar diagnostics from the existing precomputed diagnostic artifact.
- Multi-model surface/spatial lane: native `.wxa` dense2d product files, with the existing local Zarr-v2 arrays used only as source/proof adapters.

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
- `GET /v1/products`
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

## Materialize Native WXA Products

```powershell
C:\Users\drew\wxstore\target\release\wxstore.exe materialize-spatial `
  --spatial-root C:\Users\drew\open-rust-wx\data\spatial `
  --model hrrr `
  --run 20260405_18z `
  --products dewpoint_depression_2m,vpd_2m,heat_index_2m `
  --hours 0-2 `
  --output-format wxa
```

The `.wxa` files are WxStore-native: JSON metadata, fixed chunk index, zstd-compressed f32 spatial chunks, and no Open-Meteo file/code dependency.

## Current Benchmark

Sequential raw HTTP benchmark on this local Windows workstation, release build, service on `127.0.0.1:8897`.

| Scenario | Requests | Concurrency | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR WXA derived forecast, 3 vars x 3h | 30,000 | 192 | 19,485.6 | 4.76 ms | 5.41 ms | 8.96 ms | 1.07 KB |
| GFS WXA derived forecast, 3 vars x 3h | 30,000 | 192 | 19,469.0 | 4.65 ms | 5.32 ms | 24.84 ms | 1.05 KB |
| ECMWF IFS WXA dewpoint depression, 2h | 30,000 | 192 | 19,293.4 | 4.61 ms | 5.47 ms | 25.35 ms | 0.83 KB |
| ECMWF ENS member 001 WXA dewpoint depression, 1h | 30,000 | 192 | 19,405.3 | 4.65 ms | 5.44 ms | 16.76 ms | 0.80 KB |
| HRRR WXA 24h windowed forecast, 3 vars x 1h | 30,000 | 192 | 19,508.5 | 4.83 ms | 5.70 ms | 26.51 ms | 0.94 KB |
| HRRR 48h temporal sounding binary + basic diagnostics | 30,000 | 192 | 11,313.2 | 14.91 ms | 27.07 ms | 34.87 ms | 30.3 KB |
| HRRR 48h temporal sounding compact JSON + basic diagnostics | 10,000 | 128 | 4,433.0 | 27.85 ms | 40.74 ms | 47.20 ms | 112.6 KB |

Full-grid binary map-source extraction:

| Scenario | Requests | Concurrency | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR WXA VPD grid `f000` | 300 | 16 | 540.2 | 28.37 ms | 40.05 ms | 46.45 ms | 7.27 MB |
| GFS WXA VPD grid `f000` | 300 | 16 | 1,046.4 | 14.76 ms | 21.47 ms | 23.76 ms | 3.96 MB |
| HRRR WXA 24h max temp grid `f000` | 300 | 16 | 528.2 | 29.01 ms | 41.57 ms | 47.19 ms | 7.27 MB |

## Storage Notes

- Temporal sounding uses the custom point-temporal `.wxp` lane with `chunk_x=8`, `chunk_y=1`, all 49 hours and 40 pressure levels per chunk.
- Surface forecast and grid endpoints prefer native `.wxa` products and fall back to the local Zarr source adapter only when a WXA product is not present.
- Current WXA proof storage: 20 files, 78.5 MiB total. HRRR proof products are 45.8 MiB; GFS proof products are 28.5 MiB.
- `latest-benchmark-results.json` contains the latest machine-readable benchmark table.
