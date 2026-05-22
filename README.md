# WxStore

WxStore is a local-first storage/API layer for model-agnostic weather data. It
does not depend on Open-Meteo file formats or Open-Meteo code.

This service currently loads:

- HRRR CONUS temporal pressure-profile lane: `TMP, SPFH, UGRD, VGRD, HGT`, 40 pressure levels, `f000-f048`.
- HRRR precomputed diagnostic lane: basic/severe/parcel scalar diagnostics from the existing precomputed diagnostic artifact.
- Multi-model surface/spatial lane: native `.wxa` dense2d product files, with local Zarr-v2 arrays used only as source/proof adapters.
- Optional object/media lanes for observations, mesoanalysis innovation indexes, static plots, evidence bundles, satellite tiles, radar tiles, cross-sections, soundings, and archive events.

WxStore is designed for trusted local or private-network deployments. It has no
built-in authentication layer; see [SECURITY.md](SECURITY.md) before exposing an
instance beyond localhost.

## Build And Test

```powershell
cargo build --release
cargo test --locked
```

The service binary is written to `target\release\wxstore.exe`.

## Data

The repository contains code, documentation, and small benchmark/proof metadata.
It does not include model grids, WXA/WXP stores, radar/satellite tiles, static
plot outputs, or observation mirrors. Generate or mount those artifacts locally
and keep them under ignored directories such as `data/`, `artifacts/`, or
`proof/`.

## Run

Start with whichever local lanes you have. A clone-friendly spatial-only run:

```powershell
.\target\release\wxstore.exe serve `
  --spatial-root C:\path\to\wxstore\data\spatial `
  --host 127.0.0.1 `
  --port 8897
```

A fuller local operations run can add roots as they become available:

```powershell
.\target\release\wxstore.exe serve `
  --profile-store C:\path\to\hrrr_profile_store `
  --diagnostic-store C:\path\to\diagnostic_store `
  --spatial-root C:\path\to\wxstore\data\spatial `
  --static-plots-root C:\path\to\static_plots `
  --evidence-root C:\path\to\evidence `
  --observations-root C:\path\to\observations `
  --mesoanalysis-innovation-index-root C:\path\to\mesoanalysis_index `
  --satellite-tiles-root C:\path\to\satellite_tiles `
  --radar-tiles-root C:\path\to\radar_tiles `
  --archive-root C:\path\to\archive `
  --host 127.0.0.1 `
  --port 8897
