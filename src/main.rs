use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs::{self, File},
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Instant,
};

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use memmap2::{Mmap, MmapOptions};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tower_http::cors::CorsLayer;

const WXP_MAGIC: &[u8; 8] = b"ORWXWXP0";
const WXP_VERSION: u32 = 1;
const WXP_HEADER_LEN: usize = 64;
const WXP_INDEX_RECORD_LEN: usize = 16;
const WXA_DENSE2D_MAGIC: &[u8; 8] = b"WXAD2D1!";
const WXA_DENSE2D_VERSION: u32 = 1;
const WXA_DENSE2D_HEADER_LEN: usize = 64;
const WXA_DENSE2D_INDEX_RECORD_LEN: usize = 64;
const WXA_SPATIAL_CHUNK_Y: usize = 256;
const WXA_SPATIAL_CHUNK_X: usize = 256;
const WXBIN_MAGIC: &[u8; 8] = b"WXPTBIN1";
const MISSING_I16: i16 = i16::MIN;
const CACHE_LIMIT: usize = 512;

const SOUNDING_CORE: &[&str] = &["TMP", "SPFH", "UGRD", "VGRD", "HGT"];
const BASIC_DIAGNOSTICS: &[&str] = &[
    "sbcape_j_kg",
    "sbcin_j_kg",
    "mlcape_j_kg",
    "mucape_j_kg",
    "dcape_j_kg",
    "srh_0_1km_m2_s2",
    "srh_0_3km_m2_s2",
    "shear_0_6km_ms",
    "stp_fixed",
    "scp",
    "ehi_0_1km",
    "pw_in",
    "lapse_rate_700_500_c_km",
];
const SEVERE_DIAGNOSTICS: &[&str] = &[
    "sbcape_j_kg",
    "sbcin_j_kg",
    "mlcape_j_kg",
    "mlcin_j_kg",
    "mucape_j_kg",
    "mucin_j_kg",
    "dcape_j_kg",
    "srh_0_1km_m2_s2",
    "srh_0_3km_m2_s2",
    "shear_0_1km_ms",
    "shear_0_3km_ms",
    "shear_0_6km_ms",
    "shear_0_8km_ms",
    "stp_fixed",
    "stp_cin",
    "scp",
    "ship",
    "ehi_0_1km",
    "ehi_0_3km",
    "lapse_rate_700_500_c_km",
    "lapse_rate_850_500_c_km",
];
const PARCEL_DIAGNOSTICS: &[&str] = &[
    "sbcape_j_kg",
    "sbcin_j_kg",
    "sblcl_m_agl",
    "sblfc_m_agl",
    "sbel_m_agl",
    "mlcape_j_kg",
    "mlcin_j_kg",
    "mllcl_m_agl",
    "mucape_j_kg",
    "mucin_j_kg",
    "mulcl_m_agl",
];

#[derive(Parser)]
#[command(name = "wxstore", about = "Model-agnostic weather store/API")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Serve(ServeArgs),
    Inspect(InspectArgs),
    MaterializeSpatial(MaterializeSpatialArgs),
    ImportRustwxGrids(ImportRustwxGridsArgs),
}

#[derive(Parser, Clone)]
struct ServeArgs {
    #[arg(long)]
    profile_store: PathBuf,
    #[arg(long)]
    diagnostic_store: Option<PathBuf>,
    #[arg(long)]
    spatial_root: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8897)]
    port: u16,
}

#[derive(Parser, Clone)]
struct InspectArgs {
    #[arg(long)]
    profile_store: PathBuf,
    #[arg(long)]
    diagnostic_store: Option<PathBuf>,
    #[arg(long)]
    spatial_root: Option<PathBuf>,
}

#[derive(Parser, Clone)]
struct MaterializeSpatialArgs {
    #[arg(long)]
    spatial_root: PathBuf,
    #[arg(long)]
    model: String,
    #[arg(long)]
    run: String,
    #[arg(long)]
    member: Option<String>,
    #[arg(long)]
    products: String,
    #[arg(long, default_value = "0-2")]
    hours: String,
    #[arg(long, default_value = "wxa")]
    output_format: String,
}

#[derive(Parser, Clone)]
struct ImportRustwxGridsArgs {
    #[arg(long)]
    manifest: PathBuf,
    #[arg(long)]
    spatial_root: PathBuf,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    run: Option<String>,
    #[arg(long)]
    member: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    match Cli::parse().command {
        Command::Serve(args) => serve(args).await,
        Command::Inspect(args) => {
            let profile = Arc::new(ProfileLane::open(&args.profile_store)?);
            let diagnostic = args
                .diagnostic_store
                .as_ref()
                .map(|path| DiagnosticLane::open(path))
                .transpose()?;
            let spatial = args
                .spatial_root
                .as_ref()
                .map(|path| SpatialLane::open(path))
                .transpose()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&store_status(
                    &profile,
                    diagnostic.as_ref(),
                    spatial.as_ref()
                ))?
            );
            Ok(())
        }
        Command::MaterializeSpatial(args) => materialize_spatial(args),
        Command::ImportRustwxGrids(args) => import_rustwx_grids(args),
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let state = Arc::new(AppState {
        profile: Arc::new(ProfileLane::open(&args.profile_store)?),
        diagnostic: args
            .diagnostic_store
            .as_ref()
            .map(|path| DiagnosticLane::open(path))
            .transpose()
            .map(|lane| lane.map(Arc::new))?,
        spatial: args
            .spatial_root
            .as_ref()
            .map(|path| SpatialLane::open(path))
            .transpose()
            .map(|lane| lane.map(Arc::new))?,
        cache: RwLock::new(ResponseCache::default()),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/v1/status", get(status))
        .route("/api/status", get(status))
        .route("/v1/models", get(models))
        .route("/v1/variables", get(variables))
        .route("/v1/products", get(products))
        .route("/v1/grid", get(grid_field))
        .route("/v1/sample", get(sample_point))
        .route("/v1/wind-field", get(wind_field))
        .route("/v1/layers", get(layers))
        .route("/v1/tilejson/{model}/{run}/{variable}", get(tilejson))
        .route(
            "/v1/tiles/{model}/{run}/{variable}/{forecast_hour}/{z}/{x}/{y}",
            get(raster_tile),
        )
        .route(
            "/v1/mapbox/layers/{model}/{run}/{variable}",
            get(mapbox_layer),
        )
        .route(
            "/v1/mapbox/tilejson/{model}/{run}/{variable}/{frame}",
            get(mapbox_tilejson),
        )
        .route(
            "/v1/mapbox/tiles/{model}/{run}/{variable}/{frame}/{z}/{x}/{y}",
            get(mapbox_raster_tile),
        )
        .route("/v1/forecast", get(forecast))
        .route("/v1/latest/{model}/{domain}", get(latest))
        .route("/v1/resolve", get(resolve))
        .route("/v1/temporal-sounding", get(temporal_sounding))
        .route("/api/point", get(temporal_sounding))
        .route("/v1/point.bin", get(point_bin))
        .route(
            "/v1/runs/{model}/{domain}/{run}/grid/{x}/{y}/temporal-sounding",
            get(canonical_temporal_sounding),
        )
        .route(
            "/v1/runs/{model}/{domain}/{run}/grid/{x}/{y}/temporal-sounding.bin",
            get(canonical_point_bin),
        )
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    println!("WxStore listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn materialize_spatial(args: MaterializeSpatialArgs) -> Result<()> {
    let started = Instant::now();
    let lane = SpatialLane::open(&args.spatial_root)?;
    let products = split_csv(&args.products);
    let hours = parse_hours_u32(&args.hours)?;
    let output_format = args.output_format.to_ascii_lowercase();
    if products.is_empty() {
        bail!("--products must list at least one product");
    }
    if hours.is_empty() {
        bail!("--hours must list at least one hour");
    }
    if output_format != "wxa" && output_format != "zarr" {
        bail!("--output-format must be 'wxa' or 'zarr'");
    }

    let mut wrote = Vec::new();
    let mut errors = Vec::new();

    for product in products {
        if output_format == "wxa" {
            let product_started = Instant::now();
            let mut grids = Vec::new();
            for hour in &hours {
                match lane.read_grid(
                    &args.model,
                    &args.run,
                    args.member.as_deref(),
                    &product,
                    *hour,
                ) {
                    Ok(grid) => grids.push(grid),
                    Err(err) => errors.push(json!({
                        "product": product,
                        "hour": hour,
                        "error": err.to_string()
                    })),
                }
            }
            if !grids.is_empty() {
                match write_spatial_wxa_grids(
                    &args.spatial_root,
                    &args.model,
                    &args.run,
                    args.member.as_deref(),
                    &product,
                    &grids,
                ) {
                    Ok(path) => {
                        let bytes = fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0);
                        for grid in &grids {
                            wrote.push(json!({
                                "product": product,
                                "hour": grid.forecast_hour,
                                "path": path,
                                "bytes": bytes,
                                "format": "wxa_dense2d",
                                "elapsed_ms": product_started.elapsed().as_millis()
                            }));
                        }
                    }
                    Err(err) => errors.push(json!({
                        "product": product,
                        "hours": hours,
                        "error": err.to_string()
                    })),
                }
            }
        } else {
            for hour in &hours {
                let item_started = Instant::now();
                match lane.read_grid(
                    &args.model,
                    &args.run,
                    args.member.as_deref(),
                    &product,
                    *hour,
                ) {
                    Ok(grid) => {
                        let path = match write_spatial_zarr_grid(
                            &args.spatial_root,
                            &args.model,
                            &args.run,
                            args.member.as_deref(),
                            &product,
                            *hour,
                            &grid,
                        ) {
                            Ok(path) => path,
                            Err(err) => {
                                errors.push(json!({
                                    "product": product,
                                    "hour": hour,
                                    "error": err.to_string()
                                }));
                                continue;
                            }
                        };
                        wrote.push(json!({
                            "product": product,
                            "hour": hour,
                            "path": path,
                            "bytes": fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0),
                            "format": "zarr_v2",
                            "elapsed_ms": item_started.elapsed().as_millis()
                        }));
                    }
                    Err(err) => errors.push(json!({
                        "product": product,
                        "hour": hour,
                        "error": err.to_string()
                    })),
                }
            }
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": "wxstore.materialize_spatial.report.v1",
            "model": args.model,
            "run": args.run,
            "member": args.member,
            "output_format": output_format,
            "hours": hours,
            "wrote_count": wrote.len(),
            "error_count": errors.len(),
            "elapsed_ms": started.elapsed().as_millis(),
            "wrote": wrote,
            "errors": errors
        }))?
    );
    Ok(())
}

fn import_rustwx_grids(args: ImportRustwxGridsArgs) -> Result<()> {
    let started = Instant::now();
    let manifest_path = args.manifest;
    let manifest_dir = manifest_path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let manifest: RustwxGridExportManifest = serde_json::from_slice(
        &fs::read(&manifest_path).with_context(|| format!("read {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parse {}", manifest_path.display()))?;
    let model = args.model.unwrap_or(manifest.model.clone());
    let run = args.run.unwrap_or(manifest.run_id.clone());
    let member = args.member.or_else(|| Some("control".to_string()));

    let mut by_product = BTreeMap::<String, Vec<SpatialGrid>>::new();
    for record in manifest.fields {
        let values_path = resolve_export_path(&manifest_dir, &record.values_path);
        let lat_path = resolve_export_path(&manifest_dir, &record.lat_path);
        let lon_path = resolve_export_path(&manifest_dir, &record.lon_path);
        let values = read_f32_file(&values_path)
            .with_context(|| format!("read values {}", values_path.display()))?;
        let lat = read_f32_file(&lat_path)
            .with_context(|| format!("read latitudes {}", lat_path.display()))?;
        let lon = read_f32_file(&lon_path)
            .with_context(|| format!("read longitudes {}", lon_path.display()))?;
        if values.len() != record.nx * record.ny
            || lat.len() != record.nx * record.ny
            || lon.len() != record.nx * record.ny
        {
            bail!(
                "grid '{}' f{:03} has inconsistent dimensions",
                record.product_slug,
                record.forecast_hour
            );
        }
        let grid_meta = grid_meta_from_latlon(&model, record.nx, record.ny, &lat, &lon, &record);
        by_product
            .entry(record.product_slug.clone())
            .or_default()
            .push(SpatialGrid {
                model: model.clone(),
                run_id: run.clone(),
                member: member.clone(),
                variable: record.product_slug,
                units: record.units,
                forecast_hour: u32::from(record.forecast_hour),
                nx: record.nx,
                ny: record.ny,
                grid_meta,
                values: Arc::new(values),
            });
    }

    let mut wrote = Vec::new();
    for (product, grids) in &by_product {
        let path = write_spatial_wxa_grids(
            &args.spatial_root,
            &model,
            &run,
            member.as_deref(),
            product,
            grids,
        )?;
        wrote.push(json!({
            "product": product,
            "path": path,
            "hours": grids.iter().map(|grid| grid.forecast_hour).collect::<Vec<_>>(),
            "bytes": fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0)
        }));
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": "wxstore.import_rustwx_grids.report.v1",
            "source_manifest": manifest_path,
            "model": model,
            "run": run,
            "member": member,
            "product_count": wrote.len(),
            "elapsed_ms": started.elapsed().as_millis(),
            "wrote": wrote,
            "source_blockers": manifest.blockers
        }))?
    );
    Ok(())
}

struct AppState {
    profile: Arc<ProfileLane>,
    diagnostic: Option<Arc<DiagnosticLane>>,
    spatial: Option<Arc<SpatialLane>>,
    cache: RwLock<ResponseCache>,
}

