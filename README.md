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
  --mesoanalysis-innovation-index-root C:\Users\drew\rustwx\target\surface_mesoanalysis_calibration\innovation_wxstore_index_smoke `
  --host 127.0.0.1 `
  --port 8897
```

## Endpoints

- `GET /v1/status`
- `GET /v1/models`
- `GET /v1/variables?model=hrrr&run=latest`
- `GET /v1/products`
- `GET /v1/layers?model=hrrr&run=latest`
- `GET /v1/forecast?latitude=35.22&longitude=-97.44&model=hrrr&hourly=temperature_2m,dew_point_2m&forecast_hours=0-2`
- `GET /v1/grid?model=hrrr&variable=temperature_2m&forecast_hour=0&format=bin`
- `GET /v1/mapbox/layers/hrrr/20260405_18z/vpd_2m?hours=0-2&palette=magma&range=0,5`
- `GET /v1/mapbox/tilejson/hrrr/20260405_18z/vpd_2m/f000?palette=magma&range=0,5`
- `GET /v1/mapbox/tiles/hrrr/20260405_18z/vpd_2m/f000/{z}/{x}/{y}?palette=magma&range=0,5`
- `GET /v1/mapbox/tiles/hrrr/hrrr_20260429_060000/500mb_temperature/f000/{z}/{x}/{y}?palette=temperature&range=-40,20`
- `GET /v1/latest/{model}/{domain}`
- `GET /v1/objects?kind=surface_observation&q=WISCONET:HNCK`
- `GET /v1/objects?kind=surface_observation&q=AZMET:AZ01`
- `GET /v1/objects?kind=surface_observation&bbox=-103,33,-94,37`
- `GET /v1/objects?kind=surface_observation&lat=35.18&lon=-97.44&radius_km=50`
- `GET /v1/objects?kind=coastal_meteorology_observation&category=ocean&network=CO-OPS_MET&parameter=wind&max_age_minutes=90`
- `GET /v1/objects?kind=marine_observation&category=ocean&network=NDBC&parameter=wave_height`
- `GET /v1/objects?kind=air_quality_observation&category=air_quality&network=AIRNOW&parameter=pm25`
- `GET /v1/objects?category=ocean&quality_tier=1&parameter=wind`
- `GET /v1/objects?kind=surface_observation&quality_tier=2&parameter=wind`
- `GET /v1/objects?kind=flash_flood_observation&quality_tier=3&parameter=precipitation`
- `GET /v1/objects?kind=marine_observation&q=NDBC:46029`
- `GET /v1/objects?kind=hydro_observation&q=07148400`
- `GET /v1/objects?kind=hydro_observation&source=noaa_ahps_river_gauges&q=AHPS:DCBN8`
- `GET /v1/objects?kind=hydro_forecast_observation&source=noaa_nwps_river_forecasts&q=NWPS:ABBG1`
- `GET /v1/objects?kind=flash_flood_observation&source=maricopa_fcd_alert&q=Humboldt`
- `GET /v1/objects?kind=coastal_water_observation&q=9414290`
- `GET /v1/objects?kind=coastal_meteorology_observation&source=noaa_coops_meteorology&q=Nawiliwili`
- `GET /v1/objects?kind=air_quality_observation&q=AIRNOW:010270001`
- `GET /v1/observations/sources`
- `GET /v1/mesoanalysis/innovation/status`
- `GET /v1/mesoanalysis/innovation/query?station=KP69&variable=temperature_c`
- `GET /v1/mesoanalysis/innovation/query?kind=source&source=aviation_weather_metar_conus&variable=wind_speed_ms`
- `GET /v1/mesoanalysis/innovation/watchlist?kind=station&top=20`
- `GET /v1/resolve?lat=35.22&lon=-97.44`
- `GET /v1/temporal-sounding?lat=35.22&lon=-97.44&hours=0-48&diagnostics=basic`
- `GET /v1/point.bin?lat=35.22&lon=-97.44&hours=0-48&diagnostics=basic`
- `GET /v1/runs/{model}/{domain}/{run}/grid/{x}/{y}/temporal-sounding?hours=0-48`
- `GET /v1/runs/{model}/{domain}/{run}/grid/{x}/{y}/temporal-sounding.bin?hours=0-48`

Direct-observation station and source objects returned by `/v1/objects` include
a `parameters` array such as `wind`, `wave_height`, `pm25`,
`water_temperature`, or `streamflow`, using the same strict taxonomy as the
`parameter=` filter. Source objects also include `station_count` so agents can
discover capable lanes before paging through station objects.
They also include `quality_tier`: 1 primary official/national operational,
2 established mesonet/agency, 3 local/utility/infrastructure specialty,
4 derived/daily secondary, 5 unknown.
Direct-observation weather objects are materialized through an in-process cache
keyed by the runner observation index and per-source latest artifact
fingerprints, so repeated object queries avoid reparsing all station files while
still refreshing when runner publishes new observation artifacts.

The mesoanalysis innovation lane serves the RustWX OI/kriging calibration index
as machine-readable WxStore JSON. It is intentionally model-agnostic: HRRR, RAP,
RRFS, GFS, and other model backgrounds remain providers, while the lane exposes
station/source innovation history, ranked watchlists, and source reliability
signals for agent packet builders.

## Loaded Model Runs

| Model | Run | Product coverage used in local proof |
| --- | --- | --- |
| `hrrr` | `20260405_18z` | surface/spatial fields, many hourly leads |
| `gfs` | `20260405_12z` | surface/spatial fields, many hourly leads |
| `ecmwf_ifs` | `20260411_12z` | local sparse lead coverage: `f000`, `f003` |
| `ecmwf_ens` | `20260412_00z`, member `001` | local sparse lead coverage: `f003` |
| `hrrr` | `20260429T06Z` | temporal pressure profile + diagnostics, `f000-f048` |

## Import Rustwx Grid Exports

The production bridge is `rustwx -> f32 grid export -> WxStore WXA import`.
This is model-agnostic for any model rustwx can export today. Current rustwx
model IDs are:

```text
hrrr
gfs
rrfs / rrfs-a
ecmwf-open-data
wrf-gdex
```

Example GFS smoke import:

```powershell
cargo run -p rustwx-cli --bin rustwx_grid_export -- `
  --model gfs `
  --date 20260430 `
  --cycle 12 `
  --forecast-hour 0 `
  --source aws `
  --region conus `
  --bounds=-125.0,-66.0,24.0,50.0 `
  --product 2m_temperature,wind_u_10m_ms,wind_v_10m_ms `
  --out-dir C:\Users\drew\rustwx\proof\wxstore_multimodel_gfs_smoke `
  --cache-dir C:\Users\drew\rustwx\proof\wxstore_grid_export_cache

