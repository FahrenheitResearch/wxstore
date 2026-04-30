# WxStore Product Completion Report

Date: 2026-04-30

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
- `/v1/grid` now accepts raw variables, rustwx-style direct aliases, stored derived products, cheap virtual derived products, and windowed product patterns.
- `materialize-spatial` command computes product grids and writes them as served Zarr-v2 spatial arrays.
- Stored product arrays now take priority over virtual compute paths.
- Product existence is cached so point-forecast routing does not hit the filesystem on every request.

Materializer command shape:

```powershell
C:\Users\drew\wxstore\target\release\wxstore.exe materialize-spatial `
  --spatial-root C:\Users\drew\open-rust-wx\data\spatial `
  --model hrrr `
  --run 20260405_18z `
  --products dewpoint_depression_2m,vpd_2m,heat_index_2m `
  --hours 0-2
```

## Local Materialization Timing

Products materialized from currently available local fields:

| Model | Run | Member | Products | Hours | Product-hour grids | Time |
| --- | --- | --- | --- | --- | ---: | ---: |
| HRRR | `20260405_18z` | control | `dewpoint_depression_2m,vpd_2m,heat_index_2m` | `0-2` | 9 | 1.356 s |
| GFS | `20260405_12z` | control | `dewpoint_depression_2m,vpd_2m,heat_index_2m` | `0-2` | 9 | 0.721 s |
| ECMWF IFS | `20260411_12z` | control | `dewpoint_depression_2m` | `0,3` | 2 | 0.195 s |
| ECMWF ENS | `20260412_00z` | `001` | `dewpoint_depression_2m` | `3` | 1 | 0.091 s |

Total proof: 21 product-hour grids in 2.363 s.

## Materialized Storage

| Product directory | Size |
| --- | ---: |
| `hrrr/20260405_18z/dewpoint_depression_2m.zarr` | 6.48 MiB |
| `hrrr/20260405_18z/vpd_2m.zarr` | 14.84 MiB |
| `hrrr/20260405_18z/heat_index_2m.zarr` | 6.03 MiB |
| `gfs/20260405_12z/dewpoint_depression_2m.zarr` | 3.49 MiB |
| `gfs/20260405_12z/vpd_2m.zarr` | 8.51 MiB |
| `gfs/20260405_12z/heat_index_2m.zarr` | 3.67 MiB |
| `ecmwf_ifs/20260411_12z/dewpoint_depression_2m.zarr` | 2.85 MiB |
| `ecmwf_ens/20260412_00z/member 001/dewpoint_depression_2m.zarr` | 1.43 MiB |

Current HRRR `20260405_18z` local spatial run after adding derived products:

```text
variables: 11
total:     0.674 GiB
average:   62.76 MiB per variable
```

## Serving Benchmarks

Sequential local raw HTTP, release service on `127.0.0.1:8897`.

| Product | Req/s | P50 | P95 | P99 | Avg payload |
| --- | ---: | ---: | ---: | ---: | ---: |
| HRRR raw surface forecast, 3 vars x 3h | 14,592.6 | 5.67 ms | 7.04 ms | 8.44 ms | 1.06 KiB |
| HRRR materialized derived forecast, 3 vars x 3h | 19,319.1 | 5.74 ms | 7.27 ms | 22.88 ms | 1.08 KiB |
| GFS materialized derived forecast, 3 vars x 3h | 6,280.0 | 21.16 ms | 38.38 ms | 45.83 ms | 1.06 KiB |
| HRRR materialized VPD grid binary, `f000` | 475.2 | 32.45 ms | 43.86 ms | 50.98 ms | 7.27 MiB |
| HRRR 48h temporal sounding binary + diagnostics | 10,925.0 | 15.51 ms | 27.10 ms | 37.94 ms | 30.3 KiB |
| HRRR 48h temporal sounding compact JSON + diagnostics | 3,852.4 | 24.60 ms | 32.76 ms | 36.25 ms | 112.6 KiB |

## Storage Math For Full HRRR Map Product Lanes

HRRR CONUS:

```text
grid cells: 1799 * 1059 = 1,905,141
one raw f32 grid: 7.27 MiB
49 forecast hours, one field raw f32: 356 MiB
75 fields, 49h raw f32: ~26.1 GiB
75 fields, 49h raw i16: ~13.0 GiB before compression
```

Empirical current Zarr/zlib HRRR spatial average after materialized fields:

```text
~62.8 MiB per variable for the current mixed 49h/sparse local run
75 fields estimate from current empirical average: ~4.6 GiB
safer production range for 75 HRRR 2D fields: ~5-8 GiB/run
```

This is only for scalar 2D map/grid fields. Temporal profile lanes are separate:

```text
current HRRR 5-var profile core .wxp: 16.75 GiB
HRRR source VolumeStore has 11 pressure vars available
all-11 pressure profile .wxp estimate: ~35-40 GiB
native flat intermediate estimate: ~150 GiB, so it should not be retained
```

## Important Boundary

The API can now expose and materialize product grids, but the local proof only materialized products whose dependencies are present locally. The final production builder needs to feed WxStore from rustwx direct/derived outputs for every supported product, then write native WXA dense lanes instead of relying on the current proof Zarr adapter.

The next storage format step is a native WXA dense lane:

```text
axes: field_id, forecast_hour, y, x
chunk: field=1, hour=1, y=256, x=256
payload: append-only compressed chunks
index: mmap-friendly offsets, codec, stats, checksums
manifest: field provenance, formula versions, units, dependencies
```

The current Zarr adapter remains useful as a source/proof adapter, not the final on-disk serving format.
