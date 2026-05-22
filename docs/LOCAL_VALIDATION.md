# Local Validation And Benchmarking

These commands exercise a local WxStore process only. They do not require a
remote server.

## Start WxStore

Build the service and start it with whatever local stores you have available:

```powershell
cargo build --release

.\target\release\wxstore.exe serve `
  --spatial-root C:\path\to\wxstore\data\spatial `
  --host 127.0.0.1 `
  --port 8897
```

Add optional roots when validating the corresponding lanes:

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

If you are validating older proof data, set `--spatial-root` to the root that
contains the desired model/run directories.

## Smoke Validation

Run all discovered local spatial models and the latest run for each model:

```powershell
.\scripts\Invoke-WxStoreSmoke.ps1 -BaseUrl http://127.0.0.1:8897
```

Run selected models and write machine-readable output:

```powershell
.\scripts\Invoke-WxStoreSmoke.ps1 `
  -BaseUrl http://127.0.0.1:8897 `
  -Models hrrr,gfs `
  -RunsPerModel 1 `
  -OutFile .\latest-smoke-results.json
```

The smoke script validates:

- `GET /v1/models`
- `GET /v1/variables?model=...&run=...`
- `GET /v1/sample?model=...&run=...&variable=...&forecast_hour=...`
- `GET /v1/mapbox/layers/{model}/{run}/{variable}`
- `GET /v1/mapbox/tiles/{model}/{run}/{variable}/{frame}/{z}/{x}/{y}.png`
- `GET /v1/forecast?latitude=...&longitude=...&model=...&run=...`
- `GET /v1/wind-field?...` when both `wind_u_10m_ms` and `wind_v_10m_ms` are available

Wind-field checks are reported as `skip` for model/runs without both U and V
wind grids.

## Lightweight Benchmark

Run a sequential local benchmark against a representative discovered model/run:

```powershell
.\scripts\Invoke-WxStoreBenchmark.ps1 -BaseUrl http://127.0.0.1:8897 -Requests 100 -Warmup 5
```

Benchmark a specific product and save JSON:

```powershell
.\scripts\Invoke-WxStoreBenchmark.ps1 `
  -BaseUrl http://127.0.0.1:8897 `
  -Model hrrr `
  -Run 20260429_hrrr_06z `
  -Variable 2m_temperature `
  -ForecastHour 0 `
  -Requests 500 `
  -Warmup 20 `
  -OutFile .\latest-local-benchmark-results.json
```

This script is intentionally simple and PowerShell-native. Use it for local
regression checks, not as a replacement for high-concurrency load tools.

## Expected Smoke Outcome

A validation candidate should have:

- zero `fail` rows;
- expected `skip` rows only for optional products that are not loaded locally;
- `/v1/models` listing every model/run intended for validation;
- `/v1/variables` showing non-empty `available_hours` for the products under test;
- PNG tile responses with non-zero byte counts;
- forecast and sample responses for at least one in-domain point per model.
