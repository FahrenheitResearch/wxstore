# Production Readiness Checklist

Use this checklist before promoting a WxStore build or data refresh.

## Build And Configuration

- Release binary is built from the intended commit: `cargo build --release`.
- Runtime command is recorded with exact `--profile-store`, `--diagnostic-store`,
  `--spatial-root`, `--host`, and `--port` values.
- Data roots are read-only for the serving process except for intentional cache
  or generated-output directories.
- Local validation targets `127.0.0.1` or another explicitly local bind address.
- No validation command points at Hetzner or any remote production host.

## Data Coverage

- `/v1/models` lists all expected model families and run IDs.
- `/v1/variables` returns non-empty `variables` and `available_hours` for each
  production model/run/member.
- HRRR profile lane exposes the intended pressure-profile hours and levels.
- Diagnostic lane is present when diagnostics are part of the advertised API.
- Sparse model coverage is documented, especially ECMWF or ensemble runs with
  limited forecast hours.
- WXA files include grid metadata sufficient for sample, forecast, tile, and
  wind-field endpoints.

## API Smoke

- `.\scripts\Invoke-WxStoreSmoke.ps1 -BaseUrl http://127.0.0.1:8897` exits 0.
- Smoke output has zero `fail` rows.
- Any `skip` rows are expected and tied to products not loaded for that model.
- Manual spot checks cover `/v1/models`, `/v1/variables`, `/v1/sample`,
  `/v1/mapbox/layers`, `/v1/mapbox/tiles`, `/v1/forecast`, and `/v1/wind-field`.
- Mapbox PNG tile responses render valid PNGs and have expected non-zero sizes.
- Forecast JSON contains the requested variables and forecast hours.

## Benchmark And Regression

- Local benchmark output is saved for the build or data refresh:

```powershell
.\scripts\Invoke-WxStoreBenchmark.ps1 `
  -BaseUrl http://127.0.0.1:8897 `
  -Requests 500 `
  -Warmup 20 `
  -OutFile .\latest-local-benchmark-results.json
```

- P50/P95/P99 latencies are compared with `BENCHMARKS.md` or the previous local
  benchmark artifact.
- Tile benchmarks are run after at least one warmup request to separate cold
  decode/cache behavior from steady-state behavior.
- Any payload-size or latency jump is explained by a data, compression, product,
  or code change.

## Operations

- Service logs are captured and reviewed for startup warnings and request errors.
- Health/status endpoint is monitored.
- Disk capacity is checked for profile stores, WXA stores, diagnostics, logs, and
  cache growth.
- Restart procedure is documented and tested locally.
- Rollback artifact and previous data roots remain available until the new build
  is accepted.
- Public documentation reflects the exact model/run/product coverage being
  served.