```

Optional routes return unavailable/not-configured responses when their backing
root is not provided.

| Flag | Enables |
| --- | --- |
| `--profile-store` | HRRR pressure-profile grids and temporal sounding profile lane |
| `--diagnostic-store` | precomputed HRRR basic/severe/parcel diagnostics |
| `--spatial-root` | WXA/Zarr spatial products, forecast/grid/sample/tile endpoints |
| `--static-plots-root` | static plot catalog, artifacts, and MP4 export |
| `--evidence-root` | evidence bundle catalog and bundle detail |
| `--observations-root` | direct weather object/station/source observation catalog |
| `--mesoanalysis-innovation-index-root` | station/source innovation status, query, and watchlist |
| `--satellite-tiles-root` | satellite viewer, layers, frames, and tiles |
| `--radar-tiles-root` | radar viewer, layers, frames, tiles, sidecars, and sampling |
| `--archive-root` | archive/hrrrarchive event catalogs, polygons, runs, and status |

## API Surface

UI and health:

- `GET /`, `/plots`, `/satellite`, `/radar`, `/tools`, `/meteograms`, `/cross-sections`, `/plot-lab`, `/projection-demo`, `/archive`, `/hrrrarchive`, `/ops`
- `GET /livez`, `/readyz`, `/v1/status`, `/api/status`

Model, grid, forecast, and tile APIs:

- `GET /v1/models`
- `GET /v1/variables?model=hrrr&run=latest`
- `GET /v1/products`
- `GET /v1/layers?model=hrrr&run=latest`
- `GET /v1/latest/{model}/{domain}`
- `GET /v1/resolve?lat=35.22&lon=-97.44`
- `GET /v1/forecast?latitude=35.22&longitude=-97.44&model=hrrr&hourly=temperature_2m,dew_point_2m&forecast_hours=0-2`
- `GET /v1/grid?model=hrrr&variable=temperature_2m&forecast_hour=0&format=bin`
- `GET /v1/sample?model=hrrr&run=latest&variable=temperature_2m&forecast_hour=0&lat=35.22&lon=-97.44`
- `GET /v1/wind-field?model=hrrr&run=latest&forecast_hour=0`
- `GET /v1/tilejson/{model}/{run}/{variable}`
- `GET /v1/tiles/{model}/{run}/{variable}/{forecast_hour}/{z}/{x}/{y}`
- `GET /v1/mapbox/layers/{model}/{run}/{variable}`
- `GET /v1/mapbox/tilejson/{model}/{run}/{variable}/{frame}`
- `GET /v1/mapbox/tiles/{model}/{run}/{variable}/{frame}/{z}/{x}/{y}`

Objects, observations, and mesoanalysis:

- `GET /v1/objects?kind=surface_observation&q=WISCONET:HNCK`
- `GET /v1/objects?kind=surface_observation&bbox=-103,33,-94,37`
- `GET /v1/objects?kind=surface_observation&lat=35.18&lon=-97.44&radius_km=50`
- `GET /v1/objects?kind=marine_observation&category=ocean&network=NDBC&parameter=wave_height`
- `GET /v1/objects?kind=air_quality_observation&category=air_quality&network=AIRNOW&parameter=pm25`
- `GET /v1/observations/sources`
- `GET /v1/observations/sources/{source_id}`
- `GET /v1/mesoanalysis/innovation/status`
- `GET /v1/mesoanalysis/innovation/query?station=KP69&variable=temperature_c`
- `GET /v1/mesoanalysis/innovation/query?kind=source&source=aviation_weather_metar_conus&variable=wind_speed_ms`
- `GET /v1/mesoanalysis/innovation/watchlist?kind=station&top=20`

Media and agent artifact lanes:

- `GET /v1/static-plots`
- `POST /v1/static-plots/export-mp4`
- `GET /v1/static-plots/artifacts/{manifest_id}/{artifact_index}`
- `GET /v1/evidence/bundles`
- `GET /v1/evidence/bundles/{bundle_id}`
- `GET /v1/satellite/layers`
- `GET /v1/satellite/layers/{layer_id}/frames.json`
- `GET /v1/satellite/tiles/{layer_id}/frames/{frame_id}/{z}/{x}/{tile_file}`
- `GET /v1/radar/layers`
- `GET /v1/radar/layers/{layer_id}/frames.json`
- `GET /v1/radar/tiles/{layer_id}/frames/{frame_id}/{z}/{x}/{tile_file}`
- `GET /v1/radar/tiles/{layer_id}/frames/{frame_id}/{tilt_id}/{z}/{x}/{tile_file}`
- `GET /v1/radar/sidecars/{layer_id}/frames/{frame_id}/{sidecar_file}`
- `GET /v1/radar/sidecars/{layer_id}/frames/{frame_id}/{tilt_id}/{sidecar_file}`
- `GET /v1/radar/sample`
- `GET /v1/plot-lab/config`
- `POST /v1/plot-lab/render`
- `GET /v1/plot-lab/artifacts/{render_id}/{file_name}`
- `GET /v1/ops/live`

Cross-section, sounding, and archive APIs:

- `GET /v1/cross-section/status`
- `GET /v1/cross-section/status/products`
- `GET /v1/cross-section/products`
- `POST /v1/cross-section/render`
- `GET /v1/cross-section/artifacts/{render_id}/{file_name}`
- `GET /v1/sounding/status`
- `POST /v1/sounding/render`
- `GET /v1/sounding/artifacts/{render_id}/{file_name}`
- `GET /v1/archive/status`, `/v1/archive/events`, `/v1/archive/events/{event_id}`
- `GET /v1/archive/events/{event_id}/polygons`, `/v1/archive/events/{event_id}/runs`
- `GET /v1/hrrrarchive/status`, `/v1/hrrrarchive/events`, `/v1/hrrrarchive/events/{event_id}`
- `GET /v1/hrrrarchive/events/{event_id}/polygons`, `/v1/hrrrarchive/events/{event_id}/runs`
- `GET /v1/temporal-sounding?lat=35.22&lon=-97.44&hours=0-48&diagnostics=basic`
- `GET /api/point?lat=35.22&lon=-97.44&hours=0-48&diagnostics=basic`
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
This is model-agnostic for any model rustwx can export today. The current
RustWX exporter supports direct, derived, and HRRR windowed grid lanes while
intentionally excluding ECAPE/heavy products from the default WXA export set.

Current RustWX model IDs include:

```text
hrrr
hrrr-ak
gfs
gdas
gefs
aigfs
aigefs
hgefs
ecmwf-open-data
aifs
rap
nam
hiresw
href
sref
rtma
urma
nbm
rrfs-a
rrfs-public
refs
rrfs-firewx
wrf / wrf-gdex
```

Example GFS smoke import:

```powershell
cargo run -p rustwx-cli --bin rustwx_grid_export -- `
  --model gfs `
  --date 20260430 `
  --cycle 12 `
  --forecast-hour 0-2 `
  --source aws `
  --region conus `
  --product 2m_temperature,wind_u_10m_ms,wind_v_10m_ms `
  --out-dir target/artifacts/wxstore_grid_export `
  --cache-dir target/artifacts/wxstore_grid_export_cache

.\target\release\wxstore.exe import-rustwx-grids `
  --manifest C:\path\to\rustwx\target\artifacts\wxstore_grid_export\20260430_gfs_12z\conus_f000_f002\manifest.json `
  --spatial-root C:\path\to\wxstore\data\spatial `
  --publish-latest
