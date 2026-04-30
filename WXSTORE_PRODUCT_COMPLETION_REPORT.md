# WxStore Product Completion Report

Date: 2026-04-30

## Rustwx Map Layer Bridge

This pass added the direct rustwx -> WxStore map layer path:

- `rustwx_grid_export` exports raw rustwx map products as f32 grids plus lat/lon grids, exact crop coordinates, bounds, provenance fetch identities, and blockers.
- `wxstore import-rustwx-grids` imports those grids into native `.wxa` dense2d spatial arrays.
- WxStore serves those `.wxa` grids as Mapbox-compatible temporal raster layers through `/v1/mapbox/layers`, `/v1/mapbox/tilejson`, and `/v1/mapbox/tiles`.
- Final PNG tile bytes are cached in-process by immutable tile key, so repeated Mapbox/CDN traffic serves pre-encoded bytes.

Proof command:

```powershell
cargo run -p rustwx-cli --bin rustwx_grid_export -- `
  --model hrrr `
  --date 20260429 `
  --cycle 6 `
  --forecast-hour 1 `
  --source aws `
  --region southwest `
  --all-supported `
  --out-dir C:\Users\drew\rustwx\proof\wxstore_grid_export_all `
  --cache-dir C:\Users\drew\rustwx\proof\wxstore_grid_export_cache
```

Result:

| Step | Result |
| --- | ---: |
| Exported products | 84 one-hour SW CONUS layers |
| Expected blockers | 45, mostly f024/f048 window products requested at f001 plus composite direct products |
| rustwx export elapsed | 322.159 s |
| Downloaded source ranges | 95 chunks / 223.49 MB |
| Raw f32 export directory | 97.37 MB |
| WxStore WXA import elapsed | 3.118 s |
| Native WXA spatial root size after f001 all-layer import | 30.36 MB |

The `--all-supported` sweep excludes heavy ECAPE unless `--include-heavy` is passed. With `--include-heavy`, the exporter also includes the ECAPE family slugs (`sbecape`, `mlecape`, `muecape`, ECAPE/CAPE ratios, `sbncape`, `sbecin`, `mlecin`, `ecape_scp`, `ecape_ehi_0_1km`, `ecape_ehi_0_3km`, and `ecape_stp`). ECAPE is intentionally opt-in because it is much more expensive than ordinary direct/derived/windowed maps.

Layer proof:

```text
GET /v1/variables?model=hrrr&run=20260429_hrrr_06z
  -> 84 variables

GET /v1/mapbox/layers/hrrr/20260429_hrrr_06z/stp_fixed?hours=1&palette=severe&range=0,5
  -> bounds [-128.9484, 28.1077, -106.5824, 44.1221]

GET /v1/mapbox/tiles/hrrr/20260429_hrrr_06z/stp_fixed/f001/4/2/6.png?palette=severe&range=0,5
  -> 200 image/png, nonblank