#[derive(Default)]
struct ResponseCache {
    entries: HashMap<String, Bytes>,
    order: VecDeque<String>,
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

const INDEX_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>WxStore Layer Viewer</title>
  <link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css" />
  <style>
    html, body, .maps, .map { height: 100%; margin: 0; }
    body { font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background: #111827; color: #111827; }
    .maps { display: grid; grid-template-columns: 1fr; }
    body.compare .maps { grid-template-columns: 1fr 1fr; gap: 2px; }
    .map { background: #111827; min-width: 0; }
    #mapB { display: none; }
    body.compare #mapB { display: block; }
    .panel {
      position: absolute;
      z-index: 1000;
      top: 12px;
      left: 12px;
      display: grid;
      grid-template-columns: 150px 150px minmax(190px, 280px) 78px 96px 78px 78px 74px;
      gap: 8px;
      align-items: end;
      padding: 10px;
      border-radius: 8px;
      background: rgba(255,255,255,0.94);
      box-shadow: 0 8px 24px rgba(0,0,0,0.22);
      max-width: calc(100vw - 24px);
    }
    label { display: grid; gap: 4px; font-size: 11px; font-weight: 700; color: #374151; }
    select, input, button {
      height: 34px;
      border: 1px solid #cbd5e1;
      border-radius: 6px;
      background: #fff;
      color: #111827;
      font: inherit;
      font-size: 13px;
      padding: 0 8px;
    }
    .check { display: flex; align-items: center; gap: 6px; height: 34px; }
    .check input { width: 16px; height: 16px; padding: 0; }
    button { cursor: pointer; background: #0f172a; color: #fff; border-color: #0f172a; font-weight: 700; }
    .status {
      position: absolute;
      z-index: 1000;
      left: 12px;
      bottom: 12px;
      max-width: min(720px, calc(100vw - 24px));
      padding: 8px 10px;
      border-radius: 6px;
      background: rgba(17,24,39,0.88);
      color: #e5e7eb;
      font-size: 12px;
      line-height: 1.35;
    }
    .usage-panel {
      position: absolute;
      z-index: 1000;
      right: 12px;
      bottom: 12px;
      width: 306px;
      padding: 10px;
      border-radius: 8px;
      background: rgba(255,255,255,0.94);
      box-shadow: 0 8px 24px rgba(0,0,0,0.22);
      color: #111827;
      font-size: 12px;
    }
    .usage-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      margin-bottom: 8px;
    }
    .usage-title { font-weight: 800; color: #111827; }
    .usage-state {
      padding: 2px 6px;
      border-radius: 999px;
      background: #e5e7eb;
      color: #374151;
      font-size: 11px;
      font-weight: 800;
    }
    .usage-state.running { background: #dcfce7; color: #166534; }
    .usage-buttons { display: grid; grid-template-columns: 1fr 1fr 1fr; gap: 6px; margin-bottom: 8px; }
    .usage-buttons button { height: 30px; font-size: 12px; padding: 0 6px; }
    .usage-grid { display: grid; grid-template-columns: 1fr 1fr; gap: 6px 10px; }
    .metric { display: grid; gap: 1px; min-width: 0; }
    .metric span { color: #64748b; font-size: 10px; font-weight: 800; text-transform: uppercase; }
    .metric strong { color: #111827; font-size: 13px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .usage-note { margin-top: 8px; color: #475569; font-size: 11px; line-height: 1.35; }
    .picker-panel {
      position: absolute;
      z-index: 1000;
      right: 12px;
      top: 92px;
      width: 306px;
      padding: 10px;
      border-radius: 8px;
      background: rgba(17,24,39,0.88);
      box-shadow: 0 8px 24px rgba(0,0,0,0.22);
      color: #e5e7eb;
      font-size: 12px;
      pointer-events: none;
    }
    .picker-title { color: #cbd5e1; font-size: 11px; font-weight: 800; text-transform: uppercase; margin-bottom: 4px; }
    .picker-value { font-size: 22px; font-weight: 850; color: #fff; line-height: 1.15; }
    .picker-meta { margin-top: 5px; color: #cbd5e1; line-height: 1.35; overflow-wrap: anywhere; }
    .badge {
      position: absolute;
      z-index: 900;
      top: 70px;
      padding: 5px 8px;
      border-radius: 5px;
      background: rgba(17,24,39,0.78);
      color: #fff;
      font-size: 12px;
      font-weight: 700;
      pointer-events: none;
    }
    #badgeA { left: 12px; }
    #badgeB { display: none; left: calc(50% + 14px); }
    body.compare #badgeB { display: block; }
    @media (max-width: 980px) {
      .panel { grid-template-columns: 1fr 1fr 1fr 74px; right: 12px; }
      .wide { grid-column: 1 / -1; }
      .usage-panel { left: 12px; right: 12px; bottom: 12px; width: auto; }
      .picker-panel { left: 12px; right: 12px; top: auto; bottom: 292px; width: auto; }
      .status { bottom: 154px; }
      body.compare .maps { grid-template-columns: 1fr; grid-template-rows: 1fr 1fr; }
      #badgeB { left: 12px; top: calc(50% + 72px); }
    }
  </style>
</head>
<body>
  <div class="maps">
    <div id="mapA" class="map"></div>
    <div id="mapB" class="map"></div>
  </div>
  <div id="badgeA" class="badge"></div>
  <div id="badgeB" class="badge"></div>
  <div id="pickerPanel" class="picker-panel">
    <div class="picker-title">Hover Picker</div>
    <div id="pickerValue" class="picker-value">move over map</div>
    <div id="pickerMeta" class="picker-meta">Samples the selected layer/hour from the WxStore grid.</div>
  </div>
  <div class="panel">
    <label>Model<select id="model"></select></label>
    <label>Run A<select id="runA"></select></label>
    <label>Run B<select id="runB"></select></label>
    <label class="wide">Layer<select id="layer"></select></label>
    <label>Hour<select id="hour"></select></label>
    <label>Palette<select id="palette">
      <option value="auto">auto</option>
      <option value="vpd">vpd</option>
      <option value="severe">severe</option>
      <option value="temp">temp</option>
      <option value="precip">precip</option>
      <option value="viridis">viridis</option>
    </select></label>
    <label>Min<input id="min" inputmode="decimal" /></label>
    <label>Max<input id="max" inputmode="decimal" /></label>
    <label>Compare<span class="check"><input id="compare" type="checkbox" /> side</span></label>
    <button id="apply">Apply</button>
  </div>
  <div id="status" class="status">Loading WxStore layers...</div>
  <div id="usagePanel" class="usage-panel">
    <div class="usage-head">
      <div class="usage-title">Overlay Usage</div>
      <div id="usageState" class="usage-state">stopped</div>
    </div>
    <div class="usage-buttons">
      <button id="usageStart">Start</button>
      <button id="usageStop">Stop</button>
      <button id="usageReset">Reset</button>
    </div>
    <div class="usage-grid">
      <div class="metric"><span>elapsed</span><strong id="usageElapsed">0.0s</strong></div>
      <div class="metric"><span>visible</span><strong id="usageVisible">0 tiles</strong></div>
      <div class="metric"><span>tile reqs</span><strong id="usageTiles">0</strong></div>
      <div class="metric"><span>unique tiles</span><strong id="usageUnique">0</strong></div>
      <div class="metric"><span>payload</span><strong id="usagePayload">0 B</strong></div>
      <div class="metric"><span>wire</span><strong id="usageWire">0 B</strong></div>
      <div class="metric"><span>avg tile</span><strong id="usageAvg">0 B</strong></div>
      <div class="metric"><span>rate</span><strong id="usageRate">0 Mbps</strong></div>
    </div>
    <div class="usage-note">Counts same-origin WxStore overlay tiles/layer metadata only. Base-map tiles are ignored. Start, change/apply a layer, then pan or zoom.</div>
  </div>
  <script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js"></script>
  <script>
    const els = {
      model: document.getElementById("model"),
      runA: document.getElementById("runA"),
      runB: document.getElementById("runB"),
      layer: document.getElementById("layer"),
      hour: document.getElementById("hour"),
      palette: document.getElementById("palette"),
      min: document.getElementById("min"),
      max: document.getElementById("max"),
      compare: document.getElementById("compare"),
      apply: document.getElementById("apply"),
      status: document.getElementById("status"),
      badgeA: document.getElementById("badgeA"),
      badgeB: document.getElementById("badgeB"),
      pickerValue: document.getElementById("pickerValue"),
      pickerMeta: document.getElementById("pickerMeta"),
      usageState: document.getElementById("usageState"),
      usageStart: document.getElementById("usageStart"),
      usageStop: document.getElementById("usageStop"),
      usageReset: document.getElementById("usageReset"),
      usageElapsed: document.getElementById("usageElapsed"),
      usageVisible: document.getElementById("usageVisible"),
      usageTiles: document.getElementById("usageTiles"),
      usageUnique: document.getElementById("usageUnique"),
      usagePayload: document.getElementById("usagePayload"),
      usageWire: document.getElementById("usageWire"),
      usageAvg: document.getElementById("usageAvg"),
      usageRate: document.getElementById("usageRate"),
    };
    const mapA = L.map("mapA", { zoomControl: true }).setView([36.5, -116.5], 5);
    const mapB = L.map("mapB", { zoomControl: false }).setView([36.5, -116.5], 5);
    for (const map of [mapA, mapB]) {
      L.tileLayer("https://tile.openstreetmap.org/{z}/{x}/{y}.png", {
        maxZoom: 12,
        attribution: "&copy; OpenStreetMap"
      }).addTo(map);
    }
    let syncing = false;
    function syncMaps(source, target) {
      if (syncing) return;
      syncing = true;
      target.setView(source.getCenter(), source.getZoom(), { animate: false });
      syncing = false;
    }
    mapA.on("moveend", () => syncMaps(mapA, mapB));
    mapB.on("moveend", () => syncMaps(mapB, mapA));
    let overlayA = null;
    let overlayB = null;
    let variablesByRun = {};
    let modelInfoById = {};
    let runs = [];
    let pickerAbort = null;
    let pickerLastAt = 0;
    const usage = {
      active: false,
      startedAt: 0,
      stoppedElapsedMs: 0,
      timer: null,
      seenEntries: new Set(),
      uniqueTileUrls: new Set(),
      tileRequests: 0,
      metadataRequests: 0,
      payloadBytes: 0,
      wireBytes: 0,
      cachedResponses: 0,
    };

    function tileBase() {
      return location.origin;
    }

    function addBase(map) {
      return L.tileLayer("https://tile.openstreetmap.org/{z}/{x}/{y}.png", {
      maxZoom: 12,
      attribution: "&copy; OpenStreetMap"
      }).addTo(map);
    }

    function defaultsFor(name) {
      const lower = name.toLowerCase();
      if (lower.includes("vpd")) return ["vpd", "0", "5"];
      if (lower.includes("stp") || lower.includes("scp") || lower.includes("ehi")) return ["severe", "0", "5"];
      if (lower.includes("cape")) return ["severe", "0", "3000"];
      if (lower.includes("cin")) return ["severe", "-250", "0"];
      if (lower.includes("qpf") || lower.includes("precip")) return ["precip", "0", "0.25"];
      if (lower.includes("temp") || lower.includes("dewpoint") || lower.includes("wetbulb")) return ["temp", "-20", "40"];
      if (lower.includes("rh") || lower.includes("humidity") || lower.includes("cloud")) return ["viridis", "0", "100"];
      if (lower.includes("wind") || lower.includes("shear")) return ["viridis", "0", "40"];
      return ["viridis", "0", "1"];
    }

    function setStatus(text) {
      els.status.textContent = text;
    }

    function activeRunForMap(map) {
      return map === mapB && els.compare.checked ? els.runB.value : els.runA.value;
    }

    function formatLayerValue(value, units) {
      if (value === null || value === undefined || !Number.isFinite(Number(value))) {
        return "no data";
      }
      const n = Number(value);
      const precision = Math.abs(n) >= 100 ? 0 : Math.abs(n) >= 10 ? 1 : 2;
      return `${n.toFixed(precision)} ${units || ""}`.trim();
    }

    function setPickerWaiting(latlng, run) {
      els.pickerValue.textContent = "sampling...";
      els.pickerMeta.textContent = `${els.layer.value} f${String(els.hour.value).padStart(3, "0")} | ${run} | ${latlng.lat.toFixed(4)}, ${latlng.lng.toFixed(4)}`;
    }

    async function samplePicker(map, latlng) {
      const now = performance.now();
      if (now - pickerLastAt < 120) return;
      pickerLastAt = now;
      const run = activeRunForMap(map);
      const layer = els.layer.value;
      const hour = els.hour.value;
      if (!run || !layer || hour === "") return;
      if (pickerAbort) pickerAbort.abort();
      pickerAbort = new AbortController();
      setPickerWaiting(latlng, run);
      const params = new URLSearchParams({
        model: els.model.value,
        run,
        variable: layer,
        forecast_hour: hour,
        lat: latlng.lat.toString(),
        lon: latlng.lng.toString(),
      });
      try {
        const res = await fetch(`/v1/sample?${params.toString()}`, { signal: pickerAbort.signal });
        if (!res.ok) throw new Error(`sample ${res.status}`);
        const data = await res.json();
        if (!data.in_domain) {
          els.pickerValue.textContent = "outside layer";
          els.pickerMeta.textContent = `${layer} f${String(hour).padStart(3, "0")} | ${latlng.lat.toFixed(4)}, ${latlng.lng.toFixed(4)}`;
          return;
        }
        els.pickerValue.textContent = formatLayerValue(data.value, data.units);
        const grid = data.grid || {};
        els.pickerMeta.textContent = `${data.variable} f${String(data.forecast_hour).padStart(3, "0")} | ${data.run_id} | grid ${grid.x},${grid.y} | ${data.requested.lat.toFixed(4)}, ${data.requested.lon.toFixed(4)}`;
      } catch (err) {
        if (err.name === "AbortError") return;
        els.pickerValue.textContent = "sample failed";
        els.pickerMeta.textContent = err.message;
      }
    }

    function formatBytes(bytes) {
      if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
      const units = ["B", "KB", "MB", "GB"];
      let value = bytes;
      let unit = 0;
      while (value >= 1024 && unit < units.length - 1) {
        value /= 1024;
        unit += 1;
      }
      return `${value >= 10 || unit === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`;
    }

    function usageElapsedMs() {
      return usage.active ? performance.now() - usage.startedAt : usage.stoppedElapsedMs;
    }

    function currentVisibleOverlayTiles() {
      return [overlayA, overlayB].reduce((count, layer) => {
        if (!layer || !layer._tiles) return count;
        return count + Object.keys(layer._tiles).length;
      }, 0);
    }

    function resetUsageCounters() {
      usage.seenEntries.clear();
      usage.uniqueTileUrls.clear();
      usage.tileRequests = 0;
      usage.metadataRequests = 0;
      usage.payloadBytes = 0;
      usage.wireBytes = 0;
      usage.cachedResponses = 0;
      usage.stoppedElapsedMs = 0;
      if (performance.clearResourceTimings) performance.clearResourceTimings();
      updateUsageDisplay();
    }

    function updateUsageDisplay() {
      const elapsedMs = usageElapsedMs();
      const elapsedSec = elapsedMs / 1000;
      const avgTile = usage.tileRequests ? usage.payloadBytes / usage.tileRequests : 0;
      const mbps = elapsedSec > 0 ? (usage.wireBytes * 8) / elapsedSec / 1_000_000 : 0;
      els.usageState.textContent = usage.active ? "running" : "stopped";
      els.usageState.classList.toggle("running", usage.active);
      els.usageElapsed.textContent = `${elapsedSec.toFixed(1)}s`;
      els.usageVisible.textContent = `${currentVisibleOverlayTiles()} tiles`;
      els.usageTiles.textContent = String(usage.tileRequests);
      els.usageUnique.textContent = String(usage.uniqueTileUrls.size);
      els.usagePayload.textContent = formatBytes(usage.payloadBytes);
      els.usageWire.textContent = formatBytes(usage.wireBytes);
      els.usageAvg.textContent = formatBytes(avgTile);
      els.usageRate.textContent = `${mbps.toFixed(mbps >= 10 ? 1 : 2)} Mbps`;
    }

    function classifyWxStoreResource(urlText) {
      let url;
      try {
        url = new URL(urlText, location.href);
      } catch {
        return null;
      }
      if (url.origin !== location.origin) return null;
      if (url.pathname.includes("/v1/mapbox/tiles/")) return "tile";
      if (url.pathname.includes("/v1/mapbox/layers/") || url.pathname.includes("/v1/mapbox/tilejson/")) return "metadata";
      return null;
    }

    function recordResourceTiming(entry) {
      if (!usage.active) return;
      const kind = classifyWxStoreResource(entry.name);
      if (!kind) return;
      const key = `${entry.name}|${entry.startTime.toFixed(3)}|${entry.duration.toFixed(3)}`;
      if (usage.seenEntries.has(key)) return;
      usage.seenEntries.add(key);
      const payloadBytes = entry.encodedBodySize || entry.decodedBodySize || 0;
      const wireBytes = entry.transferSize || 0;
      usage.payloadBytes += payloadBytes;
      usage.wireBytes += wireBytes;
      if (wireBytes === 0 && payloadBytes > 0) usage.cachedResponses += 1;
      if (kind === "tile") {
        usage.tileRequests += 1;
        usage.uniqueTileUrls.add(entry.name);
      } else {
        usage.metadataRequests += 1;
      }
      updateUsageDisplay();
    }

    function startUsageMonitor() {
      resetUsageCounters();
      usage.active = true;
      usage.startedAt = performance.now();
      usage.timer = window.setInterval(updateUsageDisplay, 500);
      updateUsageDisplay();
      setStatus("Usage monitor running. Apply a layer, pan, or zoom to measure overlay tile traffic.");
    }

    function stopUsageMonitor() {
      if (usage.active) {
        usage.stoppedElapsedMs = performance.now() - usage.startedAt;
      }
      usage.active = false;
      if (usage.timer) {
        window.clearInterval(usage.timer);
        usage.timer = null;
      }
      updateUsageDisplay();
    }

    if ("PerformanceObserver" in window) {
      const observer = new PerformanceObserver(list => {
        for (const entry of list.getEntries()) recordResourceTiming(entry);
      });
      try {
        observer.observe({ type: "resource", buffered: true });
      } catch {
        observer.observe({ entryTypes: ["resource"] });
      }
    }

    async function loadModelList() {
      const res = await fetch("/v1/models");
      if (!res.ok) throw new Error(`models failed: ${res.status}`);
      const data = await res.json();
      const models = data.spatial_loaded.models || [];
      modelInfoById = {};
      els.model.innerHTML = "";
      for (const model of models) {
        modelInfoById[model.id] = model;
        const opt = document.createElement("option");
        opt.value = model.id;
        opt.textContent = model.id;
        els.model.appendChild(opt);
      }
      if (models.some(item => item.id === "hrrr")) {
        els.model.value = "hrrr";
      } else if (models[0]) {
        els.model.value = models[0].id;
      } else {
        const opt = document.createElement("option");
        opt.value = "hrrr";
        opt.textContent = "hrrr";
        els.model.appendChild(opt);
        els.model.value = "hrrr";
      }
      loadRunList();
    }

    function loadRunList() {
      const model = modelInfoById[els.model.value];
      variablesByRun = {};
      runs = model ? (model.runs || []).slice() : ["20260429_hrrr_06z"];
      runs.sort();
      for (const select of [els.runA, els.runB]) {
        select.innerHTML = "";
        for (const run of runs) {
          const opt = document.createElement("option");
          opt.value = run;
          opt.textContent = run;
          select.appendChild(opt);
        }
      }
      els.runA.value = runs[runs.length - 1] || "";
      els.runB.value = runs[0] || els.runA.value;
    }

    async function loadVariablesFor(run) {
      const key = `${els.model.value}|${run}`;
      if (variablesByRun[key]) return variablesByRun[key];
      const res = await fetch(`/v1/variables?model=${els.model.value}&run=${run}`);
      if (!res.ok) throw new Error(`variables failed: ${res.status}`);
      const data = await res.json();
      variablesByRun[key] = { hours: data.available_hours || {}, variables: data.variables || [] };
      return variablesByRun[key];
    }

    async function refreshLayerList() {
      const run = els.runA.value;
      const data = await loadVariablesFor(run);
      els.layer.innerHTML = "";
      for (const name of data.variables) {
        const opt = document.createElement("option");
        opt.value = name;
        opt.textContent = name;
        els.layer.appendChild(opt);
      }
      const preferred = ["vpd_2m", "stp_fixed", "2m_temperature", "composite_reflectivity"].find(v => data.hours[v]);
      if (preferred) els.layer.value = preferred;
      await refreshHours();
    }

    async function refreshHours() {
      const runA = await loadVariablesFor(els.runA.value);
      const runB = await loadVariablesFor(els.runB.value);
      const a = runA.hours[els.layer.value] || [];
      const b = runB.hours[els.layer.value] || [];
      const common = els.compare.checked ? a.filter(hour => b.includes(hour)) : a;
      els.hour.innerHTML = "";
      for (const hour of common) {
        const opt = document.createElement("option");
        opt.value = hour;
        opt.textContent = `f${String(hour).padStart(3, "0")}`;
        els.hour.appendChild(opt);
      }
      const [palette, min, max] = defaultsFor(els.layer.value);
      els.palette.value = palette;
      els.min.value = min;
      els.max.value = max;
    }

    async function layerFor(run, map, existingOverlay, badge) {
      const layer = els.layer.value;
      const hour = els.hour.value;
      if (!run || !layer || hour === "") return existingOverlay;
      const palette = els.palette.value === "auto" ? defaultsFor(layer)[0] : els.palette.value;
      const range = `${els.min.value},${els.max.value}`;
      const url = `/v1/mapbox/layers/${els.model.value}/${run}/${layer}?hours=${hour}&palette=${encodeURIComponent(palette)}&range=${encodeURIComponent(range)}&base_url=${encodeURIComponent(tileBase())}`;
      const res = await fetch(url);
      if (!res.ok) throw new Error(`${run} layer failed: ${res.status}`);
      const data = await res.json();
      const frame = data.frames && data.frames[0];
      if (!frame) throw new Error(`${run} has no frame`);
      if (existingOverlay) map.removeLayer(existingOverlay);
      const next = L.tileLayer(frame.tiles[0] + ".png", { opacity: 0.72, maxZoom: data.maxzoom || 9 }).addTo(map);
      next.on("tileload tileerror loading load", updateUsageDisplay);
      if (data.bounds) {
        map.fitBounds([[data.bounds[1], data.bounds[0]], [data.bounds[3], data.bounds[2]]], { padding: [18, 18] });
      }
      badge.textContent = `${run} f${String(hour).padStart(3, "0")}`;
      return next;
    }

    async function applyLayer() {
      document.body.classList.toggle("compare", els.compare.checked);
      setTimeout(() => { mapA.invalidateSize(); mapB.invalidateSize(); }, 40);
      overlayA = await layerFor(els.runA.value, mapA, overlayA, els.badgeA);
      if (els.compare.checked) {
        overlayB = await layerFor(els.runB.value, mapB, overlayB, els.badgeB);
      } else if (overlayB) {
        mapB.removeLayer(overlayB);
        overlayB = null;
      }
      const palette = els.palette.value === "auto" ? defaultsFor(els.layer.value)[0] : els.palette.value;
      setStatus(`${els.model.value} ${els.layer.value} f${String(els.hour.value).padStart(3, "0")} | ${palette} ${els.min.value},${els.max.value} | A=${els.runA.value}${els.compare.checked ? " B=" + els.runB.value : ""}`);
    }

    els.model.addEventListener("change", () => {
      loadRunList();
      refreshLayerList().then(applyLayer).catch(err => setStatus(err.message));
    });
    els.runA.addEventListener("change", () => refreshLayerList().then(applyLayer).catch(err => setStatus(err.message)));
    els.runB.addEventListener("change", () => refreshHours().then(applyLayer).catch(err => setStatus(err.message)));
    els.layer.addEventListener("change", () => refreshHours().then(applyLayer).catch(err => setStatus(err.message)));
    els.compare.addEventListener("change", () => refreshHours().then(applyLayer).catch(err => setStatus(err.message)));
    els.apply.addEventListener("click", () => applyLayer().catch(err => setStatus(err.message)));
    els.usageStart.addEventListener("click", startUsageMonitor);
    els.usageStop.addEventListener("click", stopUsageMonitor);
    els.usageReset.addEventListener("click", () => {
      const wasActive = usage.active;
      stopUsageMonitor();
      resetUsageCounters();
      if (wasActive) startUsageMonitor();
    });
    for (const map of [mapA, mapB]) {
      map.on("moveend zoomend", updateUsageDisplay);
      map.on("mousemove", event => samplePicker(map, event.latlng));
      map.on("mouseout", () => {
        els.pickerValue.textContent = "move over map";
        els.pickerMeta.textContent = "Samples the selected layer/hour from the WxStore grid.";
      });
    }
    loadModelList().then(refreshLayerList).then(applyLayer).catch(err => setStatus(err.message));
  </script>
</body>
</html>
"#;

impl AppState {
    fn cache_get(&self, key: &str) -> Option<Bytes> {
        self.cache.read().ok()?.entries.get(key).cloned()
    }

    fn cache_insert(&self, key: String, value: Bytes) {
        let Ok(mut cache) = self.cache.write() else {
            return;
        };
        if !cache.entries.contains_key(&key) {
            cache.order.push_back(key.clone());
        }
        cache.entries.insert(key, value);
        while cache.entries.len() > CACHE_LIMIT {
            if let Some(oldest) = cache.order.pop_front() {
                cache.entries.remove(&oldest);
            } else {
                break;
            }
        }
    }
}

#[derive(Debug, Deserialize)]
struct PointQuery {
    lat: Option<f64>,
    lon: Option<f64>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    hours: Option<String>,
    forecast_hours: Option<String>,
    profile: Option<String>,
    profile_vars: Option<String>,
    variables: Option<String>,
    diagnostics: Option<String>,
    diag: Option<String>,
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModelRunQuery {
    model: String,
    run: Option<String>,
    member: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GridQuery {
    model: String,
    variable: String,
    forecast_hour: Option<u32>,
    run: Option<String>,
    member: Option<String>,
    members: Option<String>,
    format: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SampleQuery {
    model: String,
    variable: String,
    forecast_hour: Option<u32>,
    run: Option<String>,
    member: Option<String>,
    members: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    latitude: Option<f64>,
    longitude: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct WindFieldQuery {
    model: String,
    run: Option<String>,
    member: Option<String>,
    members: Option<String>,
    forecast_hour: Option<u32>,
    u: Option<String>,
    v: Option<String>,
    u_variable: Option<String>,
    v_variable: Option<String>,
    stride: Option<usize>,
    bounds: Option<String>,
    west: Option<f64>,
    south: Option<f64>,
    east: Option<f64>,
    north: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct LayerQuery {
    model: Option<String>,
    run: Option<String>,
    member: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TileJsonQuery {
    member: Option<String>,
    forecast_hour: Option<u32>,
    palette: Option<String>,
    min: Option<f32>,
    max: Option<f32>,
    transparent_below: Option<f32>,
    transparent_above: Option<f32>,
    alpha: Option<u8>,
    base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MapboxLayerQuery {
    member: Option<String>,
    hours: Option<String>,
    palette: Option<String>,
    range: Option<String>,
    transparent_below: Option<f32>,
    transparent_above: Option<f32>,
    alpha: Option<u8>,
    base_url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TileQuery {
    member: Option<String>,
    palette: Option<String>,
    min: Option<f32>,
    max: Option<f32>,
    transparent_below: Option<f32>,
    transparent_above: Option<f32>,
    alpha: Option<u8>,
}

#[derive(Debug, Deserialize)]
struct ForecastQuery {
    lat: Option<f64>,
    lon: Option<f64>,
    latitude: Option<f64>,
    longitude: Option<f64>,
    model: Option<String>,
    models: Option<String>,
    run: Option<String>,
    member: Option<String>,
    members: Option<String>,
    hourly: Option<String>,
    forecast_hours: Option<String>,
    hours: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CanonicalQuery {
    hours: Option<String>,
    forecast_hours: Option<String>,
    profile: Option<String>,
    profile_vars: Option<String>,
    variables: Option<String>,
    diagnostics: Option<String>,
    diag: Option<String>,
    format: Option<String>,
}

type ApiError = (StatusCode, Json<Value>);

async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(store_status(
        &state.profile,
        state.diagnostic.as_deref(),
        state.spatial.as_deref(),
    ))
}

async fn latest(
    State(state): State<Arc<AppState>>,
    AxumPath((model, domain)): AxumPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let manifest = &state.profile.manifest;
    if model != manifest.model || domain != manifest.domain {
        return Err(not_found("model/domain is not loaded on this node"));
    }
    Ok(Json(json!({
        "schema": "wxstore.latest.v1",
        "model": manifest.model,
        "domain": manifest.domain,
        "run_id": manifest.run_id,
        "cycle": manifest.cycle,
        "products": {
            "profile_pressure_core": "ready",
            "diag_scalar_basic": if state.diagnostic.is_some() { "ready_sparse_v0" } else { "unavailable" },
            "surface_spatial": if state.spatial.is_some() { "ready" } else { "unavailable" }
        },
        "canonical_run_url": format!("/v1/runs/{}/{}/{}", manifest.model, manifest.domain, manifest.run_id),
    })))
}

async fn models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let spatial = state.spatial.as_deref().map(SpatialLane::models_json);
    Json(json!({
        "schema": "wxstore.models.v1",
        "profile_loaded": {
            "model": state.profile.manifest.model,
            "domain": state.profile.manifest.domain,
            "run_id": state.profile.manifest.run_id,
            "products": ["temporal_sounding", "point_bin"]
        },
        "spatial_loaded": spatial.unwrap_or_else(|| json!({"status": "unavailable"})),
        "science_engine_scope": {
            "current_profile_lane": ["hrrr"],
            "current_spatial_surface_lanes": state.spatial.as_deref().map(SpatialLane::model_ids).unwrap_or_default(),
            "designed_for": ["hrrr", "gfs", "nam", "rap", "rrfs", "ecmwf_ifs", "ecmwf_ens", "ai_model_outputs"]
        }
    }))
}

async fn variables(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ModelRunQuery>,
) -> Result<Json<Value>, ApiError> {
    let Some(spatial) = state.spatial.as_deref() else {
        return Err(not_found("spatial lane is not configured"));
    };
    let run = spatial.resolve_run(&query.model, query.run.as_deref())?;
    let member = query.member.as_deref();
    Ok(Json(spatial.variables_json(&query.model, &run, member)?))
}

async fn products(State(state): State<Arc<AppState>>) -> Json<Value> {
    let local_inventory =
        fs::read("rustwx-inventory/rustwx_hrrr_20260429_f000_capability_inventory.json")
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    Json(json!({
        "schema": "wxstore.products.v1",
        "service_products": {
            "raw_grid_variables": state.spatial.as_deref().map(SpatialLane::models_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "virtual_grid_products": {
                "direct_aliases": [
                    "2m_temperature",
                    "2m_dewpoint",
                    "2m_relative_humidity",
                    "10m_wind_gusts",
                    "total_qpf",
                    "mslp_10m_winds",
                    "visibility",
                    "sbcape"
                ],
                "cheap_derived": [
                    "dewpoint_depression_2m",
                    "vpd_2m",
                    "heat_index_2m",
                    "wind_chill_2m",
                    "apparent_temperature_2m",
                    "wind_speed_10m",
                    "wind_direction_10m"
                ],
                "windowed_patterns": [
                    "2m_temp_0_24h_max",
                    "2m_temp_0_24h_min",
                    "2m_temp_0_24h_range",
                    "2m_temp_0_48h_max",
                    "2m_dewpoint_0_24h_max",
                    "2m_rh_0_24h_min",
                    "10m_wind_0_24h_max"
                ]
            },
            "temporal_sounding": {
                "model": state.profile.manifest.model,
                "run_id": state.profile.manifest.run_id,
                "variables": state.profile.variable_names(),
                "hours": state.profile.manifest.forecast_hours,
                "levels_hpa": state.profile.manifest.levels_hpa
            }
        },
        "rustwx_hrrr_inventory": local_inventory.unwrap_or_else(|| json!({
            "status": "not_generated",
            "command": "cargo run --release -p rustwx-cli --bin hrrr_capability_inventory -- --date 20260429 --forecast-hour 0 --out-dir C:\\\\Users\\\\drew\\\\wxstore\\\\rustwx-inventory"
        }))
    }))
}

fn read_grid_from_state(
    state: &AppState,
    model: &str,
    run: Option<&str>,
    member: Option<&str>,
    variable: &str,
    forecast_hour: u32,
) -> Result<SpatialGrid> {
    let mut spatial_error = None::<String>;
    if let Some(spatial) = state.spatial.as_deref() {
        match spatial.resolve_run(model, run) {
            Ok(resolved_run) => {
                match spatial.read_grid(model, &resolved_run, member, variable, forecast_hour) {
                    Ok(grid) => return Ok(grid),
                    Err(err) => spatial_error = Some(err.to_string()),
                }
            }
            Err((_, body)) => {
                spatial_error = Some(
                    body.0
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("spatial run is not available")
                        .to_string(),
                );
            }
        }
    }

    let profile = &state.profile;
    let profile_run_requested = run
        .map(|value| {
            value == "latest" || value == profile.manifest.run_id || value == profile.manifest.cycle
        })
        .unwrap_or(true);
    if model == profile.manifest.model && profile_run_requested && member.is_none() {
        if let Some(grid) = profile.read_pressure_grid_product(variable, forecast_hour)? {
            return Ok(grid);
        }
    }

    if let Some(err) = spatial_error {
        bail!("{err}");
    }
    bail!("grid product '{variable}' is not available for {model}");
}

async fn grid_field(
    State(state): State<Arc<AppState>>,
    Query(query): Query<GridQuery>,
) -> Result<Response, ApiError> {
    let format = query.format.as_deref().unwrap_or("json");
    let model = query.model;
    let variable = query.variable;
    let member = query.member.or(query.members.and_then(|value| {
        value
            .split(',')
            .next()
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
    }));
    let hour = query.forecast_hour.unwrap_or(0);
    let requested_run = query.run;
    let state_for_read = state.clone();
    let read = tokio::task::spawn_blocking(move || {
        read_grid_from_state(
            &state_for_read,
            &model,
            requested_run.as_deref(),
            member.as_deref(),
            &variable,
            hour,
        )
    })
    .await
    .map_err(|err| internal_error(format!("join error: {err}")))?
    .map_err(|err| bad_request(err.to_string()))?;

    if format == "bin" {
        let mut body = Vec::with_capacity(read.values.len() * 4);
        for value in read.values.iter() {
            body.extend_from_slice(&value.to_le_bytes());
        }
        let mut response = Bytes::from(body).into_response();
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/octet-stream"),
        );
        insert_header(headers, "x-wxstore-model", &read.model);
        insert_header(headers, "x-wxstore-run-id", &read.run_id);
        insert_header(headers, "x-wxstore-variable", &read.variable);
        insert_header(headers, "x-wxstore-units", &read.units);
        insert_header(headers, "x-wxstore-nx", &read.nx.to_string());
        insert_header(headers, "x-wxstore-ny", &read.ny.to_string());
        insert_header(
            headers,
            "x-wxstore-forecast-hour",
            &read.forecast_hour.to_string(),
        );
        if let Some(member) = &read.member {
            insert_header(headers, "x-wxstore-member", member);
        }
        headers.insert(
            "access-control-expose-headers",
            HeaderValue::from_static("x-wxstore-model,x-wxstore-run-id,x-wxstore-variable,x-wxstore-units,x-wxstore-nx,x-wxstore-ny,x-wxstore-forecast-hour,x-wxstore-member"),
        );
        return Ok(response);
    }

    Ok(Json(json!({
        "schema": "wxstore.grid.v1",
        "model": read.model,
        "run_id": read.run_id,
        "member": read.member,
        "variable": read.variable,
        "units": read.units,
        "forecast_hour": read.forecast_hour,
        "nx": read.nx,
        "ny": read.ny,
        "grid": read.grid_meta(),
        "data": read.values.as_ref()
    }))
    .into_response())
}

async fn sample_point(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SampleQuery>,
) -> Result<Json<Value>, ApiError> {
    let lat = query
        .lat
        .or(query.latitude)
        .ok_or_else(|| bad_request("lat/latitude is required"))?;
    let lon = query
        .lon
        .or(query.longitude)
        .ok_or_else(|| bad_request("lon/longitude is required"))?;
    let model = query.model;
    let variable = query.variable;
    let member = query.member.or(query.members.and_then(|value| {
        value
            .split(',')
            .next()
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
    }));
    let hour = query.forecast_hour.unwrap_or(0);
    let requested_run = query.run;
    let state_for_read = state.clone();
    let grid = tokio::task::spawn_blocking(move || {
        read_grid_from_state(
            &state_for_read,
            &model,
            requested_run.as_deref(),
            member.as_deref(),
            &variable,
            hour,
        )
    })
    .await
    .map_err(|err| internal_error(format!("join error: {err}")))?
    .map_err(|err| bad_request(err.to_string()))?;

    let Some(index) = grid_index_for_latlon(&grid, lat, lon) else {
        return Ok(Json(json!({
            "schema": "wxstore.sample.v1",
            "in_domain": false,
            "requested": {"lat": lat, "lon": lon},
            "model": grid.model,
            "run_id": grid.run_id,
            "member": grid.member,
            "variable": grid.variable,
            "units": grid.units,
            "forecast_hour": grid.forecast_hour,
            "bounds": grid_bounds(&grid)
        })));
    };
    let value = grid.values.get(index).copied().unwrap_or(f32::NAN);
    let value_json = if value.is_finite() {
        json!(f64::from(value))
    } else {
        Value::Null
    };
    Ok(Json(json!({
        "schema": "wxstore.sample.v1",
        "in_domain": true,
        "requested": {"lat": lat, "lon": lon},
        "model": grid.model,
        "run_id": grid.run_id,
        "member": grid.member,
        "variable": grid.variable,
        "units": grid.units,
        "forecast_hour": grid.forecast_hour,
        "value": value_json,
        "grid": {
            "x": index % grid.nx,
            "y": index / grid.nx,
            "index": index,
            "sample": "nearest"
        },
        "bounds": grid_bounds(&grid)
    })))
}

async fn wind_field(
    State(state): State<Arc<AppState>>,
    Query(query): Query<WindFieldQuery>,
) -> Result<Json<Value>, ApiError> {
    let requested_bounds = wind_query_bounds(&query)?;
    let model = query.model;
    let run = query.run.unwrap_or_else(|| "latest".to_string());
    let member = query.member.or(query.members.and_then(|value| {
        value
            .split(',')
            .next()
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
    }));
    let hour = query.forecast_hour.unwrap_or(0);
    let u_variable = query
        .u_variable
        .or(query.u)
        .unwrap_or_else(|| "wind_u_10m_ms".to_string());
    let v_variable = query
        .v_variable
        .or(query.v)
        .unwrap_or_else(|| "wind_v_10m_ms".to_string());
    let stride = query.stride.unwrap_or(10).clamp(3, 80);
    let state_for_read = state.clone();
    let model_for_read = model.clone();
    let run_for_read = run.clone();
    let member_for_read = member.clone();
    let u_for_read = u_variable.clone();
    let v_for_read = v_variable.clone();
    let (u_grid, v_grid) = tokio::task::spawn_blocking(move || {
        let u_grid = read_grid_from_state(
            &state_for_read,
            &model_for_read,
            Some(&run_for_read),
            member_for_read.as_deref(),
            &u_for_read,
            hour,
        )?;
        let v_grid = read_grid_from_state(
            &state_for_read,
            &model_for_read,
            Some(&run_for_read),
            member_for_read.as_deref(),
            &v_for_read,
            hour,
        )?;
        Ok::<_, anyhow::Error>((u_grid, v_grid))
    })
    .await
    .map_err(|err| internal_error(format!("join error: {err}")))?
    .map_err(|err| bad_request(err.to_string()))?;

    if u_grid.nx != v_grid.nx || u_grid.ny != v_grid.ny {
        return Err(bad_request(
            "wind U/V grids do not have matching dimensions",
        ));
    }
    let bounds = requested_bounds.unwrap_or_else(|| grid_bounds(&u_grid));
    let mut vectors = Vec::<Value>::new();
    for y in (0..u_grid.ny).step_by(stride) {
        for x in (0..u_grid.nx).step_by(stride) {
            let index = y * u_grid.nx + x;
            let Some((&u_value, &v_value)) = u_grid.values.get(index).zip(v_grid.values.get(index))
            else {
                continue;
            };
            if !u_value.is_finite() || !v_value.is_finite() {
                continue;
            }
            let (lat, lon) = grid_latlon_at(&u_grid, x, y);
            if lon < bounds[0] || lon > bounds[2] || lat < bounds[1] || lat > bounds[3] {
                continue;
            }
            let speed_ms = ((u_value * u_value + v_value * v_value) as f64).sqrt();
            vectors.push(json!({
                "x": x,
                "y": y,
                "lat": lat,
                "lon": lon,
                "u": u_value,
                "v": v_value,
                "speed_ms": speed_ms,
                "speed_kt": speed_ms * 1.943_844_5
            }));
        }
    }

    Ok(Json(json!({
        "schema": "wxstore.wind_field.v1",
        "model": u_grid.model,
        "run_id": u_grid.run_id,
        "member": u_grid.member,
        "forecast_hour": u_grid.forecast_hour,
        "u_variable": u_variable,
        "v_variable": v_variable,
        "units": "m/s",
        "speed_units": "m/s",
        "nx": u_grid.nx,
        "ny": u_grid.ny,
        "stride": stride,
        "bounds": bounds,
        "grid": u_grid.grid_meta(),
        "vector_count": vectors.len(),
        "vectors": vectors
    })))
}

async fn layers(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LayerQuery>,
) -> Json<Value> {
    let model = query.model.unwrap_or_else(|| "hrrr".to_string());
    let run = query.run.unwrap_or_else(|| "latest".to_string());
    let mut layers = Vec::<Value>::new();
    if let Some(spatial) = state.spatial.as_deref() {
        if let Ok(resolved_run) = spatial.resolve_run(&model, Some(&run)) {
            if let Ok(variables) =
                spatial.variables_for(&model, &resolved_run, query.member.as_deref())
            {
                for variable in variables {
                    let hours = spatial
                        .available_hours_for(
                            &model,
                            &resolved_run,
                            query.member.as_deref(),
                            &variable,
                        )
                        .unwrap_or_default();
                    layers.push(json!({
                        "id": variable,
                        "kind": "raster_grid",
                        "source": "wxa_or_spatial_adapter",
                        "model": model,
                        "run": resolved_run,
                        "member": query.member,
                        "forecast_hours": hours,
                        "tilejson": format!("/v1/tilejson/{}/{}/{}", model, resolved_run, variable)
                    }));
                }
            }
        }
    }
    if model == state.profile.manifest.model
        && (run == "latest" || run == state.profile.manifest.run_id)
    {
        for level in [1000u16, 925, 850, 700, 500, 300, 250, 200] {
            for suffix in [
                "temperature",
                "height",
                "wind_speed",
                "rh",
                "dewpoint",
                "specific_humidity",
            ] {
                let variable = format!("{level}mb_{suffix}");
                layers.push(json!({
                    "id": variable,
                    "kind": "raster_grid",
                    "source": "profile_pressure_core",
                    "model": state.profile.manifest.model,
                    "run": state.profile.manifest.run_id,
                    "forecast_hours": state.profile.manifest.forecast_hours,
                    "pressure_hpa": level,
                    "tilejson": format!("/v1/tilejson/{}/{}/{}", state.profile.manifest.model, state.profile.manifest.run_id, variable)
                }));
            }
        }
    }
    Json(json!({
        "schema": "wxstore.layers.v1",
        "model": model,
        "run": run,
        "mapbox": {
            "tile_template": "/v1/tiles/{model}/{run}/{variable}/{forecast_hour}/{z}/{x}/{y}.png",
            "tilejson_template": "/v1/tilejson/{model}/{run}/{variable}?forecast_hour={forecast_hour}"
        },
        "layers": layers
    }))
}

async fn tilejson(
    State(_state): State<Arc<AppState>>,
    AxumPath((model, run, variable)): AxumPath<(String, String, String)>,
    Query(query): Query<TileJsonQuery>,
) -> Json<Value> {
    let base = query
        .base_url
        .unwrap_or_else(|| "http://127.0.0.1:8897".to_string())
        .trim_end_matches('/')
        .to_string();
    let hour = query.forecast_hour.unwrap_or(0);
    let mut tile_url =
        format!("{base}/v1/tiles/{model}/{run}/{variable}/{hour}/{{z}}/{{x}}/{{y}}.png");
    let mut params = Vec::new();
    if let Some(member) = query.member {
        params.push(format!("member={member}"));
    }
    if let Some(palette) = query.palette {
        params.push(format!("palette={palette}"));
    }
    if let Some(min) = query.min {
        params.push(format!("min={min}"));
    }
    if let Some(max) = query.max {
        params.push(format!("max={max}"));
    }
    if let Some(transparent_below) = query.transparent_below {
        params.push(format!("transparent_below={transparent_below}"));
    }
    if let Some(transparent_above) = query.transparent_above {
        params.push(format!("transparent_above={transparent_above}"));
    }
    if let Some(alpha) = query.alpha {
        params.push(format!("alpha={alpha}"));
    }
    if !params.is_empty() {
        tile_url.push('?');
        tile_url.push_str(&params.join("&"));
    }
    Json(json!({
        "tilejson": "3.0.0",
        "name": format!("{model}/{run}/{variable}"),
        "scheme": "xyz",
        "tiles": [tile_url],
        "minzoom": 0,
        "maxzoom": 9,
        "bounds": [-180.0, -85.05112878, 180.0, 85.05112878],
        "wxstore": {
            "schema": "wxstore.mapbox_layer.v1",
            "model": model,
            "run": run,
            "variable": variable,
            "forecast_hour": hour,
            "temporal_tile_template": format!("{base}/v1/tiles/{model}/{run}/{variable}/{{forecast_hour}}/{{z}}/{{x}}/{{y}}.png")
        }
    }))
}

async fn raster_tile(
    State(state): State<Arc<AppState>>,
    AxumPath((model, run, variable, forecast_hour, z, x, y)): AxumPath<(
        String,
        String,
        String,
        u32,
        u32,
        u32,
        String,
    )>,
    Query(query): Query<TileQuery>,
) -> Result<Response, ApiError> {
    let y = parse_tile_y(&y).map_err(bad_anyhow)?;
    let transparent_below = query
        .transparent_below
        .or_else(|| default_transparent_below_for_variable(&variable));
    let transparent_above = query
        .transparent_above
        .or_else(|| default_transparent_above_for_variable(&variable));
    let alpha = query.alpha.unwrap_or(210);
    let cache_key = format!(
        "tile:v2:{model}:{run}:{member}:{variable}:f{forecast_hour:03}:{z}:{x}:{y}:{palette}:{min:?}:{max:?}:{transparent_below:?}:{transparent_above:?}:{alpha:?}",
        member = query.member.as_deref().unwrap_or(""),
        palette = query.palette.as_deref().unwrap_or(""),
        min = query.min,
        max = query.max,
    );
    if let Some(png) = state.cache_get(&cache_key) {
        return Ok(png_tile_response(png));
    }
    let member = query.member.clone();
    let state_for_read = state.clone();
    let grid = tokio::task::spawn_blocking(move || {
        read_grid_from_state(
            &state_for_read,
            &model,
            Some(&run),
            member.as_deref(),
            &variable,
            forecast_hour,
        )
    })
    .await
    .map_err(|err| internal_error(format!("join error: {err}")))?
    .map_err(|err| bad_request(err.to_string()))?;
    let query = TileQuery {
        transparent_below,
        transparent_above,
        alpha: Some(alpha),
        ..query
    };
    let png = tokio::task::spawn_blocking(move || render_raster_tile_png(&grid, z, x, y, &query))
        .await
        .map_err(|err| internal_error(format!("join error: {err}")))?
        .map_err(|err| bad_request(err.to_string()))?;
    let png = Bytes::from(png);
    state.cache_insert(cache_key, png.clone());
    Ok(png_tile_response(png))
}

fn png_tile_response(png: Bytes) -> Response {
    let mut response = png.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=31536000, immutable"),
    );
    response
}

async fn mapbox_layer(
    State(state): State<Arc<AppState>>,
    AxumPath((model, run, variable)): AxumPath<(String, String, String)>,
    Query(query): Query<MapboxLayerQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = query
        .base_url
        .unwrap_or_else(|| "http://127.0.0.1:8897".to_string())
        .trim_end_matches('/')
        .to_string();
    let (min, max) = query
        .range
        .as_deref()
        .and_then(parse_range_pair)
        .unwrap_or_else(|| default_range_for_variable(&variable, &[]));
    let palette = query
        .palette
        .unwrap_or_else(|| default_palette_for_variable(&variable).to_string());
    let transparent_below = query
        .transparent_below
        .or_else(|| default_transparent_below_for_variable(&variable));
    let transparent_above = query
        .transparent_above
        .or_else(|| default_transparent_above_for_variable(&variable));
    let alpha = query.alpha.unwrap_or(210);
    let hours = available_hours_for_layer(&state, &model, &run, query.member.as_deref(), &variable)
        .map_err(bad_anyhow)?;
    let requested_hours = query
        .hours
        .as_deref()
        .map(parse_hours_u32)
        .transpose()
        .map_err(bad_anyhow)?;
    let hours = if let Some(requested) = requested_hours {
        hours
            .into_iter()
            .filter(|hour| requested.contains(hour))
            .collect::<Vec<_>>()
    } else {
        hours
    };
    let bounds = hours
        .first()
        .and_then(|hour| {
            state.spatial.as_ref().and_then(|spatial| {
                spatial
                    .read_grid(&model, &run, query.member.as_deref(), &variable, *hour)
                    .ok()
            })
        })
        .map(|grid| grid_bounds(&grid))
        .unwrap_or([-180.0, -85.05112878, 180.0, 85.05112878]);
    let frames = hours
        .iter()
        .map(|hour| {
            let frame = format!("f{hour:03}");
            let mut tile = format!(
                "{base}/v1/mapbox/tiles/{model}/{run}/{variable}/{frame}/{{z}}/{{x}}/{{y}}?palette={palette}&range={min},{max}"
            );
            let mut tilejson_url =
                format!("{base}/v1/mapbox/tilejson/{model}/{run}/{variable}/{frame}?palette={palette}&range={min},{max}");
            if let Some(member) = query.member.as_deref() {
                tile.push_str("&member=");
                tile.push_str(member);
                tilejson_url.push_str("&member=");
                tilejson_url.push_str(member);
            }
            if let Some(transparent_below) = transparent_below {
                tile.push_str("&transparent_below=");
                tile.push_str(&transparent_below.to_string());
                tilejson_url.push_str("&transparent_below=");
                tilejson_url.push_str(&transparent_below.to_string());
            }
            if let Some(transparent_above) = transparent_above {
                tile.push_str("&transparent_above=");
                tile.push_str(&transparent_above.to_string());
                tilejson_url.push_str("&transparent_above=");
                tilejson_url.push_str(&transparent_above.to_string());
            }
            tile.push_str("&alpha=");
            tile.push_str(&alpha.to_string());
            tilejson_url.push_str("&alpha=");
            tilejson_url.push_str(&alpha.to_string());
            json!({
                "forecast_hour": hour,
                "frame": frame,
                "tiles": [tile],
                "tilejson_url": tilejson_url
            })
        })
        .collect::<Vec<_>>();
    Ok(Json(json!({
        "schema": "wxstore.mapbox.layer.v1",
        "model": model,
        "run_id": run,
        "variable": variable,
        "bounds": bounds,
        "minzoom": 0,
        "maxzoom": 9,
        "tile_size": 256,
        "palette": {"id": palette, "range": [min, max]},
        "rendering": {
            "alpha": alpha,
            "transparent_below": transparent_below,
            "transparent_above": transparent_above
        },
        "frames": frames
    })))
}

async fn mapbox_tilejson(
    State(state): State<Arc<AppState>>,
    AxumPath((model, run, variable, frame)): AxumPath<(String, String, String, String)>,
    Query(query): Query<MapboxLayerQuery>,
) -> Result<Json<Value>, ApiError> {
    let forecast_hour = parse_frame_hour(&frame).map_err(bad_anyhow)?;
    let (min, max) = query
        .range
        .as_deref()
        .and_then(parse_range_pair)
        .unwrap_or_else(|| default_range_for_variable(&variable, &[]));
    Ok(tilejson(
        State(state),
        AxumPath((model, run, variable)),
        Query(TileJsonQuery {
            member: query.member,
            forecast_hour: Some(forecast_hour),
            palette: query.palette,
            min: Some(min),
            max: Some(max),
            transparent_below: query.transparent_below,
            transparent_above: query.transparent_above,
            alpha: query.alpha,
            base_url: query.base_url,
        }),
    )
    .await)
}

async fn mapbox_raster_tile(
    State(state): State<Arc<AppState>>,
    AxumPath((model, run, variable, frame, z, x, y)): AxumPath<(
        String,
        String,
        String,
        String,
        u32,
        u32,
        String,
    )>,
    Query(query): Query<MapboxLayerQuery>,
) -> Result<Response, ApiError> {
    let forecast_hour = parse_frame_hour(&frame).map_err(bad_anyhow)?;
    let (min, max) = query
        .range
        .as_deref()
        .and_then(parse_range_pair)
        .unwrap_or_else(|| default_range_for_variable(&variable, &[]));
    raster_tile(
        State(state),
        AxumPath((model, run, variable, forecast_hour, z, x, y)),
        Query(TileQuery {
            member: query.member,
            palette: query.palette,
            min: Some(min),
            max: Some(max),
            transparent_below: query.transparent_below,
            transparent_above: query.transparent_above,
            alpha: query.alpha,
        }),
    )
    .await
}

async fn forecast(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ForecastQuery>,
) -> Result<Json<Value>, ApiError> {
    let Some(spatial) = state.spatial.clone() else {
        return Err(not_found("spatial lane is not configured"));
    };
    let lat = query
        .lat
        .or(query.latitude)
        .ok_or_else(|| bad_request("latitude is required"))?;
    let lon = query
        .lon
        .or(query.longitude)
        .ok_or_else(|| bad_request("longitude is required"))?;
    let model = query
        .model
        .or(query.models)
        .unwrap_or_else(|| "hrrr".to_string());
    let run = spatial.resolve_run(&model, query.run.as_deref())?;
    let member = query.member.or(query.members.and_then(|value| {
        value
            .split(',')
            .next()
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
    }));
    let variables = split_csv(
        query
            .hourly
            .as_deref()
            .unwrap_or("temperature_2m,dew_point_2m,wind_gusts_10m"),
    );
    if variables.is_empty() {
        return Err(bad_request("hourly must list at least one variable"));
    }
    let hours = query
        .forecast_hours
        .or(query.hours)
        .map(|value| parse_hours_u32(&value))
        .transpose()
        .map_err(bad_anyhow)?
        .unwrap_or_else(|| vec![0, 1, 2]);

    let read = tokio::task::spawn_blocking(move || {
        spatial.forecast_point(
            &model,
            &run,
            member.as_deref(),
            lat,
            lon,
            &variables,
            &hours,
        )
    })
    .await
    .map_err(|err| internal_error(format!("join error: {err}")))?
    .map_err(|err| bad_request(err.to_string()))?;
    Ok(Json(read))
}

async fn resolve(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PointQuery>,
) -> Result<Json<Value>, ApiError> {
    let lat = query_lat(&query)?;
    let lon = query_lon(&query)?;
    let point = state.profile.locate_nearest(lat, lon).map_err(bad_anyhow)?;
    let manifest = &state.profile.manifest;
    Ok(Json(json!({
        "schema": "wxstore.resolve.v1",
        "model": manifest.model,
        "domain": manifest.domain,
        "run_id": manifest.run_id,
        "cycle": manifest.cycle,
        "requested": {"lat": lat, "lon": lon},
        "grid": point,
        "canonical_url": format!(
            "/v1/runs/{}/{}/{}/grid/{}/{}/temporal-sounding",
            manifest.model, manifest.domain, manifest.run_id, point.x, point.y
        ),
    })))
}

async fn temporal_sounding(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<PointQuery>,
) -> Result<Response, ApiError> {
    let lat = query_lat(&query)?;
    let lon = query_lon(&query)?;
    let point = state.profile.locate_nearest(lat, lon).map_err(bad_anyhow)?;
    let req = RequestShape::from_point_query(&state.profile, &query).map_err(bad_anyhow)?;
    json_response_for_point(state, point, lat, lon, req, headers, false).await
}

async fn point_bin(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PointQuery>,
) -> Result<Response, ApiError> {
    let lat = query_lat(&query)?;
    let lon = query_lon(&query)?;
    let point = state.profile.locate_nearest(lat, lon).map_err(bad_anyhow)?;
    let mut req = RequestShape::from_point_query(&state.profile, &query).map_err(bad_anyhow)?;
    req.response_format = ResponseFormat::WxBin;
    binary_response_for_point(state, point, lat, lon, req, false).await
}

async fn canonical_temporal_sounding(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath((model, domain, run, x, y)): AxumPath<(String, String, String, usize, usize)>,
    Query(query): Query<CanonicalQuery>,
) -> Result<Response, ApiError> {
    validate_canonical(&state.profile, &model, &domain, &run)?;
    let point = state.profile.grid_point(x, y).map_err(bad_anyhow)?;
    let req = RequestShape::from_canonical_query(&state.profile, &query).map_err(bad_anyhow)?;
    json_response_for_point(state, point, point.lat, point.lon, req, headers, true).await
}

async fn canonical_point_bin(
    State(state): State<Arc<AppState>>,
    AxumPath((model, domain, run, x, y)): AxumPath<(String, String, String, usize, usize)>,
    Query(query): Query<CanonicalQuery>,
) -> Result<Response, ApiError> {
    validate_canonical(&state.profile, &model, &domain, &run)?;
    let point = state.profile.grid_point(x, y).map_err(bad_anyhow)?;
    let mut req = RequestShape::from_canonical_query(&state.profile, &query).map_err(bad_anyhow)?;
    req.response_format = ResponseFormat::WxBin;
    binary_response_for_point(state, point, point.lat, point.lon, req, true).await
}

async fn json_response_for_point(
    state: Arc<AppState>,
    point: GridPoint,
    requested_lat: f64,
    requested_lon: f64,
    req: RequestShape,
    headers: HeaderMap,
    immutable: bool,
) -> Result<Response, ApiError> {
    if req.response_format == ResponseFormat::WxBin {
        return binary_response_for_point(
            state,
            point,
            requested_lat,
            requested_lon,
            req,
            immutable,
        )
        .await;
    }
    let key = cache_key(&state.profile.manifest.run_id, &point, &req, "json");
    if immutable {
        if let Some(bytes) = state.cache_get(&key) {
            return Ok(bytes_response(bytes, "application/json", true, immutable));
        }
    }

    let profile = state.profile.clone();
    let diagnostic = state.diagnostic.clone();
    let started = Instant::now();
    let body = tokio::task::spawn_blocking(move || {
        build_json_body(
            &profile,
            diagnostic.as_deref(),
            point,
            requested_lat,
            requested_lon,
            &req,
            started,
        )
    })
    .await
    .map_err(|err| internal_error(&format!("join error: {err}")))?
    .map_err(|err| bad_request(&err.to_string()))?;
    let bytes = Bytes::from(
        serde_json::to_vec(&body).map_err(|err| internal_error(&format!("encode JSON: {err}")))?,
    );
    if immutable {
        state.cache_insert(key, bytes.clone());
    }
    let cache_hit = headers
        .get("x-wxstore-repeat")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == "1");
    Ok(bytes_response(
        bytes,
        "application/json",
        cache_hit,
        immutable,
    ))
}

async fn binary_response_for_point(
    state: Arc<AppState>,
    point: GridPoint,
    requested_lat: f64,
    requested_lon: f64,
    req: RequestShape,
    immutable: bool,
) -> Result<Response, ApiError> {
    let key = cache_key(&state.profile.manifest.run_id, &point, &req, "wxbin");
    if immutable {
        if let Some(bytes) = state.cache_get(&key) {
            return Ok(bytes_response(
                bytes,
                "application/vnd.wxstore.point.v1+bin",
                true,
                immutable,
            ));
        }
    }
    let profile = state.profile.clone();
    let diagnostic = state.diagnostic.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        build_binary_body(
            &profile,
            diagnostic.as_deref(),
            point,
            requested_lat,
            requested_lon,
            &req,
        )
    })
    .await
    .map_err(|err| internal_error(&format!("join error: {err}")))?
    .map_err(|err| bad_request(&err.to_string()))?;
    if immutable {
        state.cache_insert(key, bytes.clone());
    }
    Ok(bytes_response(
        bytes,
        "application/vnd.wxstore.point.v1+bin",
        false,
        immutable,
    ))
}

fn store_status(
    profile: &ProfileLane,
    diagnostic: Option<&DiagnosticLane>,
    spatial: Option<&SpatialLane>,
) -> Value {
    json!({
        "schema": "wxstore.status.v1",
        "service": "wxstore",
        "ok": true,
        "loaded_run": run_manifest_json(profile, diagnostic, spatial),
        "lanes": {
            "profile_pressure_core": profile.lane_manifest_json(),
            "diag_scalar_basic": diagnostic.map(DiagnosticLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "surface_spatial": spatial.map(SpatialLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"}))
        },
        "cache": {"entries_limit": CACHE_LIMIT}
    })
}

fn run_manifest_json(
    profile: &ProfileLane,
    diagnostic: Option<&DiagnosticLane>,
    spatial: Option<&SpatialLane>,
) -> Value {
    let manifest = &profile.manifest;
    json!({
        "schema": "wxstore.run.v1",
        "store_version": "1.0.0",
        "model": manifest.model,
        "domain": manifest.domain,
        "cycle_time_utc": manifest.cycle,
        "run_id": manifest.run_id,
        "grid": {
            "grid_id": format!("{}.{}.grid", manifest.model, manifest.domain),
            "nx": manifest.nx,
            "ny": manifest.ny,
            "projection": if manifest.model == "hrrr" && manifest.nx == 1799 && manifest.ny == 1059 { "lambert_conformal" } else { "unknown" }
        },
        "lead_time": {
            "units": "seconds",
            "values": manifest.forecast_hours.iter().map(|hour| u32::from(*hour) * 3600).collect::<Vec<_>>(),
            "forecast_hour_labels": manifest.forecast_hours.iter().map(|hour| format!("f{hour:03}")).collect::<Vec<_>>()
        },
        "members": [{"id": "control", "index": 0, "kind": "deterministic"}],
        "vertical_axes": {
            "pressure_hpa": manifest.levels_hpa
        },
        "lanes": [
            {"id": "profile_pressure_core", "status": "complete"},
            {"id": "diag_scalar_basic", "status": if diagnostic.is_some() { "ready_sparse_v0" } else { "unavailable" }},
            {"id": "surface_spatial", "status": if spatial.is_some() { "ready" } else { "unavailable" }}
        ],
        "provenance": {
            "builder": "wxstore v0 from custom .wxp lane",
            "source": "rustwx-generated model lanes and local model-run stores",
            "notes": [
                "No Open-Meteo file format or code is used by this service.",
                "Diagnostic lane currently wraps a sparse precomputed diagnostic brick until dense diagnostics are built.",
                "Spatial lane serves native WXA dense2d products and falls back to local model-run Zarr arrays only when a WXA product is unavailable."
            ]
        }
    })
}

fn build_json_body(
    profile: &ProfileLane,
    diagnostic: Option<&DiagnosticLane>,
    point: GridPoint,
    requested_lat: f64,
    requested_lon: f64,
    req: &RequestShape,
    started: Instant,
) -> Result<Value> {
    let mut profile_fields = Map::new();
    let mut field_values = HashMap::<String, Vec<Option<f64>>>::new();
    for variable in &req.profile_variables {
        let series = profile.read_variable_point(variable, &req.hours, &point)?;
        let values = series
            .values
            .iter()
            .map(|value| {
                if *value == MISSING_I16 {
                    None
                } else {
                    Some(
                        f64::from(*value) / f64::from(series.scale_factor)
                            - f64::from(series.add_offset),
                    )
                }
            })
            .collect::<Vec<_>>();
        field_values.insert(variable.clone(), values.clone());
        let (key, units) = profile_output(variable);
        profile_fields.insert(
            key.to_string(),
            json!({
                "dims": ["time", "pressure_hpa"],
                "units": units,
                "source_variable": variable,
                "encoding": {
                    "kind": "i16_linear",
                    "scale_factor": series.scale_factor,
                    "add_offset": series.add_offset,
                    "missing": MISSING_I16
                },
                "values": values
            }),
        );
    }
    if let (Some(temp), Some(q)) = (field_values.get("TMP"), field_values.get("SPFH")) {
        let mut values = Vec::with_capacity(temp.len());
        for (index, temp_value) in temp.iter().enumerate() {
            let level =
                profile.manifest.levels_hpa[index % profile.manifest.levels_hpa.len()] as f64;
            values.push(match (temp_value, q.get(index).and_then(|value| *value)) {
                (Some(_), Some(q_kg_kg)) => specific_humidity_to_dewpoint_c(q_kg_kg, level),
                _ => None,
            });
        }
        profile_fields.insert(
            "dewpoint_c".to_string(),
            json!({
                "dims": ["time", "pressure_hpa"],
                "units": "degC",
                "source_variable": "TMP+SPFH",
                "derivation": "wxstore.q_to_dewpoint.v1",
                "values": values
            }),
        );
    }

    let valid_times = valid_times_from_cycle(&profile.manifest.cycle, &req.hours);
    let diagnostics = if req.diagnostic_mode == DiagnosticMode::None {
        None
    } else {
        Some(diagnostic_json(
            diagnostic,
            point,
            &req.hours,
            req.diagnostic_mode,
        )?)
    };
    let mut body = json!({
        "schema": "wxstore.temporal_sounding.v1",
        "model": profile.manifest.model,
        "domain": profile.manifest.domain,
        "run": {
            "cycle": profile.manifest.cycle,
            "run_id": profile.manifest.run_id,
            "forecast_hours": req.hours,
            "valid_times": valid_times
        },
        "site": {
            "requested": {"lat": requested_lat, "lon": requested_lon},
            "grid": point,
        },
        "axes": {
            "forecast_hour": req.hours,
            "valid_time": valid_times,
            "pressure_hpa": profile.manifest.levels_hpa
        },
        "profile": {
            "lane": "profile_pressure_core",
            "layout": "field_keyed_arrays",
            "dims": ["time", "pressure_hpa"],
            "fields": profile_fields
        },
        "coverage": {
            "profile": {"status": "complete", "available_hour_ranges": hour_ranges(&req.hours)}
        },
        "provenance": {
            "profile_lane": "profile_pressure_core",
            "profile_format": profile.manifest.format,
            "diagnostic_lane": if diagnostic.is_some() { "diag_scalar_basic/sparse_v0" } else { "none" }
        },
        "_site": {
            "implementation": "wxstore_final_arch_v1",
            "elapsed_ms": started.elapsed().as_millis(),
            "response_format": req.response_format.as_str(),
            "profile": req.profile_mode.as_str(),
            "diagnostics": req.diagnostic_mode.as_str()
        }
    });
    if let Some(diagnostics) = diagnostics {
        body.as_object_mut()
            .unwrap()
            .insert("diagnostics".to_string(), diagnostics);
    }
    Ok(body)
}

fn build_binary_body(
    profile: &ProfileLane,
    diagnostic: Option<&DiagnosticLane>,
    point: GridPoint,
    requested_lat: f64,
    requested_lon: f64,
    req: &RequestShape,
) -> Result<Bytes> {
    let mut payload = Vec::<u8>::new();
    let mut fields = Vec::<Value>::new();
    for variable in &req.profile_variables {
        let series = profile.read_variable_point(variable, &req.hours, &point)?;
        let offset = payload.len();
        for value in &series.values {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        let (api_name, units) = profile_output(variable);
        fields.push(json!({
            "section": "profile",
            "name": api_name,
            "source_variable": variable,
            "units": units,
            "dtype": "i16",
            "dims": ["time", "pressure_hpa"],
            "shape": [req.hours.len(), profile.manifest.levels_hpa.len()],
            "scale_factor": series.scale_factor,
            "add_offset": series.add_offset,
            "missing": MISSING_I16,
            "payload_offset": offset,
            "payload_len": series.values.len() * 2
        }));
    }

    if req.diagnostic_mode != DiagnosticMode::None {
        if let Some(diagnostic) = diagnostic {
            let response =
                diagnostic.sample_point(point.lat, point.lon, &req.hours, req.diagnostic_mode);
            for (name, values) in response.values {
                let offset = payload.len();
                for value in values {
                    payload.extend_from_slice(&value.unwrap_or(f64::NAN).to_le_bytes());
                }
                let units = response.units.get(&name).cloned().unwrap_or_default();
                fields.push(json!({
                    "section": "diagnostics",
                    "name": name,
                    "units": units,
                    "dtype": "f64",
                    "dims": ["time"],
                    "shape": [req.hours.len()],
                    "missing": "NaN",
                    "payload_offset": offset,
                    "payload_len": req.hours.len() * 8
                }));
            }
        }
    }

    let header = json!({
        "schema": "wxstore.point.bin.v1",
        "endianness": "little",
        "model": profile.manifest.model,
        "domain": profile.manifest.domain,
        "run": {
            "cycle": profile.manifest.cycle,
            "run_id": profile.manifest.run_id,
            "forecast_hours": req.hours,
            "valid_times": valid_times_from_cycle(&profile.manifest.cycle, &req.hours)
        },
        "point": {
            "requested_lat": requested_lat,
            "requested_lon": requested_lon,
            "grid": point
        },
        "axes": {
            "pressure_hpa": profile.manifest.levels_hpa
        },
        "fields": fields,
        "provenance": {
            "profile_lane": "profile_pressure_core",
            "diagnostic_lane": if diagnostic.is_some() { "diag_scalar_basic/sparse_v0" } else { "none" }
        }
    });
    let header_bytes = serde_json::to_vec(&header)?;
    let mut output =
        Vec::with_capacity(WXBIN_MAGIC.len() + 12 + header_bytes.len() + payload.len());
    output.extend_from_slice(WXBIN_MAGIC);
    output.extend_from_slice(&(header_bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    output.extend_from_slice(&header_bytes);
    output.extend_from_slice(&payload);
    Ok(Bytes::from(output))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseFormat {
    JsonCompact,
    WxBin,
}

impl ResponseFormat {
    fn parse(value: Option<&str>) -> Result<Self> {
        match value.unwrap_or("json-compact") {
            "json" | "json-compact" | "compact" => Ok(Self::JsonCompact),
            "wxbin" | "binary" | "aether-bin" => Ok(Self::WxBin),
            value => bail!("unsupported format '{value}'"),
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::JsonCompact => "json-compact",
            Self::WxBin => "wxbin",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagnosticMode {
    None,
    Basic,
    Severe,
    Parcel,
    All,
}

impl DiagnosticMode {
    fn parse(value: Option<&str>) -> Self {
        match value.unwrap_or("basic") {
            "none" => Self::None,
            "severe" => Self::Severe,
            "parcel" | "parcels" => Self::Parcel,
            "all" | "full" => Self::All,
            _ => Self::Basic,
        }
    }

    fn keys(self) -> Option<&'static [&'static str]> {
        match self {
            Self::None => Some(&[]),
            Self::Basic => Some(BASIC_DIAGNOSTICS),
            Self::Severe => Some(SEVERE_DIAGNOSTICS),
            Self::Parcel => Some(PARCEL_DIAGNOSTICS),
            Self::All => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Basic => "basic",
            Self::Severe => "severe",
            Self::Parcel => "parcel",
            Self::All => "all",
        }
    }
}

#[derive(Debug, Clone)]
struct RequestShape {
    hours: Vec<u8>,
    profile_mode: String,
    profile_variables: Vec<String>,
    diagnostic_mode: DiagnosticMode,
    response_format: ResponseFormat,
}

impl RequestShape {
    fn from_point_query(profile: &ProfileLane, query: &PointQuery) -> Result<Self> {
        let hours = parse_hours(
            query
                .hours
                .as_deref()
                .or(query.forecast_hours.as_deref())
                .unwrap_or("0-48"),
        )?;
        let profile_variables = profile_variables(
            profile,
            query.profile.as_deref(),
            query.profile_vars.as_deref().or(query.variables.as_deref()),
        )?;
        Ok(Self {
            hours,
            profile_mode: query.profile.as_deref().unwrap_or("sounding").to_string(),
            profile_variables,
            diagnostic_mode: DiagnosticMode::parse(
                query.diagnostics.as_deref().or(query.diag.as_deref()),
            ),
            response_format: ResponseFormat::parse(query.format.as_deref())?,
        })
    }

    fn from_canonical_query(profile: &ProfileLane, query: &CanonicalQuery) -> Result<Self> {
        let hours = parse_hours(
            query
                .hours
                .as_deref()
                .or(query.forecast_hours.as_deref())
                .unwrap_or("0-48"),
        )?;
        let profile_variables = profile_variables(
            profile,
            query.profile.as_deref(),
            query.profile_vars.as_deref().or(query.variables.as_deref()),
        )?;
        Ok(Self {
            hours,
            profile_mode: query.profile.as_deref().unwrap_or("sounding").to_string(),
            profile_variables,
            diagnostic_mode: DiagnosticMode::parse(
                query.diagnostics.as_deref().or(query.diag.as_deref()),
            ),
            response_format: ResponseFormat::parse(query.format.as_deref())?,
        })
    }
}

fn profile_variables(
    profile: &ProfileLane,
    mode: Option<&str>,
    explicit: Option<&str>,
) -> Result<Vec<String>> {
    if let Some(explicit) = explicit {
        let variables = split_csv(explicit);
        if variables.is_empty() {
            bail!("profile_vars was provided but no variables were listed");
        }
        for variable in &variables {
            if !profile.has_variable(variable) {
                bail!("profile variable '{variable}' is not available");
            }
        }
        return Ok(variables);
    }
    let variables = match mode.unwrap_or("sounding") {
        "none" => Vec::new(),
        "sounding" | "core" => SOUNDING_CORE
            .iter()
            .map(|value| value.to_string())
            .collect(),
        "all" | "full" | "pressure-all" => profile.variable_names(),
        value => bail!("unsupported profile mode '{value}'"),
    };
    for variable in &variables {
        if !profile.has_variable(variable) {
            bail!("profile variable '{variable}' is not available");
        }
    }
    Ok(variables)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileManifest {
    format: String,
    model: String,
    domain: String,
    product: String,
    cycle: String,
    run_id: String,
    forecast_hours: Vec<u8>,
    variables: Vec<ProfileVariable>,
    levels_hpa: Vec<u16>,
    nx: usize,
    ny: usize,
    dimensions: Vec<String>,
    chunk_y: usize,
    chunk_x: usize,
    chunk_levels: usize,
    chunk_hours: usize,
    compression: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileVariable {
    name: String,
    label: String,
    units: String,
    path: String,
    codec: String,
    scale_factor: f32,
    add_offset: f32,
    missing: i16,
}

struct ProfileLane {
    root: PathBuf,
    manifest: ProfileManifest,
    files: RwLock<HashMap<String, Arc<ProfileFile>>>,
    grid_cache: RwLock<HashMap<String, Arc<SpatialGrid>>>,
}

struct ProfileFile {
    mmap: Mmap,
    header: WxpHeader,
    index: Vec<WxpIndexRecord>,
}

#[derive(Debug, Clone, Copy)]
struct WxpHeader {
    nx: usize,
    ny: usize,
    levels_len: usize,
    hours_len: usize,
    chunk_x: usize,
    scale_factor: f32,
    add_offset: f32,
    chunk_count: usize,
    index_offset: usize,
}

#[derive(Debug, Clone, Copy)]
struct WxpIndexRecord {
    offset: usize,
    len: usize,
    x_count: usize,
}

#[derive(Debug, Clone, Copy, Serialize)]
struct GridPoint {
    x: usize,
    y: usize,
    index: usize,
    lat: f64,
    lon: f64,
    sample: &'static str,
}

#[derive(Debug)]
struct QuantizedSeries {
    values: Vec<i16>,
    scale_factor: f32,
    add_offset: f32,
}

#[derive(Debug, Clone, Copy)]
enum PressureGridKind {
    Variable(&'static str),
    Dewpoint,
    RelativeHumidity,
    WindSpeed,
}

#[derive(Debug, Clone)]
struct PressureGridSpec {
    output_name: String,
    units: &'static str,
    level_hpa: u16,
    kind: PressureGridKind,
}

impl ProfileLane {
    fn open(root: &Path) -> Result<Self> {
        let manifest: ProfileManifest = serde_json::from_slice(
            &fs::read(root.join("manifest.json")).context("read profile manifest")?,
        )
        .context("parse profile manifest")?;
        Ok(Self {
            root: root.to_path_buf(),
            manifest,
            files: RwLock::new(HashMap::new()),
            grid_cache: RwLock::new(HashMap::new()),
        })
    }

    fn lane_manifest_json(&self) -> Value {
        json!({
            "schema": "wxstore.lane.v1",
            "id": "profile_pressure_core",
            "status": "complete",
            "role": "canonical",
            "format": self.manifest.format,
            "product": self.manifest.product,
            "axes": {
                "lead_time_seconds": self.manifest.forecast_hours.iter().map(|hour| u32::from(*hour) * 3600).collect::<Vec<_>>(),
                "pressure_hpa": self.manifest.levels_hpa
            },
            "chunk": {
                "y": self.manifest.chunk_y,
                "x": self.manifest.chunk_x,
                "levels": self.manifest.chunk_levels,
                "hours": self.manifest.chunk_hours
            },
            "fields": self.manifest.variables.iter().map(|variable| {
                json!({
                    "field_id": format!("{}.{}.profile_pressure.{}", self.manifest.model, self.manifest.domain, variable.name),
                    "source_name": variable.name,
                    "label": variable.label,
                    "units": variable.units,
                    "dimensions": ["y", "x_chunk", "pressure_hpa", "lead_time"],
                    "statistic": null,
                    "derivation": null,
                    "codec": {
                        "kind": variable.codec,
                        "scale_factor": variable.scale_factor,
                        "add_offset": variable.add_offset,
                        "missing": variable.missing
                    }
                })
            }).collect::<Vec<_>>()
        })
    }

    fn variable_names(&self) -> Vec<String> {
        self.manifest
            .variables
            .iter()
            .map(|v| v.name.clone())
            .collect()
    }

    fn has_variable(&self, variable: &str) -> bool {
        self.manifest
            .variables
            .iter()
            .any(|item| item.name == variable)
    }

    fn locate_nearest(&self, lat: f64, lon: f64) -> Result<GridPoint> {
        if !lat.is_finite() || !lon.is_finite() {
            bail!("lat/lon must be finite");
        }
        let (x, y) = if self.manifest.model == "hrrr"
            && self.manifest.nx == 1799
            && self.manifest.ny == 1059
        {
            HrrrLambert::default().nearest(lat, lon)
        } else {
            bail!("lat/lon lookup for this grid is not implemented yet; use canonical x/y endpoint")
        };
        Ok(GridPoint {
            x,
            y,
            index: y * self.manifest.nx + x,
            lat,
            lon: normalize_lon(lon),
            sample: "nearest",
        })
    }

    fn grid_point(&self, x: usize, y: usize) -> Result<GridPoint> {
        if x >= self.manifest.nx || y >= self.manifest.ny {
            bail!(
                "grid point ({x}, {y}) outside {}x{}",
                self.manifest.nx,
                self.manifest.ny
            );
        }
        let (lat, lon) = if self.manifest.model == "hrrr"
            && self.manifest.nx == 1799
            && self.manifest.ny == 1059
        {
            HrrrLambert::default().latlon_at(x, y)
        } else {
            (f64::NAN, f64::NAN)
        };
        Ok(GridPoint {
            x,
            y,
            index: y * self.manifest.nx + x,
            lat,
            lon,
            sample: "nearest",
        })
    }

    fn read_variable_point(
        &self,
        variable: &str,
        forecast_hours: &[u8],
        point: &GridPoint,
    ) -> Result<QuantizedSeries> {
        let hour_indices = forecast_hours
            .iter()
            .map(|hour| {
                self.manifest
                    .forecast_hours
                    .iter()
                    .position(|stored| stored == hour)
                    .ok_or_else(|| anyhow!("forecast hour f{hour:03} is not available"))
            })
            .collect::<Result<Vec<_>>>()?;
        if hour_indices.is_empty() {
            bail!("no forecast hours requested");
        }
        if !hour_indices.windows(2).all(|pair| pair[1] == pair[0] + 1) {
            bail!("non-contiguous forecast hours are not supported by this lane reader yet");
        }
        let file = self.file_for_variable(variable)?;
        let chunks_per_row = self.manifest.nx.div_ceil(file.header.chunk_x);
        let chunk_id = point.y * chunks_per_row + point.x / file.header.chunk_x;
        let record = *file
            .index
            .get(chunk_id)
            .ok_or_else(|| anyhow!("missing chunk {chunk_id} for {variable}"))?;
        let local_x = point.x % file.header.chunk_x;
        if local_x >= record.x_count {
            bail!("local x outside chunk");
        }
        let end = record.offset + record.len;
        if end > file.mmap.len() {
            bail!("chunk range exceeds file length");
        }
        let decoded = zstd::stream::decode_all(&file.mmap[record.offset..end])
            .with_context(|| format!("decode {variable} chunk {chunk_id}"))?;
        let expected_len = record.x_count * file.header.levels_len * file.header.hours_len * 2;
        if decoded.len() != expected_len {
            bail!(
                "decoded chunk length mismatch: {} != {expected_len}",
                decoded.len()
            );
        }
        let mut values = Vec::with_capacity(forecast_hours.len() * file.header.levels_len);
        for hour_index in hour_indices {
            for level_index in 0..file.header.levels_len {
                let value_index = ((local_x * file.header.levels_len + level_index)
                    * file.header.hours_len)
                    + hour_index;
                let byte_offset = value_index * 2;
                values.push(i16::from_le_bytes([
                    decoded[byte_offset],
                    decoded[byte_offset + 1],
                ]));
            }
        }
        Ok(QuantizedSeries {
            values,
            scale_factor: file.header.scale_factor,
            add_offset: file.header.add_offset,
        })
    }

    fn read_pressure_grid_product(
        &self,
        product: &str,
        forecast_hour: u32,
    ) -> Result<Option<SpatialGrid>> {
        let Some(spec) = parse_pressure_grid_product(product, &self.manifest.levels_hpa) else {
            return Ok(None);
        };
        let cache_key = format!("{}|{}|{}", spec.output_name, spec.level_hpa, forecast_hour);
        if let Some(grid) = self
            .grid_cache
            .read()
            .ok()
            .and_then(|cache| cache.get(&cache_key).cloned())
        {
            return Ok(Some((*grid).clone()));
        }

        let values = match spec.kind {
            PressureGridKind::Variable(variable) => {
                if !self.has_variable(variable) {
                    bail!(
                        "pressure product '{}' needs profile variable '{}' which is not in this profile lane",
                        product,
                        variable
                    );
                }
                self.read_pressure_variable_values(variable, spec.level_hpa, forecast_hour)?
            }
            PressureGridKind::Dewpoint => {
                let temp =
                    self.read_pressure_variable_values("TMP", spec.level_hpa, forecast_hour)?;
                let q =
                    self.read_pressure_variable_values("SPFH", spec.level_hpa, forecast_hour)?;
                temp.iter()
                    .zip(q.iter())
                    .map(|(temp, q)| {
                        finite2(*temp, *q)
                            .and_then(|(_, q)| {
                                specific_humidity_to_dewpoint_c(
                                    f64::from(q),
                                    f64::from(spec.level_hpa),
                                )
                                .map(|v| v as f32)
                            })
                            .unwrap_or(f32::NAN)
                    })
                    .collect::<Vec<_>>()
            }
            PressureGridKind::RelativeHumidity => {
                let temp =
                    self.read_pressure_variable_values("TMP", spec.level_hpa, forecast_hour)?;
                let q =
                    self.read_pressure_variable_values("SPFH", spec.level_hpa, forecast_hour)?;
                temp.iter()
                    .zip(q.iter())
                    .map(|(temp, q)| {
                        if !temp.is_finite() || !q.is_finite() {
                            return f32::NAN;
                        }
                        let Some(td) = specific_humidity_to_dewpoint_c(
                            f64::from(*q),
                            f64::from(spec.level_hpa),
                        ) else {
                            return f32::NAN;
                        };
                        relative_humidity_from_temp_dewpoint_c(*temp, td as f32)
                    })
                    .collect::<Vec<_>>()
            }
            PressureGridKind::WindSpeed => {
                let u =
                    self.read_pressure_variable_values("UGRD", spec.level_hpa, forecast_hour)?;
                let v =
                    self.read_pressure_variable_values("VGRD", spec.level_hpa, forecast_hour)?;
                u.iter()
                    .zip(v.iter())
                    .map(|(u, v)| finite2(*u, *v).map_or(f32::NAN, |(u, v)| (u * u + v * v).sqrt()))
                    .collect::<Vec<_>>()
            }
        };

        let grid = SpatialGrid {
            model: self.manifest.model.clone(),
            run_id: self.manifest.run_id.clone(),
            member: None,
            variable: spec.output_name,
            units: spec.units.to_string(),
            forecast_hour,
            nx: self.manifest.nx,
            ny: self.manifest.ny,
            grid_meta: spatial_grid_meta(&self.manifest.model, self.manifest.nx, self.manifest.ny),
            values: Arc::new(values),
        };
        if let Ok(mut cache) = self.grid_cache.write() {
            if cache.len() > 16 {
                cache.clear();
            }
            cache.insert(cache_key, Arc::new(grid.clone()));
        }
        Ok(Some(grid))
    }

    fn read_pressure_variable_values(
        &self,
        variable: &str,
        level_hpa: u16,
        forecast_hour: u32,
    ) -> Result<Vec<f32>> {
        let hour_u8 = u8::try_from(forecast_hour).map_err(|_| {
            anyhow!("forecast hour f{forecast_hour:03} is outside this profile lane")
        })?;
        let hour_index = self
            .manifest
            .forecast_hours
            .iter()
            .position(|stored| *stored == hour_u8)
            .ok_or_else(|| anyhow!("forecast hour f{forecast_hour:03} is not available"))?;
        let level_index = self
            .manifest
            .levels_hpa
            .iter()
            .position(|stored| *stored == level_hpa)
            .ok_or_else(|| anyhow!("pressure level {level_hpa} hPa is not available"))?;
        let file = self.file_for_variable(variable)?;
        let chunks_per_row = self.manifest.nx.div_ceil(file.header.chunk_x);
        let mut values = vec![f32::NAN; self.manifest.nx * self.manifest.ny];
        for y in 0..self.manifest.ny {
            for chunk_x in 0..chunks_per_row {
                let chunk_id = y * chunks_per_row + chunk_x;
                let record = *file
                    .index
                    .get(chunk_id)
                    .ok_or_else(|| anyhow!("missing chunk {chunk_id} for {variable}"))?;
                let end = record.offset + record.len;
                if end > file.mmap.len() {
                    bail!("chunk range exceeds file length");
                }
                let decoded = zstd::stream::decode_all(&file.mmap[record.offset..end])
                    .with_context(|| format!("decode {variable} chunk {chunk_id}"))?;
                let expected_len =
                    record.x_count * file.header.levels_len * file.header.hours_len * 2;
                if decoded.len() != expected_len {
                    bail!(
                        "decoded chunk length mismatch: {} != {expected_len}",
                        decoded.len()
                    );
                }
                for local_x in 0..record.x_count {
                    let x = chunk_x * file.header.chunk_x + local_x;
                    if x >= self.manifest.nx {
                        continue;
                    }
                    let value_index = ((local_x * file.header.levels_len + level_index)
                        * file.header.hours_len)
                        + hour_index;
                    let byte_offset = value_index * 2;
                    let encoded =
                        i16::from_le_bytes([decoded[byte_offset], decoded[byte_offset + 1]]);
                    if encoded != MISSING_I16 {
                        values[y * self.manifest.nx + x] =
                            f32::from(encoded) / file.header.scale_factor - file.header.add_offset;
                    }
                }
            }
        }
        Ok(values)
    }

    fn file_for_variable(&self, variable: &str) -> Result<Arc<ProfileFile>> {
        {
            let cache = self.files.read().unwrap();
            if let Some(file) = cache.get(variable) {
                return Ok(file.clone());
            }
        }
        let descriptor = self
            .manifest
            .variables
            .iter()
            .find(|item| item.name == variable)
            .ok_or_else(|| anyhow!("variable '{variable}' is not available"))?;
        let path = self.root.join(&descriptor.path);
        let file = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let mmap = unsafe { MmapOptions::new().map(&file) }
            .with_context(|| format!("mmap {}", path.display()))?;
        let header = parse_wxp_header(&mmap)?;
        let index = parse_wxp_index(&mmap, header)?;
        let file = Arc::new(ProfileFile {
            mmap,
            header,
            index,
        });
        let mut cache = self.files.write().unwrap();
        cache.insert(variable.to_string(), file.clone());
        Ok(file)
    }
}

fn parse_wxp_header(mmap: &[u8]) -> Result<WxpHeader> {
    if mmap.len() < WXP_HEADER_LEN {
        bail!("file too short for wxp header");
    }
    if &mmap[0..8] != WXP_MAGIC {
        bail!("bad wxp magic");
    }
    let version = u32_from(&mmap[8..12])?;
    if version != WXP_VERSION {
        bail!("unsupported wxp version {version}");
    }
    let header = WxpHeader {
        nx: u32_from(&mmap[12..16])? as usize,
        ny: u32_from(&mmap[16..20])? as usize,
        levels_len: u32_from(&mmap[20..24])? as usize,
        hours_len: u32_from(&mmap[24..28])? as usize,
        chunk_x: u32_from(&mmap[28..32])? as usize,
        scale_factor: f32_from(&mmap[32..36])?,
        add_offset: f32_from(&mmap[36..40])?,
        chunk_count: u64_from(&mmap[40..48])? as usize,
        index_offset: u64_from(&mmap[48..56])? as usize,
    };
    if header.nx == 0 || header.ny == 0 || header.chunk_x == 0 {
        bail!("invalid wxp header dimensions");
    }
    Ok(header)
}

fn parse_wxp_index(mmap: &[u8], header: WxpHeader) -> Result<Vec<WxpIndexRecord>> {
    let bytes_len = header.chunk_count * WXP_INDEX_RECORD_LEN;
    let end = header.index_offset + bytes_len;
    if end > mmap.len() {
        bail!("wxp index exceeds file length");
    }
    let mut records = Vec::with_capacity(header.chunk_count);
    let mut offset = header.index_offset;
    for _ in 0..header.chunk_count {
        records.push(WxpIndexRecord {
            offset: u64_from(&mmap[offset..offset + 8])? as usize,
            len: u32_from(&mmap[offset + 8..offset + 12])? as usize,
            x_count: u32_from(&mmap[offset + 12..offset + 16])? as usize,
        });
        offset += WXP_INDEX_RECORD_LEN;
    }
    Ok(records)
}

#[derive(Debug, Clone, Deserialize)]
struct ZarrArrayMeta {
    shape: Vec<usize>,
    chunks: Vec<usize>,
    dtype: String,
    compressor: Option<ZarrCompressor>,
}

#[derive(Debug, Clone, Deserialize)]
struct ZarrCompressor {
    id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct WxaDense2dMeta {
    schema: String,
    model: String,
    run: String,
    member: Option<String>,
    variable: String,
    units: String,
    nx: usize,
    ny: usize,
    forecast_hours: Vec<u32>,
    chunk_y: usize,
    chunk_x: usize,
    dtype: String,
    codec: String,
    grid: Value,
}

#[derive(Debug, Clone, Deserialize)]
struct RustwxGridExportManifest {
    model: String,
    run_id: String,
    #[serde(default)]
    fields: Vec<RustwxGridExportRecord>,
    #[serde(default)]
    blockers: Vec<Value>,
}

#[derive(Debug, Clone, Deserialize)]
struct RustwxGridExportRecord {
    product_slug: String,
    units: String,
    forecast_hour: u16,
    nx: usize,
    ny: usize,
    #[serde(default)]
    crop: Option<RustwxGridExportCrop>,
    #[serde(default)]
    bounds: Option<[f64; 4]>,
    values_path: PathBuf,
    lat_path: PathBuf,
    lon_path: PathBuf,
}

#[derive(Debug, Clone, Copy, Deserialize)]
struct RustwxGridExportCrop {
    x_start: usize,
    x_end: usize,
    y_start: usize,
    y_end: usize,
}

#[derive(Debug, Clone, Copy)]
struct WxaDense2dHeader {
    metadata_len: usize,
    index_count: usize,
    index_offset: usize,
    payload_offset: usize,
}

#[derive(Debug, Clone)]
struct WxaDense2dIndexRecord {
    forecast_hour: u32,
    chunk_y: usize,
    chunk_x: usize,
    y_count: usize,
    x_count: usize,
    raw_len: usize,
    offset: usize,
    len: usize,
    min: f32,
    max: f32,
    valid_count: u32,
}

struct SpatialLane {
    root: PathBuf,
    grid_cache: RwLock<HashMap<String, Arc<SpatialGrid>>>,
    array_exists_cache: RwLock<HashMap<String, bool>>,
}

#[derive(Clone)]
struct SpatialGrid {
    model: String,
    run_id: String,
    member: Option<String>,
    variable: String,
    units: String,
    forecast_hour: u32,
    nx: usize,
    ny: usize,
    grid_meta: Value,
    values: Arc<Vec<f32>>,
}

impl SpatialGrid {
    fn grid_meta(&self) -> Value {
        self.grid_meta.clone()
    }
}

impl SpatialLane {
    fn open(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!("spatial root does not exist: {}", root.display());
        }
        Ok(Self {
            root: root.to_path_buf(),
            grid_cache: RwLock::new(HashMap::new()),
            array_exists_cache: RwLock::new(HashMap::new()),
        })
    }

    fn model_ids(&self) -> Vec<String> {
        list_dirs(&self.root)
    }

    fn lane_manifest_json(&self) -> Value {
        let models = self
            .model_ids()
            .into_iter()
            .map(|model| {
                let runs = self.runs_for_model(&model);
                json!({
                    "model": model,
                    "run_count": runs.len(),
                    "latest_run": runs.last().cloned(),
                    "runs": runs
                })
            })
            .collect::<Vec<_>>();
        json!({
            "schema": "wxstore.lane.v1",
            "id": "surface_spatial",
            "status": "ready",
            "role": "native_surface_and_map_spatial_lane",
            "format": "wxa_dense2d_native_with_zarr_v2_source_adapter",
            "products": ["forecast_point", "grid_field", "map_source"],
            "models": models,
            "cache": {
                "kind": "in_process_grid_cache",
                "entries": self.grid_cache.read().map(|cache| cache.len()).unwrap_or(0)
            },
            "notes": [
                "WXA dense2d files are the native WxStore spatial serving format.",
                "Existing local Zarr-v2 arrays are still readable as source/proof adapters."
            ]
        })
    }

    fn models_json(&self) -> Value {
        let models = self
            .model_ids()
            .into_iter()
            .map(|model| {
                let runs = self.runs_for_model(&model);
                let latest = runs.last().cloned();
                let variables = latest
                    .as_deref()
                    .and_then(|run| self.variables_for(&model, run, None).ok())
                    .unwrap_or_default();
                let members = latest
                    .as_deref()
                    .map(|run| self.members_for(&model, run))
                    .unwrap_or_default();
                json!({
                    "id": model,
                    "runs": runs,
                    "latest_run": latest,
                    "members": members,
                    "latest_variables": variables
                })
            })
            .collect::<Vec<_>>();
        json!({
            "status": "ready",
            "root": self.root,
            "models": models
        })
    }

    fn variables_json(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
    ) -> Result<Value, ApiError> {
        let variables = self.variables_for(model, run, member).map_err(bad_anyhow)?;
        let mut available_hours = Map::new();
        for variable in &variables {
            if let Ok(hours) = self.available_hours_for(model, run, member, variable) {
                available_hours.insert(variable.clone(), json!(hours));
            }
        }
        Ok(json!({
            "schema": "wxstore.variables.v1",
            "model": model,
            "run_id": run,
            "member": member,
            "variables": variables,
            "available_hours": available_hours,
            "members": self.members_for(model, run)
        }))
    }

    fn resolve_run(&self, model: &str, run: Option<&str>) -> Result<String, ApiError> {
        if let Some(run) = run.filter(|value| *value != "latest") {
            let path = self.root.join(model).join(run);
            if path.is_dir() {
                return Ok(run.to_string());
            }
            return Err(not_found(format!(
                "run '{run}' is not available for model '{model}'"
            )));
        }
        self.runs_for_model(model)
            .last()
            .cloned()
            .ok_or_else(|| not_found(format!("no runs are available for model '{model}'")))
    }

    fn runs_for_model(&self, model: &str) -> Vec<String> {
        list_dirs(&self.root.join(model))
    }

    fn members_for(&self, model: &str, run: &str) -> Vec<String> {
        list_dirs(&self.root.join(model).join(run).join("members"))
    }

    fn variables_for(&self, model: &str, run: &str, member: Option<&str>) -> Result<Vec<String>> {
        let base = self.array_base(model, run, member);
        if base.is_dir() {
            let vars = fs::read_dir(&base)
                .with_context(|| format!("read variables in {}", base.display()))?
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| {
                    let path = entry.path();
                    if path.is_dir() && path.extension().is_some_and(|ext| ext == "zarr") {
                        path.file_stem()
                            .map(|name| name.to_string_lossy().to_string())
                    } else if path.is_file() && path.extension().is_some_and(|ext| ext == "wxa") {
                        path.file_stem()
                            .map(|name| name.to_string_lossy().to_string())
                    } else {
                        None
                    }
                })
                .collect::<BTreeSet<_>>();
            let mut vars = vars.into_iter().collect::<Vec<_>>();
            vars.sort();
            if !vars.is_empty() || member.is_some() {
                return Ok(vars);
            }
        }
        let members = self.members_for(model, run);
        if member.is_none() {
            if let Some(first) = members.first() {
                return self.variables_for(model, run, Some(first));
            }
        }
        bail!("no variables are available for model '{model}' run '{run}'");
    }

    fn available_hours_for(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
        variable: &str,
    ) -> Result<Vec<u32>> {
        let wxa_path = self.wxa_file_path(model, run, member, variable);
        if wxa_path.is_file() {
            let (_, meta, _) = read_wxa_dense2d(&wxa_path)?;
            return Ok(meta.forecast_hours);
        }
        let array_dir = self.array_dir(model, run, member, variable)?;
        let mut hours = BTreeSet::new();
        for entry in
            fs::read_dir(&array_dir).with_context(|| format!("read {}", array_dir.display()))?
        {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let Some((hour, rest)) = name.split_once('.') else {
                continue;
            };
            if rest.starts_with("0.") {
                if let Ok(hour) = hour.parse::<u32>() {
                    hours.insert(hour);
                }
            }
        }
        Ok(hours.into_iter().collect())
    }

    fn read_grid(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
        variable: &str,
        forecast_hour: u32,
    ) -> Result<SpatialGrid> {
        if self.array_exists(model, run, member, variable) {
            return self.read_raw_grid(model, run, member, variable, forecast_hour);
        }
        if let Some(raw) = raw_variable_for_product(variable) {
            return self.read_raw_grid(model, run, member, raw, forecast_hour);
        }
        if let Some(grid) =
            self.read_cheap_derived_grid(model, run, member, variable, forecast_hour)?
        {
            return Ok(grid);
        }
        self.read_raw_grid(model, run, member, variable, forecast_hour)
    }

    fn read_raw_grid(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
        variable: &str,
        forecast_hour: u32,
    ) -> Result<SpatialGrid> {
        let member_key = member.unwrap_or("-");
        let cache_key = format!("{model}|{run}|{member_key}|{variable}|{forecast_hour}");
        if let Some(grid) = self
            .grid_cache
            .read()
            .ok()
            .and_then(|cache| cache.get(&cache_key).cloned())
        {
            return Ok((*grid).clone());
        }

        let wxa_path = self.wxa_file_path(model, run, member, variable);
        if wxa_path.is_file() {
            let grid = read_spatial_wxa_grid(&wxa_path, forecast_hour)
                .with_context(|| format!("read native WXA {}", wxa_path.display()))?;
            if let Ok(mut cache) = self.grid_cache.write() {
                if cache.len() > 64 {
                    cache.clear();
                }
                cache.insert(cache_key, Arc::new(grid.clone()));
            }
            return Ok(grid);
        }

        let array_dir = self.array_dir(model, run, member, variable)?;
        let meta_path = array_dir.join(".zarray");
        let meta: ZarrArrayMeta = serde_json::from_slice(
            &fs::read(&meta_path).with_context(|| format!("read {}", meta_path.display()))?,
        )
        .with_context(|| format!("parse {}", meta_path.display()))?;
        if meta.dtype != "<f4" {
            bail!("unsupported zarr dtype '{}'", meta.dtype);
        }
        if meta.shape.len() != 3 || meta.chunks.len() != 3 {
            bail!("expected zarr shape/chunks [forecast_hour, y, x]");
        }
        let hour_count = meta.shape[0];
        let ny = meta.shape[1];
        let nx = meta.shape[2];
        let cy = meta.chunks[1].min(ny);
        let cx = meta.chunks[2].min(nx);
        let fh = forecast_hour as usize;
        if fh >= hour_count {
            bail!("forecast hour f{forecast_hour:03} outside array horizon {hour_count}");
        }
        let compressed = meta
            .compressor
            .as_ref()
            .is_some_and(|compressor| compressor.id == "zlib");
        let n_chunks_y = ny.div_ceil(cy);
        let n_chunks_x = nx.div_ceil(cx);
        let mut values = vec![f32::NAN; ny * nx];

        for chunk_y in 0..n_chunks_y {
            for chunk_x in 0..n_chunks_x {
                let path = array_dir.join(format!("{fh}.{chunk_y}.{chunk_x}"));
                let bytes =
                    fs::read(&path).with_context(|| format!("read chunk {}", path.display()))?;
                let raw = if compressed {
                    decompress_zlib(&bytes)
                        .with_context(|| format!("decompress {}", path.display()))?
                } else {
                    bytes
                };
                let chunk = raw
                    .chunks_exact(4)
                    .map(|item| f32::from_le_bytes([item[0], item[1], item[2], item[3]]))
                    .collect::<Vec<_>>();
                let y0 = chunk_y * cy;
                let x0 = chunk_x * cx;
                let y1 = (y0 + cy).min(ny);
                let x1 = (x0 + cx).min(nx);
                for yy in 0..(y1 - y0) {
                    for xx in 0..(x1 - x0) {
                        let src = yy * cx + xx;
                        let dst = (y0 + yy) * nx + (x0 + xx);
                        if let Some(value) = chunk.get(src) {
                            values[dst] = *value;
                        }
                    }
                }
            }
        }
        normalize_spatial_values(variable, &mut values);

        let grid = SpatialGrid {
            model: model.to_string(),
            run_id: run.to_string(),
            member: member.map(str::to_string),
            variable: variable.to_string(),
            units: units_for_variable(variable).to_string(),
            forecast_hour,
            nx,
            ny,
            grid_meta: spatial_grid_meta(model, nx, ny),
            values: Arc::new(values),
        };
        if let Ok(mut cache) = self.grid_cache.write() {
            if cache.len() > 64 {
                cache.clear();
            }
            cache.insert(cache_key, Arc::new(grid.clone()));
        }
        Ok(grid)
    }

    fn forecast_point(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
        lat: f64,
        lon: f64,
        variables: &[String],
        hours: &[u32],
    ) -> Result<Value> {
        let started = Instant::now();
        let mut hourly = Map::new();
        let mut units = Map::new();
        let mut sampled_grid = None::<GridPoint>;
        let times = valid_times_from_run_id(run, hours);
        hourly.insert("time".to_string(), json!(times));
        units.insert("time".to_string(), json!("iso8601"));

        for variable in variables {
            let mut values = Vec::with_capacity(hours.len());
            for hour in hours {
                let grid = self.read_grid(model, run, member, variable, *hour)?;
                units
                    .entry(variable.clone())
                    .or_insert_with(|| json!(grid.units.clone()));
                let index = grid_index_for_latlon(&grid, lat, lon).ok_or_else(|| {
                    anyhow!(
                        "lat/lon is outside {} {} {} f{hour:03}",
                        grid.model,
                        grid.run_id,
                        grid.variable
                    )
                })?;
                let x = index % grid.nx;
                let y = index / grid.nx;
                let (grid_lat, grid_lon) = grid_latlon_at(&grid, x, y);
                let point = GridPoint {
                    x,
                    y,
                    index,
                    lat: grid_lat,
                    lon: grid_lon,
                    sample: "nearest",
                };
                sampled_grid.get_or_insert(point);
                let value = grid.values.get(point.index).copied().unwrap_or(f32::NAN);
                if value.is_finite() {
                    values.push(Some(f64::from(value)));
                } else {
                    values.push(None);
                }
            }
            hourly.insert(variable.clone(), json!(values));
        }

        let gridpoint = sampled_grid.unwrap_or(GridPoint {
            x: 0,
            y: 0,
            index: 0,
            lat,
            lon: normalize_lon(lon),
            sample: "nearest",
        });
        Ok(json!({
            "latitude": lat,
            "longitude": lon,
            "generationtime_ms": started.elapsed().as_secs_f64() * 1000.0,
            "utc_offset_seconds": 0,
            "timezone": "GMT",
            "model": model,
            "run": run,
            "member": member,
            "gridpoint": gridpoint,
            "hourly_units": units,
            "hourly": hourly,
            "wxstore": {
                "schema": "wxstore.forecast.v1",
                "lane": "surface_spatial",
                "sample": "nearest",
                "format": "open_meteo_shaped_json"
            }
        }))
    }

    fn read_cheap_derived_grid(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
        variable: &str,
        forecast_hour: u32,
    ) -> Result<Option<SpatialGrid>> {
        let mk_grid =
            |template: &SpatialGrid, variable: &str, units: &str, values: Vec<f32>| SpatialGrid {
                model: template.model.clone(),
                run_id: template.run_id.clone(),
                member: template.member.clone(),
                variable: variable.to_string(),
                units: units.to_string(),
                forecast_hour,
                nx: template.nx,
                ny: template.ny,
                grid_meta: template.grid_meta.clone(),
                values: Arc::new(values),
            };

        let result = match variable {
            "dewpoint_depression_2m" => {
                let t = self.read_raw_grid(model, run, member, "temperature_2m", forecast_hour)?;
                let td = self.read_raw_grid(model, run, member, "dew_point_2m", forecast_hour)?;
                let values = t
                    .values
                    .iter()
                    .zip(td.values.iter())
                    .map(|(t, td)| finite2(*t, *td).map_or(f32::NAN, |(t, td)| t - td))
                    .collect();
                Some(mk_grid(&t, variable, "degC", values))
            }
            "vpd_2m" => {
                let t = self.read_raw_grid(model, run, member, "temperature_2m", forecast_hour)?;
                let rh =
                    self.read_raw_grid(model, run, member, "relative_humidity_2m", forecast_hour)?;
                let values = t
                    .values
                    .iter()
                    .zip(rh.values.iter())
                    .map(|(t, rh)| {
                        finite2(*t, *rh)
                            .map_or(f32::NAN, |(t, rh)| vapor_pressure_deficit_kpa(t, rh))
                    })
                    .collect();
                Some(mk_grid(&t, variable, "kPa", values))
            }
            "heat_index_2m" => {
                let t = self.read_raw_grid(model, run, member, "temperature_2m", forecast_hour)?;
                let rh =
                    self.read_raw_grid(model, run, member, "relative_humidity_2m", forecast_hour)?;
                let values = t
                    .values
                    .iter()
                    .zip(rh.values.iter())
                    .map(|(t, rh)| finite2(*t, *rh).map_or(f32::NAN, |(t, rh)| heat_index_c(t, rh)))
                    .collect();
                Some(mk_grid(&t, variable, "degC", values))
            }
            "wind_chill_2m" => {
                let t = self.read_raw_grid(model, run, member, "temperature_2m", forecast_hour)?;
                let wind = self.read_grid(model, run, member, "wind_speed_10m", forecast_hour)?;
                let values = t
                    .values
                    .iter()
                    .zip(wind.values.iter())
                    .map(|(t, wind)| {
                        finite2(*t, *wind).map_or(f32::NAN, |(t, wind)| wind_chill_c(t, wind))
                    })
                    .collect();
                Some(mk_grid(&t, variable, "degC", values))
            }
            "apparent_temperature_2m" => {
                let t = self.read_raw_grid(model, run, member, "temperature_2m", forecast_hour)?;
                let rh =
                    self.read_raw_grid(model, run, member, "relative_humidity_2m", forecast_hour)?;
                let wind = self.read_grid(model, run, member, "wind_speed_10m", forecast_hour)?;
                let values = t
                    .values
                    .iter()
                    .zip(rh.values.iter())
                    .zip(wind.values.iter())
                    .map(|((t, rh), wind)| {
                        finite3(*t, *rh, *wind).map_or(f32::NAN, |(t, rh, wind)| {
                            apparent_temperature_c(t, rh, wind)
                        })
                    })
                    .collect();
                Some(mk_grid(&t, variable, "degC", values))
            }
            "wind_speed_10m" | "10m_wind_speed" => {
                let u = self.read_raw_grid(
                    model,
                    run,
                    member,
                    "u_component_of_wind_10m",
                    forecast_hour,
                )?;
                let v = self.read_raw_grid(
                    model,
                    run,
                    member,
                    "v_component_of_wind_10m",
                    forecast_hour,
                )?;
                let values = u
                    .values
                    .iter()
                    .zip(v.values.iter())
                    .map(|(u, v)| finite2(*u, *v).map_or(f32::NAN, |(u, v)| (u * u + v * v).sqrt()))
                    .collect();
                Some(mk_grid(&u, variable, "m/s", values))
            }
            "wind_direction_10m" => {
                let u = self.read_raw_grid(
                    model,
                    run,
                    member,
                    "u_component_of_wind_10m",
                    forecast_hour,
                )?;
                let v = self.read_raw_grid(
                    model,
                    run,
                    member,
                    "v_component_of_wind_10m",
                    forecast_hour,
                )?;
                let values = u
                    .values
                    .iter()
                    .zip(v.values.iter())
                    .map(|(u, v)| {
                        finite2(*u, *v).map_or(f32::NAN, |(u, v)| wind_direction_deg(u, v))
                    })
                    .collect();
                Some(mk_grid(&u, variable, "deg", values))
            }
            _ => {
                if let Some(window) = parse_windowed_product(variable) {
                    Some(self.read_windowed_grid(model, run, member, variable, window)?)
                } else {
                    None
                }
            }
        };
        Ok(result)
    }

    fn read_windowed_grid(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
        variable: &str,
        window: WindowedProduct,
    ) -> Result<SpatialGrid> {
        let hours = self.available_hours_for(model, run, member, window.raw_variable)?;
        let selected = hours
            .into_iter()
            .filter(|hour| *hour >= window.start && *hour <= window.end)
            .collect::<Vec<_>>();
        if selected.is_empty() {
            bail!("no available hours for windowed product '{variable}'");
        }
        let first = self.read_grid(model, run, member, window.raw_variable, selected[0])?;
        let mut values = vec![
            match window.reducer {
                WindowReducer::Min => f32::INFINITY,
                WindowReducer::Max => f32::NEG_INFINITY,
                WindowReducer::Range => f32::NAN,
            };
            first.values.len()
        ];
        let mut mins = if window.reducer == WindowReducer::Range {
            vec![f32::INFINITY; first.values.len()]
        } else {
            Vec::new()
        };
        let mut maxs = if window.reducer == WindowReducer::Range {
            vec![f32::NEG_INFINITY; first.values.len()]
        } else {
            Vec::new()
        };
        let mut valid = vec![false; first.values.len()];
        for hour in selected {
            let grid = self.read_grid(model, run, member, window.raw_variable, hour)?;
            for (index, value) in grid.values.iter().copied().enumerate() {
                if !value.is_finite() {
                    continue;
                }
                valid[index] = true;
                match window.reducer {
                    WindowReducer::Min => values[index] = values[index].min(value),
                    WindowReducer::Max => values[index] = values[index].max(value),
                    WindowReducer::Range => {
                        mins[index] = mins[index].min(value);
                        maxs[index] = maxs[index].max(value);
                    }
                }
            }
        }
        if window.reducer == WindowReducer::Range {
            for index in 0..values.len() {
                if valid[index] {
                    values[index] = maxs[index] - mins[index];
                }
            }
        }
        for (index, value) in values.iter_mut().enumerate() {
            if !valid[index] {
                *value = f32::NAN;
            }
        }
        Ok(SpatialGrid {
            model: first.model,
            run_id: first.run_id,
            member: first.member,
            variable: variable.to_string(),
            units: units_for_variable(window.raw_variable).to_string(),
            forecast_hour: window.end,
            nx: first.nx,
            ny: first.ny,
            grid_meta: first.grid_meta,
            values: Arc::new(values),
        })
    }

    fn array_base(&self, model: &str, run: &str, member: Option<&str>) -> PathBuf {
        let run_path = self.root.join(model).join(run);
        if let Some(member) = member {
            run_path.join("members").join(member)
        } else {
            run_path
        }
    }

    fn array_dir(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
        variable: &str,
    ) -> Result<PathBuf> {
        let mut base = self.array_base(model, run, member);
        let mut path = base.join(format!("{variable}.zarr")).join("data");
        if path.join(".zarray").is_file() {
            return Ok(path);
        }
        if member.is_none() {
            let members = self.members_for(model, run);
            if let Some(first) = members.first() {
                base = self.array_base(model, run, Some(first));
                path = base.join(format!("{variable}.zarr")).join("data");
                if path.join(".zarray").is_file() {
                    return Ok(path);
                }
            }
        }
        bail!("spatial variable '{variable}' is not available for {model}/{run}");
    }

    fn wxa_file_path(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
        variable: &str,
    ) -> PathBuf {
        let direct = self
            .array_base(model, run, member)
            .join(format!("{variable}.wxa"));
        if direct.is_file() || member.is_some() {
            return direct;
        }
        if let Some(first) = self.members_for(model, run).first() {
            let member_path = self
                .array_base(model, run, Some(first))
                .join(format!("{variable}.wxa"));
            if member_path.is_file() {
                return member_path;
            }
        }
        direct
    }

    fn array_exists(&self, model: &str, run: &str, member: Option<&str>, variable: &str) -> bool {
        let key = format!("{}|{}|{}|{}", model, run, member.unwrap_or("-"), variable);
        if let Some(value) = self
            .array_exists_cache
            .read()
            .ok()
            .and_then(|cache| cache.get(&key).copied())
        {
            return value;
        }
        let exists = self.wxa_file_path(model, run, member, variable).is_file()
            || self.array_dir(model, run, member, variable).is_ok();
        if let Ok(mut cache) = self.array_exists_cache.write() {
            cache.insert(key, exists);
        }
        exists
    }
}

fn list_dirs(path: &Path) -> Vec<String> {
    let mut values = fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
        .collect::<Vec<_>>();
    values.sort();
    values
}

fn decompress_zlib(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    let mut decoder = ZlibDecoder::new(data);
    let mut decoded = Vec::new();
    decoder.read_to_end(&mut decoded)?;
    Ok(decoded)
}

fn write_spatial_zarr_grid(
    root: &Path,
    model: &str,
    run: &str,
    member: Option<&str>,
    product: &str,
    forecast_hour: u32,
    grid: &SpatialGrid,
) -> Result<PathBuf> {
    let mut base = root.join(model).join(run);
    if let Some(member) = member {
        base = base.join("members").join(member);
    }
    let data_dir = base.join(format!("{product}.zarr")).join("data");
    fs::create_dir_all(&data_dir)?;
    let max_hour = forecast_hour as usize + 1;
    let meta_path = data_dir.join(".zarray");
    let existing_shape = if meta_path.is_file() {
        serde_json::from_slice::<ZarrArrayMeta>(&fs::read(&meta_path)?)?.shape
    } else {
        Vec::new()
    };
    let shape_hour = existing_shape.first().copied().unwrap_or(0).max(max_hour);
    let meta = json!({
        "zarr_format": 2,
        "shape": [shape_hour, grid.ny, grid.nx],
        "chunks": [1, 256, 256],
        "dtype": "<f4",
        "compressor": {"id": "zlib", "level": 4},
        "fill_value": "NaN",
        "order": "C",
        "filters": null
    });
    fs::write(&meta_path, serde_json::to_vec_pretty(&meta)?)?;
    fs::write(
        data_dir.join(".zattrs"),
        serde_json::to_vec_pretty(&json!({
            "schema": "wxstore.spatial_product.v1",
            "model": model,
            "run": run,
            "member": member,
            "product": product,
            "units": grid.units,
            "source": "wxstore_materialize_spatial",
            "forecast_hour": forecast_hour
        }))?,
    )?;

    let cy = 256usize.min(grid.ny);
    let cx = 256usize.min(grid.nx);
    let n_chunks_y = grid.ny.div_ceil(cy);
    let n_chunks_x = grid.nx.div_ceil(cx);
    for chunk_y in 0..n_chunks_y {
        for chunk_x in 0..n_chunks_x {
            let y0 = chunk_y * cy;
            let x0 = chunk_x * cx;
            let y1 = (y0 + cy).min(grid.ny);
            let x1 = (x0 + cx).min(grid.nx);
            let mut chunk = vec![f32::NAN; cy * cx];
            for yy in 0..(y1 - y0) {
                for xx in 0..(x1 - x0) {
                    let src = (y0 + yy) * grid.nx + (x0 + xx);
                    let dst = yy * cx + xx;
                    chunk[dst] = grid.values[src];
                }
            }
            let raw = chunk
                .iter()
                .flat_map(|value| value.to_le_bytes())
                .collect::<Vec<_>>();
            let compressed = compress_zlib(&raw, 4)?;
            fs::write(
                data_dir.join(format!("{forecast_hour}.{chunk_y}.{chunk_x}")),
                compressed,
            )?;
        }
    }
    Ok(data_dir)
}

fn write_spatial_wxa_grids(
    root: &Path,
    model: &str,
    run: &str,
    member: Option<&str>,
    product: &str,
    grids: &[SpatialGrid],
) -> Result<PathBuf> {
    let first = grids
        .first()
        .ok_or_else(|| anyhow!("cannot write empty WXA product"))?;
    for grid in grids {
        if grid.nx != first.nx || grid.ny != first.ny {
            bail!("all WXA grids for a product must share dimensions");
        }
    }

    let mut base = root.join(model).join(run);
    if let Some(member) = member {
        base = base.join("members").join(member);
    }
    fs::create_dir_all(&base)?;
    let path = base.join(format!("{product}.wxa"));
    let tmp_path = base.join(format!("{product}.wxa.tmp"));

    let incoming_hours = grids
        .iter()
        .map(|grid| grid.forecast_hour)
        .collect::<BTreeSet<_>>();
    let mut merged_grids = Vec::new();
    if path.is_file() {
        let (_, meta, _) = read_wxa_dense2d(&path)
            .with_context(|| format!("read existing WXA metadata {}", path.display()))?;
        for hour in meta.forecast_hours {
            if !incoming_hours.contains(&hour) {
                merged_grids.push(read_spatial_wxa_grid(&path, hour).with_context(|| {
                    format!("read existing WXA grid {} f{hour:03}", path.display())
                })?);
            }
        }
    }
    merged_grids.extend(grids.iter().cloned());
    merged_grids.sort_by_key(|grid| grid.forecast_hour);
    let grids = merged_grids;
    let first = grids
        .first()
        .ok_or_else(|| anyhow!("cannot write empty WXA product"))?;
    for grid in &grids {
        if grid.nx != first.nx || grid.ny != first.ny {
            bail!("all WXA grids for a product must share dimensions");
        }
    }

    let cy = WXA_SPATIAL_CHUNK_Y.min(first.ny);
    let cx = WXA_SPATIAL_CHUNK_X.min(first.nx);
    let n_chunks_y = first.ny.div_ceil(cy);
    let n_chunks_x = first.nx.div_ceil(cx);

    let mut records = Vec::<WxaDense2dIndexRecord>::new();
    let mut payload = Vec::<u8>::new();
    for grid in &grids {
        for chunk_y in 0..n_chunks_y {
            for chunk_x in 0..n_chunks_x {
                let y0 = chunk_y * cy;
                let x0 = chunk_x * cx;
                let y1 = (y0 + cy).min(grid.ny);
                let x1 = (x0 + cx).min(grid.nx);
                let y_count = y1 - y0;
                let x_count = x1 - x0;
                let mut raw = Vec::with_capacity(y_count * x_count * 4);
                let mut min = f32::INFINITY;
                let mut max = f32::NEG_INFINITY;
                let mut valid_count = 0u32;
                for yy in 0..y_count {
                    for xx in 0..x_count {
                        let value = grid.values[(y0 + yy) * grid.nx + (x0 + xx)];
                        if value.is_finite() {
                            min = min.min(value);
                            max = max.max(value);
                            valid_count += 1;
                        }
                        raw.extend_from_slice(&value.to_le_bytes());
                    }
                }
                if valid_count == 0 {
                    min = f32::NAN;
                    max = f32::NAN;
                }
                let compressed =
                    zstd::stream::encode_all(raw.as_slice(), 1).with_context(|| {
                        format!("compress WXA {product} f{:03}", grid.forecast_hour)
                    })?;
                let offset = payload.len();
                let len = compressed.len();
                payload.extend_from_slice(&compressed);
                records.push(WxaDense2dIndexRecord {
                    forecast_hour: grid.forecast_hour,
                    chunk_y,
                    chunk_x,
                    y_count,
                    x_count,
                    raw_len: raw.len(),
                    offset,
                    len,
                    min,
                    max,
                    valid_count,
                });
            }
        }
    }

    let mut forecast_hours = grids
        .iter()
        .map(|grid| grid.forecast_hour)
        .collect::<Vec<_>>();
    forecast_hours.sort_unstable();
    forecast_hours.dedup();
    let meta = WxaDense2dMeta {
        schema: "wxstore.wxa.dense2d.v1".to_string(),
        model: model.to_string(),
        run: run.to_string(),
        member: member.map(str::to_string),
        variable: product.to_string(),
        units: first.units.clone(),
        nx: first.nx,
        ny: first.ny,
        forecast_hours,
        chunk_y: cy,
        chunk_x: cx,
        dtype: "f32_le".to_string(),
        codec: "zstd_level_1".to_string(),
        grid: first.grid_meta(),
    };
    let meta_bytes = serde_json::to_vec(&meta)?;
    let index_offset = WXA_DENSE2D_HEADER_LEN + meta_bytes.len();
    let payload_offset = index_offset + records.len() * WXA_DENSE2D_INDEX_RECORD_LEN;

    let mut output = Vec::with_capacity(payload_offset + payload.len());
    output.extend_from_slice(WXA_DENSE2D_MAGIC);
    output.extend_from_slice(&WXA_DENSE2D_VERSION.to_le_bytes());
    output.extend_from_slice(&(meta_bytes.len() as u32).to_le_bytes());
    output.extend_from_slice(&(records.len() as u64).to_le_bytes());
    output.extend_from_slice(&(index_offset as u64).to_le_bytes());
    output.extend_from_slice(&(payload_offset as u64).to_le_bytes());
    output.resize(WXA_DENSE2D_HEADER_LEN, 0);
    output.extend_from_slice(&meta_bytes);
    for record in &records {
        output.extend_from_slice(&record.forecast_hour.to_le_bytes());
        output.extend_from_slice(&(record.chunk_y as u32).to_le_bytes());
        output.extend_from_slice(&(record.chunk_x as u32).to_le_bytes());
        output.extend_from_slice(&(record.y_count as u32).to_le_bytes());
        output.extend_from_slice(&(record.x_count as u32).to_le_bytes());
        output.extend_from_slice(&(record.raw_len as u32).to_le_bytes());
        output.extend_from_slice(&((payload_offset + record.offset) as u64).to_le_bytes());
        output.extend_from_slice(&(record.len as u64).to_le_bytes());
        output.extend_from_slice(&record.min.to_le_bytes());
        output.extend_from_slice(&record.max.to_le_bytes());
        output.extend_from_slice(&record.valid_count.to_le_bytes());
        output.resize(output.len() + 12, 0);
    }
    output.extend_from_slice(&payload);
    fs::write(&tmp_path, output)?;
    fs::rename(&tmp_path, &path)?;
    Ok(path)
}

fn parse_tile_y(value: &str) -> Result<u32> {
    let clean = value.strip_suffix(".png").unwrap_or(value);
    clean.parse::<u32>().context("parse tile y")
}

fn parse_frame_hour(frame: &str) -> Result<u32> {
    let clean = frame.strip_prefix('f').unwrap_or(frame);
    clean
        .parse::<u32>()
        .with_context(|| format!("parse frame '{frame}'"))
}

fn parse_range_pair(value: &str) -> Option<(f32, f32)> {
    let (min, max) = value.split_once(',')?;
    let min = min.trim().parse::<f32>().ok()?;
    let max = max.trim().parse::<f32>().ok()?;
    (max > min).then_some((min, max))
}

fn available_hours_for_layer(
    state: &AppState,
    model: &str,
    run: &str,
    member: Option<&str>,
    variable: &str,
) -> Result<Vec<u32>> {
    if let Some(spatial) = state.spatial.as_deref() {
        if let Ok(resolved_run) = spatial.resolve_run(model, Some(run)) {
            if let Ok(hours) = spatial.available_hours_for(model, &resolved_run, member, variable) {
                return Ok(hours);
            }
        }
    }
    if model == state.profile.manifest.model
        && (run == "latest"
            || run == state.profile.manifest.run_id
            || run == state.profile.manifest.cycle)
        && parse_pressure_grid_product(variable, &state.profile.manifest.levels_hpa).is_some()
    {
        return Ok(state
            .profile
            .manifest
            .forecast_hours
            .iter()
            .map(|hour| u32::from(*hour))
            .collect());
    }
    bail!("no tile hours are available for {model}/{run}/{variable}");
}

fn render_raster_tile_png(
    grid: &SpatialGrid,
    z: u32,
    x: u32,
    y: u32,
    query: &TileQuery,
) -> Result<Vec<u8>> {
    if z > 14 {
        bail!("max raster tile zoom is 14 for this proof renderer");
    }
    let tile_size = 256usize;
    let (min, max) = match (query.min, query.max) {
        (Some(min), Some(max)) if max > min => (min, max),
        _ => default_range_for_variable(&grid.variable, grid.values.as_ref()),
    };
    let palette = query
        .palette
        .as_deref()
        .unwrap_or_else(|| default_palette_for_variable(&grid.variable));
    let mut rgba = vec![0u8; tile_size * tile_size * 4];
    for py in 0..tile_size {
        for px in 0..tile_size {
            let (lon, lat) = web_mercator_tile_lon_lat(z, x, y, px, py, tile_size);
            let Some(index) = grid_index_for_latlon(grid, lat, lon) else {
                continue;
            };
            let value = grid.values.get(index).copied().unwrap_or(f32::NAN);
            let color = color_for_tile_value(value, min, max, palette, query);
            let dst = (py * tile_size + px) * 4;
            rgba[dst..dst + 4].copy_from_slice(&color);
        }
    }
    encode_png_rgba(tile_size as u32, tile_size as u32, &rgba)
}

fn encode_png_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgba)?;
    }
    Ok(out)
}

fn web_mercator_tile_lon_lat(
    z: u32,
    x: u32,
    y: u32,
    px: usize,
    py: usize,
    tile_size: usize,
) -> (f64, f64) {
    let n = 2.0_f64.powi(z as i32);
    let fx = (x as f64 + (px as f64 + 0.5) / tile_size as f64) / n;
    let fy = (y as f64 + (py as f64 + 0.5) / tile_size as f64) / n;
    let lon = fx * 360.0 - 180.0;
    let lat_rad = (std::f64::consts::PI * (1.0 - 2.0 * fy)).sinh().atan();
    (lon, lat_rad.to_degrees())
}

fn grid_index_for_latlon(grid: &SpatialGrid, lat: f64, lon: f64) -> Option<usize> {
    if grid.grid_meta.get("type").and_then(Value::as_str) == Some("hrrr_lambert_crop") {
        let hrrr = HrrrLambert::default();
        let x_start = grid.grid_meta.get("x_start").and_then(Value::as_u64)? as isize;
        let y_start = grid.grid_meta.get("y_start").and_then(Value::as_u64)? as isize;
        let (xf, yf) = hrrr.project_relative(lat, lon);
        let full_x = (xf / hrrr.dx).round() as isize;
        let full_y_from_south = (yf / hrrr.dy).round() as isize;
        let full_y = (hrrr.ny - 1) as isize - full_y_from_south;
        let x = full_x - x_start;
        let y = full_y - y_start;
        if x < 0 || y < 0 || x >= grid.nx as isize || y >= grid.ny as isize {
            return None;
        }
        return Some(y as usize * grid.nx + x as usize);
    }
    if grid.model == "hrrr" && grid.nx == 1799 && grid.ny == 1059 {
        let hrrr = HrrrLambert::default();
        let (xf, yf) = hrrr.project_relative(lat, lon);
        let x = (xf / hrrr.dx).round();
        let y = (hrrr.ny - 1) as f64 - (yf / hrrr.dy).round();
        if x < 0.0 || y < 0.0 || x > (grid.nx - 1) as f64 || y > (grid.ny - 1) as f64 {
            return None;
        }
        return Some(y as usize * grid.nx + x as usize);
    }
    if let Some(index) = grid_index_from_geographic_meta(grid, lat, lon) {
        return index;
    }
    if !lat.is_finite() || !lon.is_finite() {
        return None;
    }
    let lon_east = if lon < 0.0 { lon + 360.0 } else { lon };
    let x = (lon_east / 360.0 * grid.nx as f64)
        .floor()
        .rem_euclid(grid.nx as f64) as usize;
    let y = ((90.0 - lat) / 180.0 * grid.ny as f64).floor() as isize;
    if y < 0 || y >= grid.ny as isize {
        return None;
    }
    Some(y as usize * grid.nx + x.min(grid.nx - 1))
}

fn grid_index_from_geographic_meta(
    grid: &SpatialGrid,
    lat: f64,
    lon: f64,
) -> Option<Option<usize>> {
    if !lat.is_finite() || !lon.is_finite() {
        return Some(None);
    }
    match grid.grid_meta.get("type").and_then(Value::as_str)? {
        "regular_latlon" => Some(regular_latlon_index(grid, lat, lon)),
        "rectilinear_latlon" => Some(rectilinear_latlon_index(grid, lat, lon)),
        "curvilinear_latlon_sampled" => Some(sampled_curvilinear_index(grid, lat, lon)),
        _ => None,
    }
}

fn regular_latlon_index(grid: &SpatialGrid, lat: f64, lon: f64) -> Option<usize> {
    let lat_start = meta_f64(&grid.grid_meta, "lat_start")?;
    let lon_start = meta_f64(&grid.grid_meta, "lon_start")?;
    let lat_step = meta_f64(&grid.grid_meta, "lat_step").or_else(|| {
        meta_f64(&grid.grid_meta, "lat_end")
            .map(|lat_end| (lat_end - lat_start) / grid.ny.saturating_sub(1).max(1) as f64)
    })?;
    let lon_step = meta_f64(&grid.grid_meta, "lon_step").or_else(|| {
        meta_f64(&grid.grid_meta, "lon_end")
            .map(|lon_end| (lon_end - lon_start) / grid.nx.saturating_sub(1).max(1) as f64)
    })?;
    if lat_step == 0.0 || lon_step == 0.0 {
        return None;
    }
    let y = ((lat - lat_start) / lat_step).round();
    if y < 0.0 || y > (grid.ny.saturating_sub(1)) as f64 {
        return None;
    }
    let mut x = ((unwrap_lon_near(lon, lon_start) - lon_start) / lon_step).round();
    let lon_wrap = grid
        .grid_meta
        .get("lon_wrap")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || (lon_step.abs() * grid.nx as f64 - 360.0).abs() <= lon_step.abs().max(0.01) * 2.0;
    if lon_wrap {
        x = x.rem_euclid(grid.nx as f64);
    } else if x < 0.0 || x > (grid.nx.saturating_sub(1)) as f64 {
        return None;
    }
    let x = x as usize;
    let y = y as usize;
    Some(y * grid.nx + x.min(grid.nx.saturating_sub(1)))
}

fn rectilinear_latlon_index(grid: &SpatialGrid, lat: f64, lon: f64) -> Option<usize> {
    let lat_axis = meta_f64_array(&grid.grid_meta, "lat_axis")?;
    let lon_axis = meta_f64_array(&grid.grid_meta, "lon_axis")?;
    if lat_axis.len() != grid.ny || lon_axis.len() != grid.nx {
        return None;
    }
    let y = nearest_axis_index(&lat_axis, lat, false)?;
    let lon_wrap = grid
        .grid_meta
        .get("lon_wrap")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let x = nearest_axis_index(&lon_axis, lon, lon_wrap)?;
    Some(y * grid.nx + x)
}

fn sampled_curvilinear_index(grid: &SpatialGrid, lat: f64, lon: f64) -> Option<usize> {
    let bounds = grid_bounds(grid);
    if lat < bounds[1] - 0.25 || lat > bounds[3] + 0.25 {
        return None;
    }
    if lon < bounds[0] - 0.25 || lon > bounds[2] + 0.25 {
        return None;
    }
    let sample = grid.grid_meta.get("sample")?.as_object()?;
    let sample_nx = sample.get("nx")?.as_u64()? as usize;
    let sample_ny = sample.get("ny")?.as_u64()? as usize;
    let xs = value_f64_array(sample.get("x")?)?;
    let ys = value_f64_array(sample.get("y")?)?;
    let lats = value_f64_array(sample.get("lat")?)?;
    let lons = value_f64_array(sample.get("lon")?)?;
    if xs.len() != sample_nx
        || ys.len() != sample_ny
        || lats.len() != sample_nx * sample_ny
        || lons.len() != sample_nx * sample_ny
    {
        return None;
    }

    let mut best = None::<(usize, f64)>;
    for (index, (&sample_lat, &sample_lon)) in lats.iter().zip(lons.iter()).enumerate() {
        if !sample_lat.is_finite() || !sample_lon.is_finite() {
            continue;
        }
        let dlat = sample_lat - lat;
        let dlon = normalized_lon_delta(sample_lon - lon) * lat.to_radians().cos().abs().max(0.25);
        let dist2 = dlat * dlat + dlon * dlon;
        if best.is_none_or(|(_, best_dist)| dist2 < best_dist) {
            best = Some((index, dist2));
        }
    }
    let (sample_index, _) = best?;
    let sample_x = sample_index % sample_nx;
    let sample_y = sample_index / sample_nx;
    let x = xs[sample_x]
        .round()
        .clamp(0.0, grid.nx.saturating_sub(1) as f64) as usize;
    let y = ys[sample_y]
        .round()
        .clamp(0.0, grid.ny.saturating_sub(1) as f64) as usize;
    Some(y * grid.nx + x)
}

fn grid_bounds(grid: &SpatialGrid) -> [f64; 4] {
    if let Some(bounds) = grid
        .grid_meta
        .get("bounds")
        .and_then(|value| serde_json::from_value::<[f64; 4]>(value.clone()).ok())
    {
        return bounds;
    }
    if grid.grid_meta.get("type").and_then(Value::as_str) == Some("hrrr_lambert_crop") {
        let hrrr = HrrrLambert::default();
        let x_start = grid
            .grid_meta
            .get("x_start")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let y_start = grid
            .grid_meta
            .get("y_start")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let x_end = x_start + grid.nx.saturating_sub(1);
        let y_end = y_start + grid.ny.saturating_sub(1);
        let corners = [
            hrrr.latlon_at(x_start, y_start),
            hrrr.latlon_at(x_end, y_start),
            hrrr.latlon_at(x_start, y_end),
            hrrr.latlon_at(x_end, y_end),
        ];
        let mut west = f64::INFINITY;
        let mut east = f64::NEG_INFINITY;
        let mut south = f64::INFINITY;
        let mut north = f64::NEG_INFINITY;
        for (lat, lon) in corners {
            west = west.min(lon);
            east = east.max(lon);
            south = south.min(lat);
            north = north.max(lat);
        }
        return [west, south, east, north];
    }
    if grid.model == "hrrr" && grid.nx == 1799 && grid.ny == 1059 {
        let hrrr = HrrrLambert::default();
        let corners = [
            hrrr.latlon_at(0, 0),
            hrrr.latlon_at(grid.nx - 1, 0),
            hrrr.latlon_at(0, grid.ny - 1),
            hrrr.latlon_at(grid.nx - 1, grid.ny - 1),
        ];
        let mut west = f64::INFINITY;
        let mut east = f64::NEG_INFINITY;
        let mut south = f64::INFINITY;
        let mut north = f64::NEG_INFINITY;
        for (lat, lon) in corners {
            west = west.min(lon);
            east = east.max(lon);
            south = south.min(lat);
            north = north.max(lat);
        }
        return [west, south, east, north];
    }
    if matches!(
        grid.grid_meta.get("type").and_then(Value::as_str),
        Some("regular_latlon" | "rectilinear_latlon" | "curvilinear_latlon_sampled")
    ) {
        if let Some(bounds) = grid
            .grid_meta
            .get("bounds")
            .and_then(|value| serde_json::from_value::<[f64; 4]>(value.clone()).ok())
        {
            return bounds;
        }
    }
    [-180.0, -85.05112878, 180.0, 85.05112878]
}

fn grid_latlon_at(grid: &SpatialGrid, x: usize, y: usize) -> (f64, f64) {
    if grid.grid_meta.get("type").and_then(Value::as_str) == Some("hrrr_lambert_crop") {
        let hrrr = HrrrLambert::default();
        let x_start = grid
            .grid_meta
            .get("x_start")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let y_start = grid
            .grid_meta
            .get("y_start")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let full_x = x_start + x;
        let stored_y = y_start + y;
        let projected_y = hrrr.ny.saturating_sub(1).saturating_sub(stored_y);
        return hrrr.latlon_at(full_x, projected_y);
    }
    if grid.model == "hrrr" && grid.nx == 1799 && grid.ny == 1059 {
        let hrrr = HrrrLambert::default();
        let projected_y = hrrr.ny.saturating_sub(1).saturating_sub(y);
        return hrrr.latlon_at(x, projected_y);
    }
    if let Some(latlon) = grid_latlon_from_geographic_meta(grid, x, y) {
        return latlon;
    }
    let lon = x as f64 * 360.0 / grid.nx.max(1) as f64 - 180.0;
    let lat = 90.0 - y as f64 * 180.0 / grid.ny.saturating_sub(1).max(1) as f64;
    (lat, lon)
}

fn grid_latlon_from_geographic_meta(grid: &SpatialGrid, x: usize, y: usize) -> Option<(f64, f64)> {
    if x >= grid.nx || y >= grid.ny {
        return None;
    }
    match grid.grid_meta.get("type").and_then(Value::as_str)? {
        "regular_latlon" => {
            let lat_start = meta_f64(&grid.grid_meta, "lat_start")?;
            let lon_start = meta_f64(&grid.grid_meta, "lon_start")?;
            let lat_step = meta_f64(&grid.grid_meta, "lat_step").or_else(|| {
                meta_f64(&grid.grid_meta, "lat_end")
                    .map(|lat_end| (lat_end - lat_start) / grid.ny.saturating_sub(1).max(1) as f64)
            })?;
            let lon_step = meta_f64(&grid.grid_meta, "lon_step").or_else(|| {
                meta_f64(&grid.grid_meta, "lon_end")
                    .map(|lon_end| (lon_end - lon_start) / grid.nx.saturating_sub(1).max(1) as f64)
            })?;
            let lat = lat_start + y as f64 * lat_step;
            let lon = lon_start + x as f64 * lon_step;
            Some((lat, normalize_lon(lon)))
        }
        "rectilinear_latlon" => {
            let lat_axis = meta_f64_array(&grid.grid_meta, "lat_axis")?;
            let lon_axis = meta_f64_array(&grid.grid_meta, "lon_axis")?;
            Some((*lat_axis.get(y)?, normalize_lon(*lon_axis.get(x)?)))
        }
        "curvilinear_latlon_sampled" => sampled_curvilinear_latlon_at(grid, x, y),
        _ => None,
    }
}

fn sampled_curvilinear_latlon_at(grid: &SpatialGrid, x: usize, y: usize) -> Option<(f64, f64)> {
    let sample = grid.grid_meta.get("sample")?.as_object()?;
    let sample_nx = sample.get("nx")?.as_u64()? as usize;
    let sample_ny = sample.get("ny")?.as_u64()? as usize;
    let xs = value_f64_array(sample.get("x")?)?;
    let ys = value_f64_array(sample.get("y")?)?;
    let lats = value_f64_array(sample.get("lat")?)?;
    let lons = value_f64_array(sample.get("lon")?)?;
    if sample_nx < 2
        || sample_ny < 2
        || xs.len() != sample_nx
        || ys.len() != sample_ny
        || lats.len() != sample_nx * sample_ny
        || lons.len() != sample_nx * sample_ny
    {
        return None;
    }
    let sx1 = upper_axis_index(&xs, x as f64).clamp(1, sample_nx - 1);
    let sy1 = upper_axis_index(&ys, y as f64).clamp(1, sample_ny - 1);
    let sx0 = sx1 - 1;
    let sy0 = sy1 - 1;
    let tx = fraction_between(xs[sx0], xs[sx1], x as f64);
    let ty = fraction_between(ys[sy0], ys[sy1], y as f64);
    let i00 = sy0 * sample_nx + sx0;
    let i10 = sy0 * sample_nx + sx1;
    let i01 = sy1 * sample_nx + sx0;
    let i11 = sy1 * sample_nx + sx1;
    let lat = bilerp(lats[i00], lats[i10], lats[i01], lats[i11], tx, ty);
    let lon00 = lons[i00];
    let lon = bilerp(
        lon00,
        lon00 + normalized_lon_delta(lons[i10] - lon00),
        lon00 + normalized_lon_delta(lons[i01] - lon00),
        lon00 + normalized_lon_delta(lons[i11] - lon00),
        tx,
        ty,
    );
    Some((lat, normalize_lon(lon)))
}

fn meta_f64(meta: &Value, key: &str) -> Option<f64> {
    meta.get(key).and_then(Value::as_f64)
}

fn meta_f64_array(meta: &Value, key: &str) -> Option<Vec<f64>> {
    value_f64_array(meta.get(key)?)
}

fn value_f64_array(value: &Value) -> Option<Vec<f64>> {
    value
        .as_array()?
        .iter()
        .map(Value::as_f64)
        .collect::<Option<Vec<_>>>()
}

fn nearest_axis_index(axis: &[f64], value: f64, wraps: bool) -> Option<usize> {
    if axis.is_empty() || !value.is_finite() {
        return None;
    }
    if axis.len() == 1 {
        return Some(0);
    }
    let mut target = if wraps {
        unwrap_lon_near(value, axis[0])
    } else {
        value
    };
    let increasing = axis[axis.len() - 1] >= axis[0];
    let first_step = (axis[1] - axis[0]).abs();
    let last_step = (axis[axis.len() - 1] - axis[axis.len() - 2]).abs();
    let lower = axis[0].min(axis[axis.len() - 1]) - first_step.max(last_step) * 0.5;
    let upper = axis[0].max(axis[axis.len() - 1]) + first_step.max(last_step) * 0.5;
    if wraps {
        while target < lower {
            target += 360.0;
        }
        while target > upper {
            target -= 360.0;
        }
    }
    if target < lower || target > upper {
        return None;
    }

    let mut lo = 0usize;
    let mut hi = axis.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if (increasing && axis[mid] < target) || (!increasing && axis[mid] > target) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    let mut best = lo.min(axis.len() - 1);
    if lo > 0 && (axis[lo - 1] - target).abs() <= (axis[best] - target).abs() {
        best = lo - 1;
    }
    Some(best)
}

fn upper_axis_index(axis: &[f64], value: f64) -> usize {
    if axis.len() < 2 {
        return 0;
    }
    let increasing = axis[axis.len() - 1] >= axis[0];
    let mut lo = 0usize;
    let mut hi = axis.len();
    while lo < hi {
        let mid = (lo + hi) / 2;
        if (increasing && axis[mid] < value) || (!increasing && axis[mid] > value) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    lo.min(axis.len() - 1)
}

fn fraction_between(a: f64, b: f64, value: f64) -> f64 {
    if (b - a).abs() < f64::EPSILON {
        0.0
    } else {
        ((value - a) / (b - a)).clamp(0.0, 1.0)
    }
}

fn bilerp(v00: f64, v10: f64, v01: f64, v11: f64, tx: f64, ty: f64) -> f64 {
    let top = v00 + (v10 - v00) * tx;
    let bottom = v01 + (v11 - v01) * tx;
    top + (bottom - top) * ty
}

fn unwrap_lon_near(mut lon: f64, reference: f64) -> f64 {
    while lon - reference > 180.0 {
        lon -= 360.0;
    }
    while lon - reference <= -180.0 {
        lon += 360.0;
    }
    lon
}

fn wind_query_bounds(query: &WindFieldQuery) -> Result<Option<[f64; 4]>, ApiError> {
    if let Some(bounds) = query.bounds.as_deref() {
        let parts = bounds
            .split(',')
            .map(str::trim)
            .map(str::parse::<f64>)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| bad_request(format!("invalid bounds: {err}")))?;
        if parts.len() != 4 {
            return Err(bad_request("bounds must be west,south,east,north"));
        }
        return Ok(Some([parts[0], parts[1], parts[2], parts[3]]));
    }
    match (query.west, query.south, query.east, query.north) {
        (Some(west), Some(south), Some(east), Some(north)) => Ok(Some([west, south, east, north])),
        (None, None, None, None) => Ok(None),
        _ => Err(bad_request(
            "provide all of west,south,east,north or use bounds=west,south,east,north",
        )),
    }
}

fn resolve_export_path(manifest_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        manifest_dir.join(path)
    }
}

fn read_f32_file(path: &Path) -> Result<Vec<f32>> {
    let bytes = fs::read(path)?;
    if bytes.len() % 4 != 0 {
        bail!("{} byte length is not divisible by four", path.display());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|item| f32::from_le_bytes([item[0], item[1], item[2], item[3]]))
        .collect())
}

fn grid_meta_from_latlon(
    model: &str,
    nx: usize,
    ny: usize,
    lat: &[f32],
    lon: &[f32],
    record: &RustwxGridExportRecord,
) -> Value {
    if model == "hrrr" && !lat.is_empty() && lat.len() == nx * ny && lon.len() == nx * ny {
        let hrrr = HrrrLambert::default();
        let (x_start, y_start, x_end, y_end) = if let Some(crop) = record.crop {
            (crop.x_start, crop.y_start, crop.x_end, crop.y_end)
        } else if nx == hrrr.nx && ny == hrrr.ny {
            (0, 0, hrrr.nx, hrrr.ny)
        } else {
            let (xf, yf) = hrrr.project_relative(lat[0] as f64, lon[0] as f64);
            let x_start = (xf / hrrr.dx).round().max(0.0) as usize;
            let y_start = (yf / hrrr.dy).round().max(0.0) as usize;
            (x_start, y_start, x_start + nx, y_start + ny)
        };
        let bounds = record
            .bounds
            .map(normalize_bounds)
            .or_else(|| bounds_from_latlon(lat, lon));
        return json!({
            "type": "hrrr_lambert_crop",
            "nx": nx,
            "ny": ny,
            "full_nx": hrrr.nx,
            "full_ny": hrrr.ny,
            "x_start": x_start,
            "y_start": y_start,
            "x_end": x_end,
            "y_end": y_end,
            "bounds": bounds,
            "lat1": hrrr.lat1,
            "lon1": hrrr.lon1,
            "dx_m": hrrr.dx,
            "dy_m": hrrr.dy,
            "latin1": hrrr.latin1,
            "latin2": hrrr.latin2,
            "lov": hrrr.lov
        });
    }
    if !lat.is_empty() && lat.len() == nx * ny && lon.len() == nx * ny {
        let bounds = record
            .bounds
            .map(normalize_bounds)
            .or_else(|| bounds_from_latlon(lat, lon));
        let corners = corners_from_latlon(nx, ny, lat, lon);
        if let Some((lat_axis, lon_axis)) = rectilinear_axes_from_latlon(nx, ny, lat, lon) {
            let lon_wrap = longitude_axis_wraps(&lon_axis);
            if let (Some(lat_step), Some(lon_step)) =
                (linear_axis_step(&lat_axis), linear_axis_step(&lon_axis))
            {
                return json!({
                    "type": "regular_latlon",
                    "nx": nx,
                    "ny": ny,
                    "bounds": bounds,
                    "corners": corners,
                    "lat_start": lat_axis[0],
                    "lat_end": lat_axis[lat_axis.len() - 1],
                    "lon_start": lon_axis[0],
                    "lon_end": lon_axis[lon_axis.len() - 1],
                    "lat_step": lat_step,
                    "lon_step": lon_step,
                    "lon_wrap": lon_wrap,
                    "monotonic": {
                        "lat_y": axis_direction(&lat_axis),
                        "lon_x": axis_direction(&lon_axis)
                    },
                    "sample_strategy": "regular_nearest"
                });
            }
            return json!({
                "type": "rectilinear_latlon",
                "nx": nx,
                "ny": ny,
                "bounds": bounds,
                "corners": corners,
                "lat_axis": lat_axis,
                "lon_axis": lon_axis,
                "lon_wrap": lon_wrap,
                "monotonic": {
                    "lat_y": axis_direction(&lat_axis),
                    "lon_x": axis_direction(&lon_axis)
                },
                "sample_strategy": "rectilinear_nearest"
            });
        }
        return sampled_curvilinear_meta(nx, ny, lat, lon, bounds, corners);
    }
    spatial_grid_meta(model, nx, ny)
}

fn rectilinear_axes_from_latlon(
    nx: usize,
    ny: usize,
    lat: &[f32],
    lon: &[f32],
) -> Option<(Vec<f64>, Vec<f64>)> {
    if nx == 0 || ny == 0 || lat.len() != nx * ny || lon.len() != nx * ny {
        return None;
    }
    let tolerance = 0.01;
    let mut lat_axis = Vec::with_capacity(ny);
    for y in 0..ny {
        let row = &lat[y * nx..(y + 1) * nx];
        let first = row.first().copied()? as f64;
        if !first.is_finite() {
            return None;
        }
        if row
            .iter()
            .any(|value| !value.is_finite() || ((*value as f64) - first).abs() > tolerance)
        {
            return None;
        }
        lat_axis.push(first);
    }
    let mut lon_axis = Vec::with_capacity(nx);
    for x in 0..nx {
        let value = lon[x] as f64;
        if !value.is_finite() {
            return None;
        }
        let reference = lon_axis.last().copied().unwrap_or(value);
        lon_axis.push(unwrap_lon_near(value, reference));
    }
    for y in 0..ny {
        for x in 0..nx {
            let index = y * nx + x;
            if ((lat[index] as f64) - lat_axis[y]).abs() > tolerance
                || normalized_lon_delta(lon[index] as f64 - lon_axis[x]).abs() > tolerance
            {
                return None;
            }
        }
    }
    if !axis_is_monotonic(&lat_axis) || !axis_is_monotonic(&lon_axis) {
        return None;
    }
    Some((lat_axis, lon_axis))
}

fn axis_is_monotonic(axis: &[f64]) -> bool {
    if axis.len() < 2 {
        return true;
    }
    let increasing = axis[axis.len() - 1] >= axis[0];
    axis.windows(2).all(|pair| {
        if increasing {
            pair[1] >= pair[0]
        } else {
            pair[1] <= pair[0]
        }
    })
}

fn axis_direction(axis: &[f64]) -> &'static str {
    if axis.len() < 2 {
        "constant"
    } else if axis[axis.len() - 1] >= axis[0] {
        "increasing"
    } else {
        "decreasing"
    }
}

fn linear_axis_step(axis: &[f64]) -> Option<f64> {
    if axis.len() < 2 {
        return Some(0.0);
    }
    let step = (axis[axis.len() - 1] - axis[0]) / axis.len().saturating_sub(1) as f64;
    if step == 0.0 {
        return None;
    }
    let tolerance = step.abs().max(1.0) * 0.001;
    axis.iter()
        .enumerate()
        .all(|(index, value)| (*value - (axis[0] + index as f64 * step)).abs() <= tolerance)
        .then_some(step)
}

fn longitude_axis_wraps(axis: &[f64]) -> bool {
    if axis.len() < 2 {
        return false;
    }
    let span = (axis[axis.len() - 1] - axis[0]).abs();
    let step = span / axis.len().saturating_sub(1) as f64;
    (span + step - 360.0).abs() <= step.max(0.01) * 2.0
}

fn sampled_curvilinear_meta(
    nx: usize,
    ny: usize,
    lat: &[f32],
    lon: &[f32],
    bounds: Option<[f64; 4]>,
    corners: Value,
) -> Value {
    let xs = sample_positions(nx, 33);
    let ys = sample_positions(ny, 33);
    let mut sample_lat = Vec::with_capacity(xs.len() * ys.len());
    let mut sample_lon = Vec::with_capacity(xs.len() * ys.len());
    for &y in &ys {
        for &x in &xs {
            let index = y * nx + x;
            sample_lat.push(lat[index] as f64);
            sample_lon.push(normalize_lon(lon[index] as f64));
        }
    }
    json!({
        "type": "curvilinear_latlon_sampled",
        "nx": nx,
        "ny": ny,
        "bounds": bounds,
        "corners": corners,
        "monotonic": edge_monotonic_from_latlon(nx, ny, lat, lon),
        "sample_strategy": "sampled_control_mesh_nearest",
        "sample": {
            "nx": xs.len(),
            "ny": ys.len(),
            "x": xs,
            "y": ys,
            "lat": sample_lat,
            "lon": sample_lon
        }
    })
}

fn edge_monotonic_from_latlon(nx: usize, ny: usize, lat: &[f32], lon: &[f32]) -> Value {
    let top_lon = (0..nx)
        .map(|x| unwrap_lon_near(lon[x] as f64, lon[0] as f64))
        .collect::<Vec<_>>();
    let bottom_offset = ny.saturating_sub(1) * nx;
    let bottom_lon = (0..nx)
        .map(|x| unwrap_lon_near(lon[bottom_offset + x] as f64, lon[bottom_offset] as f64))
        .collect::<Vec<_>>();
    let left_lat = (0..ny).map(|y| lat[y * nx] as f64).collect::<Vec<_>>();
    let right_lat = (0..ny)
        .map(|y| lat[y * nx + nx.saturating_sub(1)] as f64)
        .collect::<Vec<_>>();
    json!({
        "top_lon_x": axis_is_monotonic(&top_lon).then(|| axis_direction(&top_lon)),
        "bottom_lon_x": axis_is_monotonic(&bottom_lon).then(|| axis_direction(&bottom_lon)),
        "left_lat_y": axis_is_monotonic(&left_lat).then(|| axis_direction(&left_lat)),
        "right_lat_y": axis_is_monotonic(&right_lat).then(|| axis_direction(&right_lat))
    })
}

fn sample_positions(len: usize, max_count: usize) -> Vec<usize> {
    if len == 0 {
        return Vec::new();
    }
    if len <= max_count {
        return (0..len).collect();
    }
    let count = max_count.max(2);
    let mut positions = Vec::with_capacity(count);
    for index in 0..count {
        let value = (index as f64 * (len - 1) as f64 / (count - 1) as f64).round() as usize;
        if positions.last().copied() != Some(value) {
            positions.push(value);
        }
    }
    positions
}

fn corners_from_latlon(nx: usize, ny: usize, lat: &[f32], lon: &[f32]) -> Value {
    let point = |x: usize, y: usize| {
        let index = y * nx + x;
        json!({"lat": lat[index] as f64, "lon": normalize_lon(lon[index] as f64), "x": x, "y": y})
    };
    json!({
        "top_left": point(0, 0),
        "top_right": point(nx.saturating_sub(1), 0),
        "bottom_left": point(0, ny.saturating_sub(1)),
        "bottom_right": point(nx.saturating_sub(1), ny.saturating_sub(1))
    })
}

fn bounds_from_latlon(lat: &[f32], lon: &[f32]) -> Option<[f64; 4]> {
    let mut west = f64::INFINITY;
    let mut east = f64::NEG_INFINITY;
    let mut south = f64::INFINITY;
    let mut north = f64::NEG_INFINITY;
    let mut found = false;
    for (&lat, &lon) in lat.iter().zip(lon) {
        let lat = lat as f64;
        let lon = normalize_lon(lon as f64);
        if lat.is_finite() && lon.is_finite() {
            west = west.min(lon);
            east = east.max(lon);
            south = south.min(lat);
            north = north.max(lat);
            found = true;
        }
    }
    found.then_some([west, south, east, north])
}

fn normalize_bounds(bounds: [f64; 4]) -> [f64; 4] {
    let west = normalize_lon(bounds[0]);
    let east = normalize_lon(bounds[2]);
    [
        west.min(east),
        bounds[1].min(bounds[3]),
        west.max(east),
        bounds[1].max(bounds[3]),
    ]
}

fn default_range_for_variable(variable: &str, values: &[f32]) -> (f32, f32) {
    let lower = variable.to_ascii_lowercase();
    if lower.contains("rh") || lower.contains("humidity") && !lower.contains("specific") {
        return (0.0, 100.0);
    }
    if lower.contains("vpd") {
        return (0.0, 40.0);
    }
    if lower.contains("fire_weather") {
        return (0.0, 100.0);
    }
    if lower.contains("temperature")
        || lower.contains("dewpoint")
        || lower.contains("heat_index")
        || lower.contains("wind_chill")
    {
        return (-35.0, 45.0);
    }
    if lower.contains("wind") {
        return (0.0, 45.0);
    }
    if lower.contains("cape") {
        return (0.0, 5000.0);
    }
    if lower.contains("precip") || lower.contains("qpf") {
        return (0.0, 75.0);
    }
    if lower.contains("visibility") {
        return (0.0, 16093.0);
    }
    if lower.contains("height") {
        return finite_min_max(values).unwrap_or((0.0, 12000.0));
    }
    finite_min_max(values).unwrap_or((0.0, 1.0))
}

fn finite_min_max(values: &[f32]) -> Option<(f32, f32)> {
    let mut min = f32::INFINITY;
    let mut max = f32::NEG_INFINITY;
    for value in values.iter().copied().filter(|value| value.is_finite()) {
        min = min.min(value);
        max = max.max(value);
    }
    if min.is_finite() && max.is_finite() && max > min {
        Some((min, max))
    } else {
        None
    }
}

fn default_palette_for_variable(variable: &str) -> &'static str {
    let lower = variable.to_ascii_lowercase();
    if lower.contains("rh") || lower.contains("humidity") && !lower.contains("specific") {
        "humidity"
    } else if lower.contains("vpd") {
        "vpd"
    } else if lower.contains("fire_weather") {
        "fire_weather"
    } else if lower.contains("cape") || lower.contains("precip") || lower.contains("qpf") {
        "magma"
    } else if lower.contains("wind") {
        "wind"
    } else {
        "temperature"
    }
}

fn default_transparent_below_for_variable(variable: &str) -> Option<f32> {
    let lower = variable.to_ascii_lowercase();
    if lower.contains("smoke_pm25") {
        Some(2.0)
    } else if lower.contains("smoke_column") {
        Some(5.0)
    } else if lower.contains("qpf") || lower.contains("precip") {
        Some(0.01)
    } else {
        None
    }
}

fn default_transparent_above_for_variable(variable: &str) -> Option<f32> {
    let lower = variable.to_ascii_lowercase();
    if lower.contains("visibility") {
        Some(20.0)
    } else {
        None
    }
}

fn color_for_value(value: f32, min: f32, max: f32, palette: &str) -> [u8; 4] {
    if !value.is_finite() || max <= min {
        return [0, 0, 0, 0];
    }
    let t = ((value - min) / (max - min)).clamp(0.0, 1.0);
    let stops: &[[u8; 3]] = match palette {
        "humidity" => &[
            [120, 72, 32],
            [214, 180, 92],
            [120, 190, 110],
            [20, 120, 90],
            [15, 70, 110],
        ],
        "vpd" => &[
            [24, 90, 145],
            [39, 129, 172],
            [67, 164, 184],
            [110, 190, 168],
            [154, 211, 142],
            [196, 226, 126],
            [229, 232, 126],
            [247, 219, 118],
            [248, 195, 102],
            [240, 163, 85],
            [226, 130, 72],
            [207, 100, 65],
            [184, 74, 61],
            [157, 53, 60],
            [128, 37, 63],
        ],
        "fire_weather" | "fire" => &[
            [34, 139, 34],
            [50, 205, 50],
            [120, 230, 60],
            [173, 255, 47],
            [255, 215, 0],
            [255, 170, 0],
            [255, 140, 0],
            [255, 69, 0],
            [204, 0, 0],
            [139, 0, 0],
        ],
        "magma" => &[
            [18, 10, 38],
            [78, 18, 90],
            [150, 38, 85],
            [220, 87, 50],
            [252, 190, 75],
        ],
        "wind" => &[
            [238, 245, 255],
            [127, 184, 214],
            [62, 146, 135],
            [230, 190, 80],
            [190, 70, 60],
        ],
        "gray" | "grey" => &[
            [30, 30, 30],
            [90, 90, 90],
            [150, 150, 150],
            [210, 210, 210],
            [250, 250, 250],
        ],
        _ => &[
            [52, 84, 180],
            [42, 170, 220],
            [70, 180, 110],
            [245, 210, 70],
            [210, 60, 50],
        ],
    };
    let scaled = t * (stops.len() - 1) as f32;
    let i = scaled.floor() as usize;
    let j = (i + 1).min(stops.len() - 1);
    let local = scaled - i as f32;
    let mut out = [0u8; 4];
    for channel in 0..3 {
        out[channel] = (stops[i][channel] as f32 * (1.0 - local) + stops[j][channel] as f32 * local)
            .round()
            .clamp(0.0, 255.0) as u8;
    }
    out[3] = 210;
    out
}

fn color_for_tile_value(
    value: f32,
    min: f32,
    max: f32,
    palette: &str,
    query: &TileQuery,
) -> [u8; 4] {
    if !value.is_finite()
        || query
            .transparent_below
            .is_some_and(|threshold| value < threshold)
        || query
            .transparent_above
            .is_some_and(|threshold| value > threshold)
    {
        return [0, 0, 0, 0];
    }
    let mut color = color_for_value(value, min, max, palette);
    color[3] = query.alpha.unwrap_or(color[3]);
    color
}

fn read_spatial_wxa_grid(path: &Path, forecast_hour: u32) -> Result<SpatialGrid> {
    let (bytes, meta, index) = read_wxa_dense2d(path)?;
    let mut values = vec![f32::NAN; meta.nx * meta.ny];
    let mut found = false;
    for record in index
        .iter()
        .filter(|record| record.forecast_hour == forecast_hour)
    {
        let end = record.offset + record.len;
        if end > bytes.len() {
            bail!("WXA chunk exceeds file length");
        }
        let decoded = zstd::stream::decode_all(&bytes[record.offset..end])
            .with_context(|| format!("decompress WXA chunk f{forecast_hour:03}"))?;
        if decoded.len() != record.raw_len {
            bail!(
                "WXA chunk raw length mismatch: got {}, expected {}",
                decoded.len(),
                record.raw_len
            );
        }
        let y0 = record.chunk_y * meta.chunk_y;
        let x0 = record.chunk_x * meta.chunk_x;
        let mut src = 0usize;
        for yy in 0..record.y_count {
            for xx in 0..record.x_count {
                let dst = (y0 + yy) * meta.nx + (x0 + xx);
                values[dst] = f32::from_le_bytes([
                    decoded[src],
                    decoded[src + 1],
                    decoded[src + 2],
                    decoded[src + 3],
                ]);
                src += 4;
            }
        }
        found = true;
    }
    if !found {
        bail!(
            "forecast hour f{forecast_hour:03} is not available in {}",
            path.display()
        );
    }
    Ok(SpatialGrid {
        model: meta.model,
        run_id: meta.run,
        member: meta.member,
        variable: meta.variable,
        units: meta.units,
        forecast_hour,
        nx: meta.nx,
        ny: meta.ny,
        grid_meta: meta.grid,
        values: Arc::new(values),
    })
}

fn read_wxa_dense2d(path: &Path) -> Result<(Vec<u8>, WxaDense2dMeta, Vec<WxaDense2dIndexRecord>)> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let header = parse_wxa_dense2d_header(&bytes)?;
    let meta_end = WXA_DENSE2D_HEADER_LEN + header.metadata_len;
    if meta_end > bytes.len() {
        bail!("WXA metadata exceeds file length");
    }
    let meta: WxaDense2dMeta = serde_json::from_slice(&bytes[WXA_DENSE2D_HEADER_LEN..meta_end])
        .with_context(|| format!("parse WXA metadata {}", path.display()))?;
    let index_end = header.index_offset + header.index_count * WXA_DENSE2D_INDEX_RECORD_LEN;
    if index_end > bytes.len() || header.payload_offset > bytes.len() {
        bail!("WXA index exceeds file length");
    }
    let mut records = Vec::with_capacity(header.index_count);
    let mut offset = header.index_offset;
    for _ in 0..header.index_count {
        records.push(WxaDense2dIndexRecord {
            forecast_hour: u32_from(&bytes[offset..offset + 4])?,
            chunk_y: u32_from(&bytes[offset + 4..offset + 8])? as usize,
            chunk_x: u32_from(&bytes[offset + 8..offset + 12])? as usize,
            y_count: u32_from(&bytes[offset + 12..offset + 16])? as usize,
            x_count: u32_from(&bytes[offset + 16..offset + 20])? as usize,
            raw_len: u32_from(&bytes[offset + 20..offset + 24])? as usize,
            offset: u64_from(&bytes[offset + 24..offset + 32])? as usize,
            len: u64_from(&bytes[offset + 32..offset + 40])? as usize,
            min: f32_from(&bytes[offset + 40..offset + 44])?,
            max: f32_from(&bytes[offset + 44..offset + 48])?,
            valid_count: u32_from(&bytes[offset + 48..offset + 52])?,
        });
        offset += WXA_DENSE2D_INDEX_RECORD_LEN;
    }
    Ok((bytes, meta, records))
}

fn parse_wxa_dense2d_header(bytes: &[u8]) -> Result<WxaDense2dHeader> {
    if bytes.len() < WXA_DENSE2D_HEADER_LEN {
        bail!("file too short for WXA header");
    }
    if &bytes[0..8] != WXA_DENSE2D_MAGIC {
        bail!("bad WXA dense2d magic");
    }
    let version = u32_from(&bytes[8..12])?;
    if version != WXA_DENSE2D_VERSION {
        bail!("unsupported WXA dense2d version {version}");
    }
    let header = WxaDense2dHeader {
        metadata_len: u32_from(&bytes[12..16])? as usize,
        index_count: u64_from(&bytes[16..24])? as usize,
        index_offset: u64_from(&bytes[24..32])? as usize,
        payload_offset: u64_from(&bytes[32..40])? as usize,
    };
    if header.index_offset < WXA_DENSE2D_HEADER_LEN || header.payload_offset < header.index_offset {
        bail!("invalid WXA dense2d offsets");
    }
    Ok(header)
}

fn compress_zlib(data: &[u8], level: u32) -> Result<Vec<u8>> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

#[allow(dead_code)]
fn locate_spatial_point(
    model: &str,
    nx: usize,
    ny: usize,
    lat: f64,
    lon: f64,
) -> Result<GridPoint> {
    if !lat.is_finite() || !lon.is_finite() {
        bail!("lat/lon must be finite");
    }
    let (x, y, grid_lat, grid_lon) = if model == "hrrr" && nx == 1799 && ny == 1059 {
        let grid = HrrrLambert::default();
        let (x, y) = grid.nearest(lat, lon);
        let (grid_lat, grid_lon) = grid.latlon_at(x, y);
        (x, y, grid_lat, grid_lon)
    } else {
        let lon_east = if lon < 0.0 { lon + 360.0 } else { lon };
        let x = (lon_east / 360.0 * nx as f64).round().rem_euclid(nx as f64) as usize;
        let y = ((90.0 - lat) / 180.0 * (ny.saturating_sub(1)) as f64)
            .round()
            .clamp(0.0, (ny.saturating_sub(1)) as f64) as usize;
        let grid_lat = 90.0 - y as f64 * 180.0 / (ny.saturating_sub(1)).max(1) as f64;
        let grid_lon = normalize_lon(x as f64 * 360.0 / nx as f64);
        (x, y, grid_lat, grid_lon)
    };
    Ok(GridPoint {
        x,
        y,
        index: y * nx + x,
        lat: grid_lat,
        lon: grid_lon,
        sample: "nearest",
    })
}

fn spatial_grid_meta(model: &str, nx: usize, ny: usize) -> Value {
    if model == "hrrr" && nx == 1799 && ny == 1059 {
        json!({
            "type": "lambert_conformal",
            "nx": nx,
            "ny": ny,
            "lat1": 21.138123,
            "lon1": 237.280472,
            "dx_m": 3000.0,
            "dy_m": 3000.0,
            "latin1": 38.5,
            "latin2": 38.5,
            "lov": 262.5
        })
    } else {
        json!({
            "type": "regular_latlon",
            "nx": nx,
            "ny": ny,
            "lat_start": 90.0,
            "lat_end": -90.0,
            "lon_start": 0.0,
            "lon_end": 360.0 - 360.0 / nx.max(1) as f64
        })
    }
}

fn units_for_variable(variable: &str) -> &'static str {
    if let Some(window) = parse_windowed_product(variable) {
        return units_for_variable(window.raw_variable);
    }
    match variable {
        "2m_temperature"
        | "2m_dewpoint"
        | "temperature_2m"
        | "dew_point_2m"
        | "dewpoint_depression_2m"
        | "heat_index_2m"
        | "wind_chill_2m"
        | "apparent_temperature_2m"
        | "apparent_temperature"
        | "temperature"
        | "dew_point" => "degC",
        "relative_humidity_2m"
        | "relative_humidity"
        | "cloud_cover"
        | "cloud_cover_low"
        | "cloud_cover_mid"
        | "cloud_cover_high" => "%",
        "pressure_msl" | "surface_pressure" => "hPa",
        "wind_speed_10m"
        | "wind_gusts_10m"
        | "10m_wind_gusts"
        | "wind_u_10m_ms"
        | "wind_v_10m_ms"
        | "u_component_of_wind_10m"
        | "v_component_of_wind_10m"
        | "wind_speed"
        | "u_component_of_wind"
        | "v_component_of_wind" => "m/s",
        "wind_direction_10m" => "deg",
        "precipitation" | "rain" | "precipitable_water" => "mm",
        "snowfall" => "cm",
        "snow_depth" => "m",
        "shortwave_radiation" | "direct_radiation" | "diffuse_radiation" => "W/m^2",
        "cape" | "convective_inhibition" => "J/kg",
        "visibility" => "m",
        "vpd_2m" => "hPa",
        _ => "unknown",
    }
}

fn raw_variable_for_product(product: &str) -> Option<&'static str> {
    match product {
        "2m_temperature" => Some("temperature_2m"),
        "2m_dewpoint" => Some("dew_point_2m"),
        "2m_relative_humidity" | "2m_rh" => Some("relative_humidity_2m"),
        "10m_wind_gusts" => Some("wind_gusts_10m"),
        "total_qpf" => Some("precipitation"),
        "mslp" | "mean_sea_level_pressure" | "mslp_10m_winds" => Some("pressure_msl"),
        "cloud_cover" => Some("cloud_cover"),
        "low_cloud_cover" => Some("cloud_cover_low"),
        "middle_cloud_cover" => Some("cloud_cover_mid"),
        "high_cloud_cover" => Some("cloud_cover_high"),
        "precipitable_water" => Some("precipitable_water"),
        "visibility" => Some("visibility"),
        "sbcape" | "cape" => Some("cape"),
        "sbcin" | "convective_inhibition" => Some("convective_inhibition"),
        "composite_reflectivity" => Some("composite_reflectivity"),
        "shortwave_radiation" => Some("shortwave_radiation"),
        _ => None,
    }
}

fn parse_pressure_grid_product(
    product: &str,
    available_levels: &[u16],
) -> Option<PressureGridSpec> {
    let lower = product.to_ascii_lowercase();
    let (level_text, rest) = lower.split_once("mb_")?;
    let level_hpa = level_text.parse::<u16>().ok()?;
    if !available_levels.contains(&level_hpa) {
        return None;
    }
    let kind = if rest == "temperature" || rest == "temperature_height_winds" {
        PressureGridKind::Variable("TMP")
    } else if rest == "height" || rest == "height_winds" {
        PressureGridKind::Variable("HGT")
    } else if rest == "specific_humidity" || rest == "q" {
        PressureGridKind::Variable("SPFH")
    } else if rest == "u_wind" || rest == "ugrd" {
        PressureGridKind::Variable("UGRD")
    } else if rest == "v_wind" || rest == "vgrd" {
        PressureGridKind::Variable("VGRD")
    } else if rest == "wind_speed" || rest == "winds" {
        PressureGridKind::WindSpeed
    } else if rest == "dewpoint" || rest == "dewpoint_height_winds" {
        PressureGridKind::Dewpoint
    } else if rest == "rh" || rest == "relative_humidity" || rest == "rh_height_winds" {
        PressureGridKind::RelativeHumidity
    } else if rest == "absolute_vorticity" || rest == "absolute_vorticity_height_winds" {
        PressureGridKind::Variable("ABSV")
    } else {
        return None;
    };
    let units = match kind {
        PressureGridKind::Variable("TMP") | PressureGridKind::Dewpoint => "degC",
        PressureGridKind::Variable("HGT") => "m",
        PressureGridKind::Variable("SPFH") => "kg/kg",
        PressureGridKind::Variable("UGRD")
        | PressureGridKind::Variable("VGRD")
        | PressureGridKind::WindSpeed => "m/s",
        PressureGridKind::Variable("ABSV") => "s^-1",
        PressureGridKind::RelativeHumidity => "%",
        PressureGridKind::Variable(_) => "unknown",
    };
    Some(PressureGridSpec {
        output_name: product.to_string(),
        units,
        level_hpa,
        kind,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowReducer {
    Min,
    Max,
    Range,
}

#[derive(Debug, Clone, Copy)]
struct WindowedProduct {
    raw_variable: &'static str,
    start: u32,
    end: u32,
    reducer: WindowReducer,
}

fn parse_windowed_product(product: &str) -> Option<WindowedProduct> {
    let (raw_variable, prefix) = if let Some(rest) = product.strip_prefix("2m_temp_") {
        ("temperature_2m", rest)
    } else if let Some(rest) = product.strip_prefix("2m_dewpoint_") {
        ("dew_point_2m", rest)
    } else if let Some(rest) = product.strip_prefix("2m_rh_") {
        ("relative_humidity_2m", rest)
    } else if let Some(rest) = product.strip_prefix("10m_wind_") {
        ("wind_speed_10m", rest)
    } else {
        return None;
    };

    let (window, reducer) = if let Some(window) = prefix.strip_suffix("_max") {
        (window, WindowReducer::Max)
    } else if let Some(window) = prefix.strip_suffix("_min") {
        (window, WindowReducer::Min)
    } else if let Some(window) = prefix.strip_suffix("_range") {
        (window, WindowReducer::Range)
    } else {
        return None;
    };

    let (start, end) = match window {
        "1h" => (0, 1),
        "0_24h" => (0, 24),
        "0_48h" => (0, 48),
        "24_48h" => (24, 48),
        "run" => (0, 255),
        _ => return None,
    };
    Some(WindowedProduct {
        raw_variable,
        start,
        end,
        reducer,
    })
}

fn finite2(a: f32, b: f32) -> Option<(f32, f32)> {
    (a.is_finite() && b.is_finite()).then_some((a, b))
}

fn finite3(a: f32, b: f32, c: f32) -> Option<(f32, f32, f32)> {
    (a.is_finite() && b.is_finite() && c.is_finite()).then_some((a, b, c))
}

fn vapor_pressure_deficit_kpa(temp_c: f32, rh_pct: f32) -> f32 {
    let es = 0.6108 * ((17.27 * temp_c) / (temp_c + 237.3)).exp();
    es * (1.0 - (rh_pct / 100.0).clamp(0.0, 1.5))
}

fn relative_humidity_from_temp_dewpoint_c(temp_c: f32, dewpoint_c: f32) -> f32 {
    let es_td = ((17.625 * dewpoint_c) / (243.04 + dewpoint_c)).exp();
    let es_t = ((17.625 * temp_c) / (243.04 + temp_c)).exp();
    (100.0 * es_td / es_t).clamp(0.0, 150.0)
}

fn heat_index_c(temp_c: f32, rh_pct: f32) -> f32 {
    let temp_f = temp_c * 9.0 / 5.0 + 32.0;
    if temp_f < 80.0 {
        return temp_c;
    }
    let r = rh_pct;
    let hi_f = -42.379 + 2.049_015_3 * temp_f + 10.143_331 * r
        - 0.224_755_4 * temp_f * r
        - 0.006_837_83 * temp_f * temp_f
        - 0.054_817_17 * r * r
        + 0.001_228_74 * temp_f * temp_f * r
        + 0.000_852_82 * temp_f * r * r
        - 0.000_001_99 * temp_f * temp_f * r * r;
    (hi_f - 32.0) * 5.0 / 9.0
}

fn wind_chill_c(temp_c: f32, wind_ms: f32) -> f32 {
    let wind_kmh = wind_ms * 3.6;
    if temp_c > 10.0 || wind_kmh < 4.8 {
        return temp_c;
    }
    13.12 + 0.6215 * temp_c - 11.37 * wind_kmh.powf(0.16) + 0.3965 * temp_c * wind_kmh.powf(0.16)
}

fn apparent_temperature_c(temp_c: f32, rh_pct: f32, wind_ms: f32) -> f32 {
    let es_hpa = 6.105 * ((17.27 * temp_c) / (237.7 + temp_c)).exp() * (rh_pct / 100.0);
    temp_c + 0.33 * es_hpa - 0.70 * wind_ms - 4.0
}

fn wind_direction_deg(u_ms: f32, v_ms: f32) -> f32 {
    let direction = 270.0 - v_ms.atan2(u_ms).to_degrees();
    direction.rem_euclid(360.0)
}

fn normalize_spatial_values(variable: &str, values: &mut [f32]) {
    if matches!(
        variable,
        "temperature_2m" | "dew_point_2m" | "apparent_temperature" | "temperature" | "dew_point"
    ) && values
        .iter()
        .any(|value| value.is_finite() && *value > 150.0)
    {
        for value in values.iter_mut().filter(|value| value.is_finite()) {
            *value -= 273.15;
        }
    }
    if matches!(variable, "pressure_msl" | "surface_pressure")
        && values
            .iter()
            .any(|value| value.is_finite() && *value > 10_000.0)
    {
        for value in values.iter_mut().filter(|value| value.is_finite()) {
            *value /= 100.0;
        }
    }
}

fn valid_times_from_run_id(run: &str, hours: &[u32]) -> Vec<String> {
    if let Some((date, hour_z)) = run.split_once('_') {
        if date.len() == 8 && hour_z.ends_with('z') {
            if let Ok(hour) = hour_z.trim_end_matches('z').parse::<u32>() {
                return hours
                    .iter()
                    .map(|lead| {
                        format!(
                            "{}-{}-{}T{:02}:00:00Z",
                            &date[0..4],
                            &date[4..6],
                            &date[6..8],
                            (hour + lead) % 24
                        )
                    })
                    .collect();
            }
        }
    }
    hours
        .iter()
        .map(|hour| format!("{run}+f{hour:03}"))
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiagnosticManifest {
    format: String,
    schema_version: u32,
    recipe_version: String,
    hours: Vec<u8>,
    diagnostics: Vec<DiagnosticDescriptor>,
    point_count: usize,
    west_lon_deg: f64,
    east_lon_deg: f64,
    south_lat_deg: f64,
    north_lat_deg: f64,
    payload_path: String,
    payload_bytes: u64,
    build_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiagnosticDescriptor {
    key: String,
    label: String,
    units: String,
    pointer: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiagnosticPayload {
    manifest: DiagnosticManifest,
    points: Vec<DiagnosticPoint>,
    values: BTreeMap<String, Vec<Vec<Option<f32>>>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct DiagnosticPoint {
    lat: f64,
    lon: f64,
}

struct DiagnosticLane {
    payload: DiagnosticPayload,
}

struct DiagnosticSample {
    units: BTreeMap<String, String>,
    values: BTreeMap<String, Vec<Option<f64>>>,
    matched_index: usize,
}

impl DiagnosticLane {
    fn open(root: &Path) -> Result<Self> {
        let manifest: DiagnosticManifest =
            serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
        let payload_bytes = fs::read(root.join(&manifest.payload_path))?;
        let payload: DiagnosticPayload = bincode::deserialize(&payload_bytes)?;
        Ok(Self { payload })
    }

    fn lane_manifest_json(&self) -> Value {
        let manifest = &self.payload.manifest;
        json!({
            "schema": "wxstore.lane.v1",
            "id": "diag_scalar_basic",
            "status": "ready_sparse_v0",
            "role": "canonical_derived_sparse_until_dense_lane_exists",
            "format": manifest.format,
            "recipe_version": manifest.recipe_version,
            "point_count": manifest.point_count,
            "lead_time_seconds": manifest.hours.iter().map(|hour| u32::from(*hour) * 3600).collect::<Vec<_>>(),
            "diagnostics": manifest.diagnostics.iter().map(|descriptor| json!({
                "diagnostic_id": descriptor.key,
                "label": descriptor.label,
                "units": descriptor.units,
                "formula_version": manifest.recipe_version,
                "pointer": descriptor.pointer,
                "limitations": [
                    "sparse diagnostic lane; nearest diagnostic point is used",
                    "dense CONUS diagnostic lane is the production target"
                ]
            })).collect::<Vec<_>>()
        })
    }

    fn sample_point(
        &self,
        lat: f64,
        lon: f64,
        hours: &[u8],
        mode: DiagnosticMode,
    ) -> DiagnosticSample {
        let (index, _) = self.nearest_point(lat, lon);
        let hour_indices = hours
            .iter()
            .filter_map(|hour| {
                self.payload
                    .manifest
                    .hours
                    .iter()
                    .position(|stored| stored == hour)
            })
            .collect::<Vec<_>>();
        let allowed = mode.keys();
        let mut units = BTreeMap::new();
        let mut values = BTreeMap::new();
        for descriptor in &self.payload.manifest.diagnostics {
            if allowed.is_some_and(|keys| !keys.contains(&descriptor.key.as_str())) {
                continue;
            }
            units.insert(descriptor.key.clone(), descriptor.units.clone());
            let row = self
                .payload
                .values
                .get(&descriptor.key)
                .and_then(|rows| rows.get(index))
                .map(|stored| {
                    hour_indices
                        .iter()
                        .map(|&idx| stored.get(idx).and_then(|value| value.map(f64::from)))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            values.insert(descriptor.key.clone(), row);
        }
        DiagnosticSample {
            units,
            values,
            matched_index: index,
        }
    }

    fn nearest_point(&self, lat: f64, lon: f64) -> (usize, f64) {
        let cos_lat = lat.to_radians().cos().abs().max(0.2);
        let mut best_index = 0usize;
        let mut best_score = f64::INFINITY;
        for (idx, point) in self.payload.points.iter().enumerate() {
            let dlat = point.lat - lat;
            let dlon = normalized_lon_delta(point.lon - lon) * cos_lat;
            let score = dlat * dlat + dlon * dlon;
            if score < best_score {
                best_score = score;
                best_index = idx;
            }
        }
        (best_index, best_score.sqrt())
    }
}

fn diagnostic_json(
    diagnostic: Option<&DiagnosticLane>,
    point: GridPoint,
    hours: &[u8],
    mode: DiagnosticMode,
) -> Result<Value> {
    let Some(diagnostic) = diagnostic else {
        bail!("precomputed diagnostic lane is not configured");
    };
    let sample = diagnostic.sample_point(point.lat, point.lon, hours, mode);
    Ok(json!({
        "lane": "diag_scalar_basic",
        "status": "ready_sparse_v0",
        "dims": ["time"],
        "axes": {"forecast_hour": hours},
        "units": sample.units,
        "values": sample.values,
        "matched_diagnostic_point": {
            "index": sample.matched_index,
            "note": "sparse diagnostic lane; nearest diagnostic point is used"
        },
        "formula_set": diagnostic.payload.manifest.recipe_version,
        "coverage": {
            "status": "complete",
            "available_hour_ranges": hour_ranges(hours)
        }
    }))
}

#[derive(Debug, Clone, Copy)]
struct HrrrLambert {
    nx: usize,
    ny: usize,
    lat1: f64,
    lon1: f64,
    dx: f64,
    dy: f64,
    latin1: f64,
    latin2: f64,
    lov: f64,
    earth_radius_m: f64,
}

impl Default for HrrrLambert {
    fn default() -> Self {
        Self {
            nx: 1799,
            ny: 1059,
            lat1: 21.138123,
            lon1: 237.280472,
            dx: 3000.0,
            dy: 3000.0,
            latin1: 38.5,
            latin2: 38.5,
            lov: 262.5,
            earth_radius_m: 6371229.0,
        }
    }
}

impl HrrrLambert {
    fn nearest(&self, lat: f64, lon: f64) -> (usize, usize) {
        let (x, y) = self.project_relative(lat, lon);
        let i = (x / self.dx).round().clamp(0.0, (self.nx - 1) as f64) as usize;
        let j = (y / self.dy).round().clamp(0.0, (self.ny - 1) as f64) as usize;
        (i, j)
    }

    fn latlon_at(&self, x: usize, y: usize) -> (f64, f64) {
        let n = self.n();
        let f = self.f(n);
        let lon1 = Self::normalize_lon_east(self.lon1);
        let lov = Self::normalize_lon_east(self.lov);
        let theta1 = n * (lon1.to_radians() - lov.to_radians());
        let rho1 = self.rho(self.lat1, n, f);
        let xr = x as f64 * self.dx;
        let yr = y as f64 * self.dy;
        let x_abs = xr + rho1 * theta1.sin();
        let y_abs = rho1 * theta1.cos() - yr;
        let rho = (x_abs * x_abs + y_abs * y_abs).sqrt();
        let theta = x_abs.atan2(y_abs);
        let lat = 2.0 * (self.earth_radius_m * f / rho).powf(1.0 / n).atan()
            - std::f64::consts::FRAC_PI_2;
        let mut lon = lov.to_radians() + theta / n;
        lon = lon.to_degrees();
        while lon > 180.0 {
            lon -= 360.0;
        }
        while lon <= -180.0 {
            lon += 360.0;
        }
        (lat.to_degrees(), lon)
    }

    fn project_relative(&self, lat: f64, lon: f64) -> (f64, f64) {
        let n = self.n();
        let f = self.f(n);
        let lon = Self::normalize_lon_east(lon);
        let lon1 = Self::normalize_lon_east(self.lon1);
        let lov = Self::normalize_lon_east(self.lov);
        let theta = n * (lon.to_radians() - lov.to_radians());
        let theta1 = n * (lon1.to_radians() - lov.to_radians());
        let rho = self.rho(lat, n, f);
        let rho1 = self.rho(self.lat1, n, f);
        (
            rho * theta.sin() - rho1 * theta1.sin(),
            rho1 * theta1.cos() - rho * theta.cos(),
        )
    }

    fn normalize_lon_east(lon: f64) -> f64 {
        if lon < 0.0 {
            lon + 360.0
        } else {
            lon
        }
    }

    fn n(&self) -> f64 {
        let phi1 = self.latin1.to_radians();
        let phi2 = self.latin2.to_radians();
        if (self.latin1 - self.latin2).abs() < 1e-9 {
            phi1.sin()
        } else {
            (phi1.cos() / phi2.cos()).ln()
                / (((std::f64::consts::FRAC_PI_4 + phi2 / 2.0).tan())
                    / ((std::f64::consts::FRAC_PI_4 + phi1 / 2.0).tan()))
                .ln()
        }
    }

    fn f(&self, n: f64) -> f64 {
        let phi1 = self.latin1.to_radians();
        (phi1.cos() * (std::f64::consts::FRAC_PI_4 + phi1 / 2.0).tan().powf(n)) / n
    }

    fn rho(&self, lat: f64, n: f64, f: f64) -> f64 {
        let phi = lat.to_radians();
        self.earth_radius_m * f / (std::f64::consts::FRAC_PI_4 + phi / 2.0).tan().powf(n)
    }
}

fn validate_canonical(
    profile: &ProfileLane,
    model: &str,
    domain: &str,
    run: &str,
) -> Result<(), ApiError> {
    let manifest = &profile.manifest;
    if model != manifest.model || domain != manifest.domain || run != manifest.run_id {
        return Err(not_found("requested run is not loaded on this node"));
    }
    Ok(())
}

fn cache_key(run: &str, point: &GridPoint, req: &RequestShape, format: &str) -> String {
    format!(
        "run={run}|x={}|y={}|hours={}|vars={}|diag={}|format={format}",
        point.x,
        point.y,
        hours_key(&req.hours),
        req.profile_variables.join(","),
        req.diagnostic_mode.as_str(),
    )
}

fn bytes_response(
    bytes: Bytes,
    content_type: &'static str,
    cache_hit: bool,
    immutable: bool,
) -> Response {
    let mut response = bytes.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if immutable {
            "public, max-age=31536000, immutable"
        } else {
            "public, max-age=60"
        }),
    );
    headers.insert(
        "x-wxstore-cache",
        HeaderValue::from_static(if cache_hit { "hit" } else { "miss" }),
    );
    response
}

fn query_lat(query: &PointQuery) -> Result<f64, ApiError> {
    query
        .lat
        .or(query.latitude)
        .ok_or_else(|| bad_request("lat or latitude is required"))
}

fn query_lon(query: &PointQuery) -> Result<f64, ApiError> {
    query
        .lon
        .or(query.longitude)
        .ok_or_else(|| bad_request("lon or longitude is required"))
}

fn parse_hours(value: &str) -> Result<Vec<u8>> {
    let mut hours = Vec::new();
    for part in value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((start, end)) = part.split_once('-').or_else(|| part.split_once(':')) {
            let start = start
                .parse::<u8>()
                .with_context(|| format!("invalid hour range '{part}'"))?;
            let end = end
                .parse::<u8>()
                .with_context(|| format!("invalid hour range '{part}'"))?;
            if end < start {
                bail!("hour range '{part}' is reversed");
            }
            hours.extend(start..=end);
        } else {
            hours.push(
                part.parse::<u8>()
                    .with_context(|| format!("invalid hour '{part}'"))?,
            );
        }
    }
    hours.sort_unstable();
    hours.dedup();
    Ok(hours)
}

fn parse_hours_u32(value: &str) -> Result<Vec<u32>> {
    let mut hours = Vec::new();
    for part in value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        if let Some((start, end)) = part.split_once('-').or_else(|| part.split_once(':')) {
            let start = start
                .parse::<u32>()
                .with_context(|| format!("invalid hour range '{part}'"))?;
            let end = end
                .parse::<u32>()
                .with_context(|| format!("invalid hour range '{part}'"))?;
            if end < start {
                bail!("hour range '{part}' is reversed");
            }
            hours.extend(start..=end);
        } else {
            hours.push(
                part.parse::<u32>()
                    .with_context(|| format!("invalid hour '{part}'"))?,
            );
        }
    }
    hours.sort_unstable();
    hours.dedup();
    Ok(hours)
}

fn split_csv(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(str::to_string)
        .collect()
}

fn valid_times_from_cycle(cycle: &str, forecast_hours: &[u8]) -> Vec<String> {
    if let Ok(cycle_time) = chrono::DateTime::parse_from_rfc3339(cycle) {
        forecast_hours
            .iter()
            .map(|hour| {
                (cycle_time + chrono::Duration::hours(i64::from(*hour)))
                    .with_timezone(&chrono::Utc)
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            })
            .collect()
    } else {
        forecast_hours
            .iter()
            .map(|hour| format!("{cycle}+f{hour:03}"))
            .collect()
    }
}

fn profile_output(variable: &str) -> (&'static str, &'static str) {
    match variable {
        "TMP" => ("temperature_c", "degC"),
        "SPFH" => ("specific_humidity_kg_kg", "kg/kg"),
        "UGRD" => ("u_wind_ms", "m/s"),
        "VGRD" => ("v_wind_ms", "m/s"),
        "HGT" => ("height_m_msl", "m"),
        "VVEL" => ("vertical_velocity_pa_s", "Pa/s"),
        "ABSV" => ("absolute_vorticity_s_1", "s^-1"),
        "CLWMR" => ("cloud_water_mixing_ratio_kg_kg", "kg/kg"),
        "ICMR" | "CIMIXR" => ("cloud_ice_mixing_ratio_kg_kg", "kg/kg"),
        "RWMR" => ("rain_mixing_ratio_kg_kg", "kg/kg"),
        "SNMR" => ("snow_mixing_ratio_kg_kg", "kg/kg"),
        "GRLE" => ("graupel_mixing_ratio_kg_kg", "kg/kg"),
        _ => ("raw_profile_field", "unknown"),
    }
}

fn specific_humidity_to_dewpoint_c(q_kg_kg: f64, pressure_hpa: f64) -> Option<f64> {
    if !q_kg_kg.is_finite() || !pressure_hpa.is_finite() || q_kg_kg <= 0.0 || pressure_hpa <= 0.0 {
        return None;
    }
    let e_hpa = q_kg_kg * pressure_hpa / (0.622 + 0.378 * q_kg_kg);
    if !e_hpa.is_finite() || e_hpa <= 0.0 {
        return None;
    }
    let ln = (e_hpa / 6.112).ln();
    let denominator = 17.67 - ln;
    if denominator.abs() < 1.0e-6 {
        return None;
    }
    Some(243.5 * ln / denominator)
}

fn hour_ranges(hours: &[u8]) -> Vec<[u8; 2]> {
    let mut ranges = Vec::new();
    let mut iter = hours.iter().copied();
    let Some(mut start) = iter.next() else {
        return ranges;
    };
    let mut prev = start;
    for hour in iter {
        if hour == prev.saturating_add(1) {
            prev = hour;
        } else {
            ranges.push([start, prev]);
            start = hour;
            prev = hour;
        }
    }
    ranges.push([start, prev]);
    ranges
}

fn hours_key(hours: &[u8]) -> String {
    if let (Some(first), Some(last)) = (hours.first(), hours.last()) {
        if hours.windows(2).all(|pair| pair[1] == pair[0] + 1) {
            return format!("{first}-{last}");
        }
    }
    hours
        .iter()
        .map(u8::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn normalized_lon_delta(mut lon: f64) -> f64 {
    while lon > 180.0 {
        lon -= 360.0;
    }
    while lon <= -180.0 {
        lon += 360.0;
    }
    lon
}

fn normalize_lon(lon: f64) -> f64 {
    normalized_lon_delta(lon)
}

fn u32_from(bytes: &[u8]) -> Result<u32> {
    Ok(u32::from_le_bytes(bytes.try_into()?))
}

fn u64_from(bytes: &[u8]) -> Result<u64> {
    Ok(u64::from_le_bytes(bytes.try_into()?))
}

fn f32_from(bytes: &[u8]) -> Result<f32> {
    Ok(f32::from_le_bytes(bytes.try_into()?))
}

fn insert_header(headers: &mut HeaderMap, name: &'static str, value: &str) {
    if let Ok(value) = HeaderValue::from_str(value) {
        headers.insert(name, value);
    }
}

fn bad_request(reason: impl AsRef<str>) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": true, "reason": reason.as_ref()})),
    )
}

fn bad_anyhow(err: anyhow::Error) -> ApiError {
    bad_request(err.to_string())
}

fn not_found(reason: impl AsRef<str>) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(json!({"error": true, "reason": reason.as_ref()})),
    )
}

fn internal_error(reason: impl AsRef<str>) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": true, "reason": reason.as_ref()})),
    )
}