C:\Users\drew\wxstore\target\release\wxstore.exe import-rustwx-grids `
  --manifest C:\Users\drew\rustwx\proof\wxstore_multimodel_gfs_smoke\20260430_gfs_12z\conus_f000\manifest.json `
  --spatial-root C:\Users\drew\wxstore\data\rustwx_layers_all
```

Imported WXA files preserve geographic metadata from the rustwx lat/lon grid:
regular lat/lon, rectilinear lat/lon, HRRR Lambert crops, or a compact sampled
curvilinear fallback. Existing WXA files need to be re-imported to gain the new
metadata.

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

Local smoke and lightweight benchmark tooling is documented in
[`docs/LOCAL_VALIDATION.md`](docs/LOCAL_VALIDATION.md). A release/data-refresh
gate checklist is in
[`docs/PRODUCTION_READINESS_CHECKLIST.md`](docs/PRODUCTION_READINESS_CHECKLIST.md).

| Scenario | Requests | Concurrency | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR WXA derived forecast, 3 vars x 3h | 30,000 | 192 | 19,485.6 | 4.76 ms | 5.41 ms | 8.96 ms | 1.07 KB |
| GFS WXA derived forecast, 3 vars x 3h | 30,000 | 192 | 19,469.0 | 4.65 ms | 5.32 ms | 24.84 ms | 1.05 KB |
| ECMWF IFS WXA dewpoint depression, 2h | 30,000 | 192 | 19,293.4 | 4.61 ms | 5.47 ms | 25.35 ms | 0.83 KB |
| ECMWF ENS member 001 WXA dewpoint depression, 1h | 30,000 | 192 | 19,405.3 | 4.65 ms | 5.44 ms | 16.76 ms | 0.80 KB |
| HRRR WXA 24h windowed forecast, 3 vars x 1h | 30,000 | 192 | 19,508.5 | 4.83 ms | 5.70 ms | 26.51 ms | 0.94 KB |
| HRRR 48h temporal sounding binary + basic diagnostics | 30,000 | 192 | 11,313.2 | 14.91 ms | 27.07 ms | 34.87 ms | 30.3 KB |
| HRRR 48h temporal sounding compact JSON + basic diagnostics | 10,000 | 128 | 4,433.0 | 27.85 ms | 40.74 ms | 47.20 ms | 112.6 KB |

Mapbox-compatible raster PNG tiles:

| Scenario | Requests | Concurrency | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR WXA VPD tile z4 | 3,000 | 96 | 3,589.6 | 19.82 ms | 65.79 ms | 98.79 ms | 86.8 KB |
| HRRR profile 500mb temperature tile z4 | 3,000 | 96 | 3,601.4 | 23.42 ms | 48.50 ms | 76.82 ms | 43.4 KB |
| HRRR profile 500mb RH tile z4 | 1,000 | 64 | 3,416.7 | 17.29 ms | 28.00 ms | 41.46 ms | 96.1 KB |

Full-grid binary map-source extraction:

| Scenario | Requests | Concurrency | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| HRRR WXA VPD grid `f000` | 300 | 16 | 540.2 | 28.37 ms | 40.05 ms | 46.45 ms | 7.27 MB |
| GFS WXA VPD grid `f000` | 300 | 16 | 1,046.4 | 14.76 ms | 21.47 ms | 23.76 ms | 3.96 MB |
| HRRR WXA 24h max temp grid `f000` | 300 | 16 | 528.2 | 29.01 ms | 41.57 ms | 47.19 ms | 7.27 MB |

## Storage Notes

- Temporal sounding uses the custom point-temporal `.wxp` lane with `chunk_x=8`, `chunk_y=1`, all 49 hours and 40 pressure levels per chunk.
- Surface forecast and grid endpoints prefer native `.wxa` products and fall back to the local Zarr source adapter only when a WXA product is not present.
- Mapbox endpoints render transparent PNG XYZ tiles from any grid product that `read_grid` can return, including WXA spatial fields and HRRR pressure-profile grids.
- HRRR profile grids currently expose pressure-level temperature, height, wind speed, RH, dewpoint, and specific humidity at the standard pressure levels available in the profile lane.
- Current WXA proof storage: 20 files, 78.5 MiB total. HRRR proof products are 45.8 MiB; GFS proof products are 28.5 MiB.
- Full rustwx catalog materialization is blocked on a rustwx-side raw `Field2D` export builder. Current rustwx CLIs render PNG/report artifacts but do not export every direct/derived/windowed product as f32 grids for WxStore.
- `latest-benchmark-results.json` contains the latest machine-readable benchmark table.