```

Merge-on-import was also verified with f002 for `2m_temperature`, `vpd_2m`, and `qpf_1h`. After importing the second hour, `vpd_2m` reports available hours `[1, 2]`, the Mapbox layer endpoint returns frames `f001` and `f002`, and the spatial root is 31.62 MB.

Local release-server benchmark on `127.0.0.1:8899`:

| Scenario | Requests | Concurrency | Failures | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Cached VPD Mapbox PNG tile, z5 | 30,000 | 192 | 0 | 19,364.4 | 4.85 ms | 5.56 ms | 6.19 ms | 115.6 KB |
| Cached STP Mapbox PNG tile, z4 | 30,000 | 192 | 0 | 19,362.1 | 4.31 ms | 5.02 ms | 5.54 ms | 3.8 KB |
| Mapbox layer metadata | 10,000 | 128 | 0 | 8,898.2 | 14.15 ms | 21.39 ms | 25.23 ms | 838 B |

Before adding final PNG byte caching, the same dynamic z4 tile render path was about 1,711.6 req/s for a 49.9 KB tile. The cached path is the correct production/Mapbox/CDN path for immutable run URLs.

### Latest Multi-Frame Load Test

For freshness/comparison testing, a newer HRRR cycle was added:

```text
Run: 20260430_hrrr_15z
Products: 2m_temperature, vpd_2m, composite_reflectivity
Frames: f000, f001, f002, f003
```

Export timings from rustwx:

| Forecast hour | Wall time | rustwx internal | Products | Blockers |
| --- | ---: | ---: | ---: | ---: |
| f000 | 20.807 s | 20.751 s | 3 | 0 |
| f001 | 12.811 s | 12.750 s | 3 | 0 |
| f002 | 17.679 s | 17.630 s | 3 | 0 |
| f003 | 20.424 s | 20.375 s | 3 | 0 |

Total export wall time was about 71.7 s for 12 product-hour grids. The raw f32 export bundle for the four frames was 21.62 MB.

WxStore import timings into WXA:

| Forecast hour | Wall time | import internal | Products |
| --- | ---: | ---: | ---: |
| f000 | 5.356 s | 115 ms | 3 |
| f001 | 0.341 s | 200 ms | 3 |
| f002 | 0.422 s | 281 ms | 3 |
| f003 | 0.505 s | 369 ms | 3 |

The first import paid Cargo startup/build overhead; the internal importer stayed under 0.4 s while merging frames. The resulting WXA run directory is 5.30 MB.

First-load tile render after clearing the in-process tile cache:

| Tile | First load | Payload |
| --- | ---: | ---: |
| `vpd_2m f000 z5/5/12` | 74.3 ms | 90.2 KB |
| `vpd_2m f001 z5/5/12` | 44.6 ms | 58.5 KB |
| `vpd_2m f002 z5/5/12` | 41.4 ms | 46.6 KB |
| `vpd_2m f003 z5/5/12` | 45.1 ms | 43.2 KB |

Clean endpoint benchmarks:

| Scenario | Requests | Concurrency | Failures | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Cached `vpd_2m f003` PNG tile | 30,000 | 192 | 0 | 19,378.2 | 4.34 ms | 5.07 ms | 5.87 ms | 43.6 KB |
| Mapbox layer metadata, 4 frames | 10,000 | 128 | 0 | 2,389.8 | 51.75 ms | 88.29 ms | 101.91 ms | 1.63 KB |
| Variables metadata, latest run | 10,000 | 128 | 0 | 1,764.3 | 71.28 ms | 114.23 ms | 142.89 ms | 550 B |

The local viewer now exposes both runs and has a compare mode:

```text
http://127.0.0.1:8899/
```

## Current Product Inventory

Rustwx HRRR capability inventory was generated with:

```powershell
cargo run --release -p rustwx-cli --bin hrrr_capability_inventory -- `
  --date 20260429 `
  --forecast-hour 0 `
  --out-dir C:\Users\drew\wxstore\rustwx-inventory
```

Inventory result:

| Category | Available | Blocked |
| --- | ---: | ---: |
| Direct maps | 52 | 1 |
| Derived maps | 44 | 3 |
| Heavy map sets | 1 | 0 |
| Windowed maps | 49 | 0 |
| Pressure cross sections | 19 wired | 1 declared but not pressure-builder ready |

So rustwx currently declares `146` available HRRR map/product entries across direct, derived, heavy, and windowed lanes. Some entries are composites, for example fill + height contours + wind vectors. WxStore should store the scalar fields behind those composites and expose product manifests that describe the composition.

Inventory artifacts:

```text
C:\Users\drew\wxstore\rustwx-inventory\rustwx_hrrr_20260429_f000_capability_inventory.json
C:\Users\drew\wxstore\rustwx-inventory\rustwx_hrrr_20260429_f000_capability_inventory.md
```

## Implemented In WxStore

New service capabilities:

- `/v1/products` exposes service products plus the rustwx HRRR product inventory.
- `/v1/layers` exposes raster layer metadata for WXA spatial products plus HRRR pressure-profile map layers.
- `/v1/mapbox/layers/...`, `/v1/mapbox/tilejson/...`, and `/v1/mapbox/tiles/...` provide Mapbox-compatible temporal raster layer metadata and PNG XYZ tiles.
- `/v1/grid` now accepts raw variables, rustwx-style direct aliases, stored derived products, cheap virtual derived products, and windowed product patterns.
- `/v1/grid` also serves HRRR pressure-profile map grids from the temporal profile lane for products like `500mb_temperature`, `500mb_height`, `500mb_wind_speed`, `500mb_rh`, `500mb_dewpoint`, and `500mb_specific_humidity`.
- `materialize-spatial` command computes product grids and writes native `.wxa` dense2d spatial arrays by default.
- The same command can still write Zarr-v2 with `--output-format zarr`, but Zarr is now only a source/proof adapter.
- Stored product arrays now take priority over virtual compute paths.
- Product existence is cached so point-forecast routing does not hit the filesystem on every request.

Materializer command shape:

```powershell
C:\Users\drew\wxstore\target\release\wxstore.exe materialize-spatial `
  --spatial-root C:\Users\drew\open-rust-wx\data\spatial `
  --model hrrr `
  --run 20260405_18z `
  --products dewpoint_depression_2m,vpd_2m,heat_index_2m `
  --hours 0-2 `
  --output-format wxa
```

## Local Materialization Timing

Products materialized from currently available local fields:

| Model | Run | Member | Products | Hours | Product-hour grids | Time |
| --- | --- | --- | --- | --- | ---: | ---: |
| HRRR | `20260405_18z` | control | `dewpoint_depression_2m,vpd_2m,heat_index_2m` | `0-2` | 9 | 0.467 s |
| GFS | `20260405_12z` | control | `dewpoint_depression_2m,vpd_2m,heat_index_2m` | `0-2` | 9 | 0.254 s |
| ECMWF IFS | `20260411_12z` | control | `dewpoint_depression_2m` | `0,3` | 2 | 0.060 s |
| ECMWF ENS | `20260412_00z` | `001` | `dewpoint_depression_2m` | `3` | 1 | 0.029 s |
| HRRR | `20260405_18z` | control | 6 windowed 24h products | `0` | 6 | 0.350 s |
| GFS | `20260405_12z` | control | 6 windowed 24h products | `0` | 6 | 0.187 s |

Total WXA proof: 33 product-hour grids in 1.347 s.

## Materialized Storage

| Product directory | Size |
| --- | ---: |
| `hrrr/20260405_18z/dewpoint_depression_2m.wxa` | 6.30 MiB |
| `hrrr/20260405_18z/vpd_2m.wxa` | 17.36 MiB |
| `hrrr/20260405_18z/heat_index_2m.wxa` | 6.00 MiB |
| `hrrr/20260405_18z/6 windowed .wxa files` | 16.18 MiB |
| `gfs/20260405_12z/dewpoint_depression_2m.wxa` | 3.73 MiB |
| `gfs/20260405_12z/vpd_2m.wxa` | 9.74 MiB |
| `gfs/20260405_12z/heat_index_2m.wxa` | 4.41 MiB |
| `gfs/20260405_12z/6 windowed .wxa files` | 10.65 MiB |
| `ecmwf_ifs/20260411_12z/dewpoint_depression_2m.wxa` | 2.76 MiB |
| `ecmwf_ens/20260412_00z/member 001/dewpoint_depression_2m.wxa` | 1.39 MiB |

Current WXA proof storage:

```text
files:     20
total:     78.5 MiB
HRRR:      45.8 MiB
GFS:       28.5 MiB
ECMWF IFS: 2.8 MiB
ECMWF ENS: 1.4 MiB
```

## Serving Benchmarks

Sequential local raw HTTP, release service on `127.0.0.1:8897`.