```

Imported WXA files preserve geographic metadata from the rustwx lat/lon grid:
regular lat/lon, rectilinear lat/lon, HRRR Lambert crops, or a compact sampled
curvilinear fallback. Existing WXA files need to be re-imported to gain the new
metadata.

## Materialize Native WXA Products

```powershell
.\target\release\wxstore.exe materialize-spatial `
  --spatial-root C:\path\to\wxstore\data\spatial `
  --model hrrr `
  --run 20260405_18z `
  --products dewpoint_depression_2m,vpd_2m,heat_index_2m `
  --hours 0-2 `
  --output-format wxa
```

The `.wxa` files are WxStore-native: JSON metadata, fixed chunk index, zstd-compressed f32 spatial chunks, and no Open-Meteo file/code dependency.

## CLI Commands

- `serve`: start the API/UI server with any configured lane roots.
- `inspect`: print a JSON summary of configured profile, diagnostic, spatial, static plot, evidence, observation, and mesoanalysis lanes.
- `inspect-spatial`: inspect a spatial root, optionally scoped to one model.
- `materialize-spatial`: materialize derived WXA products from an existing spatial root.
- `import-rustwx-grids`: import one or more RustWX grid-export manifests into WXA storage.
- `publish-latest`: update the latest pointer for a model/run under a spatial root.
- `gc-spatial`: dry-run or apply old-run garbage collection for a spatial model.

## Current Benchmark

Example local HTTP benchmark from a Windows workstation, release build, service
on `127.0.0.1:8897`. Treat these as local proof numbers, not hosted service
SLOs.

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
- RustWX catalog materialization now comes through `rustwx_grid_export` for supported direct, derived, and HRRR windowed products. ECAPE/heavy products remain intentionally excluded from the default f32 grid export set.
- `latest-benchmark-results.json` contains the latest machine-readable benchmark table.

## License

WxStore is licensed under the MIT License. See [LICENSE](LICENSE).