| Product | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: |
| HRRR WXA derived forecast, 3 vars x 3h | 19,485.6 | 4.76 ms | 5.41 ms | 8.96 ms | 1.07 KiB |
| GFS WXA derived forecast, 3 vars x 3h | 19,469.0 | 4.65 ms | 5.32 ms | 24.84 ms | 1.05 KiB |
| ECMWF IFS WXA dewpoint depression, 2h | 19,293.4 | 4.61 ms | 5.47 ms | 25.35 ms | 0.83 KiB |
| ECMWF ENS member 001 WXA dewpoint depression, 1h | 19,405.3 | 4.65 ms | 5.44 ms | 16.76 ms | 0.80 KiB |
| HRRR WXA 24h windowed forecast, 3 vars x 1h | 19,508.5 | 4.83 ms | 5.70 ms | 26.51 ms | 0.94 KiB |
| HRRR WXA VPD grid binary, `f000` | 540.2 | 28.37 ms | 40.05 ms | 46.45 ms | 7.27 MiB |
| GFS WXA VPD grid binary, `f000` | 1,046.4 | 14.76 ms | 21.47 ms | 23.76 ms | 3.96 MiB |
| HRRR WXA 24h max temp grid binary, `f000` | 528.2 | 29.01 ms | 41.57 ms | 47.19 ms | 7.27 MiB |
| HRRR WXA VPD Mapbox PNG tile z4 | 3,589.6 | 19.82 ms | 65.79 ms | 98.79 ms | 86.8 KiB |
| HRRR profile 500mb temp Mapbox PNG tile z4 | 3,601.4 | 23.42 ms | 48.50 ms | 76.82 ms | 43.4 KiB |
| HRRR profile 500mb RH Mapbox PNG tile z4 | 3,416.7 | 17.29 ms | 28.00 ms | 41.46 ms | 96.1 KiB |
| HRRR 48h temporal sounding binary + diagnostics | 11,313.2 | 14.91 ms | 27.07 ms | 34.87 ms | 30.3 KiB |
| HRRR 48h temporal sounding compact JSON + diagnostics | 4,433.0 | 27.85 ms | 40.74 ms | 47.20 ms | 112.6 KiB |

## Storage Math For Full HRRR Map Product Lanes

HRRR CONUS:

```text
grid cells: 1799 * 1059 = 1,905,141
one raw f32 grid: 7.27 MiB
49 forecast hours, one field raw f32: 356 MiB
75 fields, 49h raw f32: ~26.1 GiB
75 fields, 49h raw i16: ~13.0 GiB before compression
```

Empirical current WXA proof products:

```text
HRRR 3 derived products over 3 hours: 29.7 MiB
HRRR 6 windowed single-grid products: 16.2 MiB
75 hourly fields over 49h raw f32: ~26.1 GiB
75 hourly fields over 49h raw i16: ~13.0 GiB before compression
safer production range for 75 HRRR WXA 2D fields: ~5-8 GiB/run
```

This is only for scalar 2D map/grid fields. Temporal profile lanes are separate:

```text
current HRRR 5-var profile core .wxp: 16.75 GiB
HRRR source VolumeStore has 11 pressure vars available
all-11 pressure profile .wxp estimate: ~35-40 GiB
native flat intermediate estimate: ~150 GiB, so it should not be retained
```

## Important Boundary

The API can now expose and materialize product grids, but the local proof only materialized products whose dependencies are present locally. The final production builder needs to feed WxStore from rustwx direct/derived outputs for every supported product.

The native WXA dense lane implemented here is:

```text
axes: field_id, forecast_hour, y, x
chunk: field=1, hour=1, y=256, x=256
payload: zstd-compressed f32 chunks
index: fixed records with offsets, lengths, min/max, valid counts
manifest: inline JSON metadata per product file
```

The current Zarr adapter remains useful as a source/proof adapter, not the final on-disk serving format.

## Current Blocker

The remaining blocker for making every non-ECAPE rustwx HRRR product physically available in WxStore is not Mapbox or serving. It is export plumbing from rustwx.

Current rustwx state:

```text
direct products:   internal Field2D producers exist
derived products:  internal light-derived Field2D producers exist
windowed products: internal Field2D producers exist
existing CLIs:     render PNG/report/manifest artifacts, not complete raw f32 grid exports
```

Needed next builder:

```text
hrrr_non_ecape_grid_export
  input: model run, hours, product filter
  output: Field2D grids + provenance manifests
  target: WXA dense2d files or neutral f32 grid bundles for WxStore ingestion
```

Products that remain true meteorology/source blockers rather than WxStore routing blockers:

```text
stp_effective
scp
scp_effective
lightning_flash_density
full smoke/native products unless the wrfnat MASSDEN/COLMD extraction path is wired into the exporter
absolute-vorticity pressure maps until the all-variable profile lane or direct pressure exporter includes ABSV
```
