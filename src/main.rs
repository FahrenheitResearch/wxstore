#![recursion_limit = "256"]

use std::{
    cmp::Ordering,
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{Arc, RwLock},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, bail, Context, Result};
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{header, HeaderMap, HeaderValue, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use memmap2::{Mmap, MmapOptions};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tower_http::cors::{Any, CorsLayer};

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
const CACHE_BYTES_LIMIT: usize = 512 * 1024 * 1024;
const MAX_QUERY_HOURS: usize = 400;
const MAX_FORECAST_VARIABLES: usize = 96;
const MAX_GRID_JSON_CELLS: usize = 4_000_000;
const MAX_TILE_ZOOM: u32 = 14;
const STATIC_PLOT_MANIFEST_CACHE_TTL_DEFAULT_SECS: u64 = 5;
const STATIC_PLOT_DEFAULT_MANIFEST_LIMIT: usize = 500;
const STATIC_PLOT_MAX_MANIFEST_LIMIT: usize = 20_000;
const STATIC_PLOT_EXPORT_MAX_FRAMES: usize = 1000;
const MESO_INNOVATION_DEFAULT_TOP: usize = 50;
const MESO_INNOVATION_MAX_TOP: usize = 500;
const RADAR_POLAR_SIDECAR_SCHEMA: &str = "rustwx.radar.polar_sidecar.v2";
const RADAR_POLAR_SIDECAR_MANIFEST_FILE: &str = "polar_sidecar_manifest.json";
const RADAR_POLAR_VALUES_FILE: &str = "polar_values_f32le.bin";
const RADAR_POLAR_GATE_FLAGS_FILE: &str = "polar_gate_flags_u8.bin";
const RADAR_SIDECAR_CACHE_MAX_ENTRIES: usize = 24;
const RADAR_EARTH_AUTHALIC_RADIUS_M: f64 = 6_371_008.8;
const RADAR_GATE_FLAG_VALID: u8 = 0b0000_0001;
const RADAR_GATE_FLAG_MISSING: u8 = 0b0000_0010;
const RADAR_GATE_FLAG_RANGE_FOLDED: u8 = 0b0000_0100;
const RADAR_GATE_FLAG_FILTERED: u8 = 0b0000_1000;
const RADAR_GATE_FLAG_DERIVED: u8 = 0b0001_0000;
const RADAR_GATE_FLAG_DEALIASED: u8 = 0b0010_0000;
const RADAR_REQUIRED_GATE_FLAG_MEANINGS: &[(&str, u8)] = &[
    ("valid", RADAR_GATE_FLAG_VALID),
    ("missing", RADAR_GATE_FLAG_MISSING),
    ("range_folded", RADAR_GATE_FLAG_RANGE_FOLDED),
    ("filtered", RADAR_GATE_FLAG_FILTERED),
    ("derived", RADAR_GATE_FLAG_DERIVED),
    ("dealiased", RADAR_GATE_FLAG_DEALIASED),
];

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
    InspectSpatial(InspectSpatialArgs),
    MaterializeSpatial(MaterializeSpatialArgs),
    ImportRustwxGrids(ImportRustwxGridsArgs),
    PublishLatest(PublishLatestArgs),
    GcSpatial(GcSpatialArgs),
}

#[derive(Parser, Clone)]
struct ServeArgs {
    #[arg(long)]
    profile_store: Option<PathBuf>,
    #[arg(long)]
    diagnostic_store: Option<PathBuf>,
    #[arg(long)]
    spatial_root: Option<PathBuf>,
    #[arg(long)]
    static_plots_root: Option<PathBuf>,
    #[arg(long)]
    evidence_root: Option<PathBuf>,
    #[arg(long)]
    observations_root: Option<PathBuf>,
    #[arg(long)]
    mesoanalysis_innovation_index_root: Option<PathBuf>,
    #[arg(long)]
    satellite_tiles_root: Option<PathBuf>,
    #[arg(long)]
    radar_tiles_root: Option<PathBuf>,
    #[arg(long)]
    archive_root: Option<PathBuf>,
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, default_value_t = 8897)]
    port: u16,
}

fn infer_ops_root(args: &ServeArgs) -> PathBuf {
    args.static_plots_root
        .as_ref()
        .and_then(|path| path.parent())
        .or_else(|| args.spatial_root.as_ref().and_then(|path| path.parent()))
        .or_else(|| {
            args.satellite_tiles_root
                .as_ref()
                .and_then(|path| path.parent())
        })
        .or_else(|| {
            args.radar_tiles_root
                .as_ref()
                .and_then(|path| path.parent())
        })
        .or_else(|| {
            args.observations_root
                .as_ref()
                .and_then(|path| path.parent())
        })
        .or_else(|| {
            args.mesoanalysis_innovation_index_root
                .as_ref()
                .and_then(|path| path.parent())
        })
        .or_else(|| args.evidence_root.as_ref().and_then(|path| path.parent()))
        .or_else(|| args.profile_store.as_ref().and_then(|path| path.parent()))
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[derive(Parser, Clone)]
struct InspectArgs {
    #[arg(long)]
    profile_store: Option<PathBuf>,
    #[arg(long)]
    diagnostic_store: Option<PathBuf>,
    #[arg(long)]
    spatial_root: Option<PathBuf>,
    #[arg(long)]
    static_plots_root: Option<PathBuf>,
    #[arg(long)]
    evidence_root: Option<PathBuf>,
    #[arg(long)]
    observations_root: Option<PathBuf>,
    #[arg(long)]
    mesoanalysis_innovation_index_root: Option<PathBuf>,
}

#[derive(Parser, Clone)]
struct InspectSpatialArgs {
    #[arg(long)]
    spatial_root: PathBuf,
    #[arg(long)]
    model: Option<String>,
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
    #[arg(long = "manifest", required = true)]
    manifests: Vec<PathBuf>,
    #[arg(long)]
    spatial_root: PathBuf,
    #[arg(long)]
    model: Option<String>,
    #[arg(long)]
    run: Option<String>,
    #[arg(long)]
    member: Option<String>,
    #[arg(long)]
    publish_latest: bool,
}

#[derive(Parser, Clone)]
struct PublishLatestArgs {
    #[arg(long)]
    spatial_root: PathBuf,
    #[arg(long)]
    model: String,
    #[arg(long)]
    run: String,
}

#[derive(Parser, Clone)]
struct GcSpatialArgs {
    #[arg(long)]
    spatial_root: PathBuf,
    #[arg(long)]
    model: String,
    #[arg(long, default_value_t = 2)]
    keep_runs: usize,
    #[arg(long)]
    apply: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().init();
    match Cli::parse().command {
        Command::Serve(args) => serve(args).await,
        Command::Inspect(args) => {
            let profile = args
                .profile_store
                .as_ref()
                .map(|path| ProfileLane::open(path))
                .transpose()?
                .map(Arc::new);
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
            let static_plots = args
                .static_plots_root
                .as_ref()
                .map(|path| StaticPlotLane::open(path))
                .transpose()?;
            let evidence = args
                .evidence_root
                .as_ref()
                .map(|path| EvidenceBundleLane::open(path))
                .transpose()?;
            let observations = args
                .observations_root
                .as_ref()
                .map(|path| ObservationLane::open(path))
                .transpose()?;
            let mesoanalysis_innovation = args
                .mesoanalysis_innovation_index_root
                .as_ref()
                .map(|path| MesoanalysisInnovationLane::open(path))
                .transpose()?;
            println!(
                "{}",
                serde_json::to_string_pretty(&store_status(
                    profile.as_deref(),
                    diagnostic.as_ref(),
                    spatial.as_ref(),
                    static_plots.as_ref(),
                    evidence.as_ref(),
                    observations.as_ref(),
                    mesoanalysis_innovation.as_ref(),
                    None,
                    None,
                    None,
                    None,
                ))?
            );
            Ok(())
        }
        Command::InspectSpatial(args) => inspect_spatial(args),
        Command::MaterializeSpatial(args) => materialize_spatial(args),
        Command::ImportRustwxGrids(args) => import_rustwx_grids(args),
        Command::PublishLatest(args) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&publish_latest_pointer(
                    &args.spatial_root,
                    &args.model,
                    &args.run,
                    "manual"
                )?)?
            );
            Ok(())
        }
        Command::GcSpatial(args) => gc_spatial(args),
    }
}

async fn serve(args: ServeArgs) -> Result<()> {
    let ops_root = infer_ops_root(&args);
    let archive_root = args
        .archive_root
        .clone()
        .unwrap_or_else(|| ops_root.join("archive"));
    let archive = if archive_root.is_dir() {
        Some(Arc::new(ArchiveLane::open(&archive_root)?))
    } else {
        None
    };
    let state = Arc::new(AppState {
        profile: args
            .profile_store
            .as_ref()
            .map(|path| ProfileLane::open(path))
            .transpose()
            .map(|lane| lane.map(Arc::new))?,
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
        static_plots: args
            .static_plots_root
            .as_ref()
            .map(|path| StaticPlotLane::open(path))
            .transpose()
            .map(|lane| lane.map(Arc::new))?,
        evidence: args
            .evidence_root
            .as_ref()
            .map(|path| EvidenceBundleLane::open(path))
            .transpose()
            .map(|lane| lane.map(Arc::new))?,
        observations: args
            .observations_root
            .as_ref()
            .map(|path| ObservationLane::open(path))
            .transpose()
            .map(|lane| lane.map(Arc::new))?,
        mesoanalysis_innovation: args
            .mesoanalysis_innovation_index_root
            .as_ref()
            .map(|path| MesoanalysisInnovationLane::open(path))
            .transpose()
            .map(|lane| lane.map(Arc::new))?,
        satellite_tiles: args
            .satellite_tiles_root
            .as_ref()
            .map(|path| SatelliteTileLane::open(path))
            .transpose()
            .map(|lane| lane.map(Arc::new))?,
        radar_tiles: args
            .radar_tiles_root
            .as_ref()
            .map(|path| RadarTileLane::open(path))
            .transpose()
            .map(|lane| lane.map(Arc::new))?,
        plot_lab: Arc::new(PlotLabLane::from_env(&ops_root)?),
        cross_sections: Arc::new(CrossSectionLane::from_env(&ops_root)?),
        soundings: Arc::new(SoundingLane::from_env(&ops_root)?),
        archive,
        ops_root,
        cache: RwLock::new(ResponseCache::default()),
    });

    let app = Router::new()
        .route("/", get(index))
        .route("/plots", get(plots))
        .route("/satellite", get(satellite_viewer))
        .route("/radar", get(radar_viewer))
        .route("/tools", get(weather_tools))
        .route("/meteograms", get(weather_tools))
        .route("/cross-sections", get(weather_tools))
        .route("/plot-lab", get(plot_lab))
        .route("/projection-demo", get(projection_demo))
        .route("/hrrrarchive", get(archive_viewer))
        .route("/archive", get(archive_viewer))
        .route("/ops", get(ops))
        .route("/livez", get(livez))
        .route("/readyz", get(readyz))
        .route("/v1/status", get(status))
        .route("/api/status", get(status))
        .route("/v1/models", get(models))
        .route("/v1/objects", get(weather_objects))
        .route("/v1/variables", get(variables))
        .route("/v1/products", get(products))
        .route("/v1/static-plots", get(static_plots))
        .route("/v1/static-plots/export-mp4", post(static_plots_export_mp4))
        .route("/v1/evidence/bundles", get(evidence_bundles))
        .route("/v1/evidence/bundles/{bundle_id}", get(evidence_bundle))
        .route("/v1/observations/sources", get(observation_sources))
        .route(
            "/v1/observations/sources/{source_id}",
            get(observation_source),
        )
        .route(
            "/v1/mesoanalysis/innovation/status",
            get(mesoanalysis_innovation_status),
        )
        .route(
            "/v1/mesoanalysis/innovation/query",
            get(mesoanalysis_innovation_query),
        )
        .route(
            "/v1/mesoanalysis/innovation/watchlist",
            get(mesoanalysis_innovation_watchlist),
        )
        .route("/v1/satellite/layers", get(satellite_layers))
        .route(
            "/v1/satellite/layers/{layer_id}/frames.json",
            get(satellite_frames),
        )
        .route(
            "/v1/satellite/tiles/{layer_id}/frames/{frame_id}/{z}/{x}/{tile_file}",
            get(satellite_tile),
        )
        .route("/v1/radar/layers", get(radar_layers))
        .route("/v1/radar/layers/{layer_id}/frames.json", get(radar_frames))
        .route(
            "/v1/radar/sidecars/{layer_id}/frames/{frame_id}/{sidecar_file}",
            get(radar_sidecar),
        )
        .route(
            "/v1/radar/sidecars/{layer_id}/frames/{frame_id}/{tilt_id}/{sidecar_file}",
            get(radar_tilt_sidecar),
        )
        .route("/v1/radar/sample", get(radar_sample))
        .route(
            "/v1/radar/tiles/{layer_id}/frames/{frame_id}/{z}/{x}/{tile_file}",
            get(radar_tile),
        )
        .route(
            "/v1/radar/tiles/{layer_id}/frames/{frame_id}/{tilt_id}/{z}/{x}/{tile_file}",
            get(radar_tilt_tile),
        )
        .route("/v1/plot-lab/config", get(plot_lab_config))
        .route("/v1/plot-lab/render", post(plot_lab_render))
        .route(
            "/v1/plot-lab/artifacts/{render_id}/{file_name}",
            get(plot_lab_artifact),
        )
        .route("/v1/ops/live", get(ops_live))
        .route(
            "/v1/static-plots/artifacts/{manifest_id}/{artifact_index}",
            get(static_plot_artifact),
        )
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
        .route("/v1/cross-section/status", get(cross_section_status))
        .route(
            "/v1/cross-section/status/products",
            get(cross_section_status_products),
        )
        .route("/v1/cross-section/products", get(cross_section_products))
        .route("/v1/cross-section/render", post(cross_section_render))
        .route(
            "/v1/cross-section/artifacts/{render_id}/{file_name}",
            get(cross_section_artifact),
        )
        .route("/v1/sounding/status", get(sounding_status))
        .route("/v1/sounding/render", post(sounding_render))
        .route(
            "/v1/sounding/artifacts/{render_id}/{file_name}",
            get(sounding_artifact),
        )
        .route("/v1/archive/status", get(archive_status))
        .route("/v1/archive/events", get(archive_events))
        .route("/v1/archive/events/{event_id}", get(archive_event))
        .route(
            "/v1/archive/events/{event_id}/polygons",
            get(archive_event_polygons),
        )
        .route(
            "/v1/archive/events/{event_id}/runs",
            get(archive_event_runs),
        )
        .route("/v1/hrrrarchive/status", get(archive_status))
        .route("/v1/hrrrarchive/events", get(archive_events))
        .route("/v1/hrrrarchive/events/{event_id}", get(archive_event))
        .route(
            "/v1/hrrrarchive/events/{event_id}/polygons",
            get(archive_event_polygons),
        )
        .route(
            "/v1/hrrrarchive/events/{event_id}/runs",
            get(archive_event_runs),
        )
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
        .layer(middleware::from_fn(add_standard_headers))
        .layer(CorsLayer::new().allow_origin(Any).allow_methods([
            Method::GET,
            Method::HEAD,
            Method::OPTIONS,
            Method::POST,
        ]))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port).parse()?;
    println!("WxStore listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn add_standard_headers(request: Request<Body>, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    headers.insert("x-frame-options", HeaderValue::from_static("DENY"));
    headers.insert(
        "referrer-policy",
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );
    if !headers.contains_key(header::CACHE_CONTROL) {
        let cache_control = if path == "/livez" || path == "/readyz" || path.ends_with("/status") {
            "no-store"
        } else if path.starts_with("/v1/latest/") || path == "/v1/models" || path == "/v1/variables"
        {
            "public, max-age=30"
        } else {
            "no-store"
        };
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(cache_control),
        );
    }
    response
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
    let source_count = args.manifests.len();
    let mut source_manifests = Vec::with_capacity(source_count);
    let model_overridden = args.model.is_some();
    let run_overridden = args.run.is_some();
    let mut batch_model = args.model.clone();
    let mut batch_run = args.run.clone();
    let member = args.member.clone().or_else(|| Some("control".to_string()));

    let mut records_by_product = BTreeMap::<String, Vec<(PathBuf, RustwxGridExportRecord)>>::new();
    let mut source_records = Vec::<(PathBuf, Vec<Value>)>::with_capacity(source_count);
    for manifest_path in args.manifests {
        let manifest_dir = manifest_path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let manifest: RustwxGridExportManifest = serde_json::from_slice(
            &fs::read(&manifest_path)
                .with_context(|| format!("read {}", manifest_path.display()))?,
        )
        .with_context(|| format!("parse {}", manifest_path.display()))?;
        let model = batch_model.get_or_insert_with(|| manifest.model.clone());
        if !model_overridden && model != &manifest.model {
            bail!(
                "manifest model mismatch in batch import: expected '{}', got '{}' from {}",
                model,
                manifest.model,
                manifest_path.display()
            );
        }
        let run = batch_run.get_or_insert_with(|| manifest.run_id.clone());
        if !run_overridden && run != &manifest.run_id {
            bail!(
                "manifest run mismatch in batch import: expected '{}', got '{}' from {}",
                run,
                manifest.run_id,
                manifest_path.display()
            );
        }
        for record in manifest.fields {
            records_by_product
                .entry(record.product_slug.clone())
                .or_default()
                .push((manifest_dir.clone(), record));
        }
        source_manifests.push(display_path(
            &manifest_path
                .canonicalize()
                .unwrap_or_else(|_| manifest_path.clone()),
        ));
        source_records.push((manifest_path, manifest.blockers));
    }
    let model = batch_model.ok_or_else(|| anyhow!("batch import did not include a model"))?;
    let run = batch_run.ok_or_else(|| anyhow!("batch import did not include a run"))?;

    let mut latlon_cache = HashMap::<PathBuf, Arc<Vec<f32>>>::new();

    let mut wrote = Vec::new();
    for (product, records) in records_by_product {
        let mut grids = Vec::with_capacity(records.len());
        for (manifest_dir, record) in records {
            grids.push(load_rustwx_export_grid(
                manifest_dir.as_path(),
                &model,
                &run,
                member.as_deref(),
                record,
                &mut latlon_cache,
            )?);
        }
        grids.sort_by_key(|grid| grid.forecast_hour);
        let path = write_spatial_wxa_grids(
            &args.spatial_root,
            &model,
            &run,
            member.as_deref(),
            &product,
            &grids,
        )?;
        wrote.push(json!({
            "product": product,
            "path": path,
            "hours": grids.iter().map(|grid| grid.forecast_hour).collect::<Vec<_>>(),
            "bytes": fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0)
        }));
    }

    let run_manifest = write_spatial_run_manifest_batch(
        &args.spatial_root,
        &model,
        &run,
        source_records.as_slice(),
        started.elapsed().as_millis(),
    )?;
    let latest_pointer = if args.publish_latest {
        Some(publish_latest_pointer(
            &args.spatial_root,
            &model,
            &run,
            "import-rustwx-grids",
        )?)
    } else {
        None
    };

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": "wxstore.import_rustwx_grids.report.v1",
            "source_manifest": source_manifests.first().cloned().unwrap_or_default(),
            "source_manifests": source_manifests,
            "source_count": source_count,
            "model": model,
            "run": run,
            "member": member,
            "product_count": wrote.len(),
            "elapsed_ms": started.elapsed().as_millis(),
            "run_manifest": run_manifest,
            "latest_pointer": latest_pointer,
            "wrote": wrote,
            "source_blockers": source_records.iter().flat_map(|(_, blockers)| blockers.iter().cloned()).collect::<Vec<_>>()
        }))?
    );
    Ok(())
}

fn load_rustwx_export_grid(
    manifest_dir: &Path,
    model: &str,
    run: &str,
    member: Option<&str>,
    record: RustwxGridExportRecord,
    latlon_cache: &mut HashMap<PathBuf, Arc<Vec<f32>>>,
) -> Result<SpatialGrid> {
    let values_path = resolve_export_path(manifest_dir, &record.values_path);
    let lat_path = resolve_export_path(manifest_dir, &record.lat_path);
    let lon_path = resolve_export_path(manifest_dir, &record.lon_path);
    let values = read_f32_file(&values_path)
        .with_context(|| format!("read values {}", values_path.display()))?;
    let lat = cached_f32_file(latlon_cache, &lat_path)
        .with_context(|| format!("read latitudes {}", lat_path.display()))?;
    let lon = cached_f32_file(latlon_cache, &lon_path)
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
    let grid_meta = grid_meta_from_latlon(
        model,
        record.nx,
        record.ny,
        lat.as_slice(),
        lon.as_slice(),
        &record,
    );
    Ok(SpatialGrid {
        model: model.to_string(),
        run_id: run.to_string(),
        member: member.map(str::to_string),
        variable: record.product_slug,
        units: record.units,
        forecast_hour: u32::from(record.forecast_hour),
        nx: record.nx,
        ny: record.ny,
        grid_meta,
        values: Arc::new(values),
    })
}

fn inspect_spatial(args: InspectSpatialArgs) -> Result<()> {
    println!(
        "{}",
        serde_json::to_string_pretty(&inspect_spatial_value(
            &args.spatial_root,
            args.model.as_deref(),
        )?)?
    );
    Ok(())
}

fn gc_spatial(args: GcSpatialArgs) -> Result<()> {
    if args.keep_runs == 0 {
        bail!("--keep-runs must be at least 1");
    }
    let model_dir = args.spatial_root.join(&args.model);
    if !model_dir.is_dir() {
        bail!("model directory does not exist: {}", model_dir.display());
    }

    let runs = list_dirs(&model_dir);
    let latest_run =
        read_latest_pointer_run(&args.spatial_root, &args.model).or_else(|| runs.last().cloned());
    let mut protected = BTreeSet::new();
    if let Some(run) = latest_run.as_ref() {
        protected.insert(run.clone());
    }
    for run in runs.iter().rev().take(args.keep_runs) {
        protected.insert(run.clone());
    }

    let candidates = runs
        .iter()
        .filter(|run| !protected.contains(*run))
        .cloned()
        .collect::<Vec<_>>();

    let mut deleted = Vec::new();
    if args.apply {
        let model_canon = model_dir
            .canonicalize()
            .with_context(|| format!("canonicalize {}", model_dir.display()))?;
        for run in &candidates {
            let target = model_dir.join(run);
            let target_canon = target
                .canonicalize()
                .with_context(|| format!("canonicalize {}", target.display()))?;
            if target_canon == model_canon || !target_canon.starts_with(&model_canon) {
                bail!(
                    "refusing to delete path outside model directory: {}",
                    target_canon.display()
                );
            }
            fs::remove_dir_all(&target_canon)
                .with_context(|| format!("delete {}", target_canon.display()))?;
            deleted.push(json!({
                "run": run,
                "path": target_canon
            }));
        }
    }

    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "schema": "wxstore.spatial.gc.v1",
            "spatial_root": args.spatial_root,
            "model": args.model,
            "apply": args.apply,
            "keep_runs": args.keep_runs,
            "latest_run": latest_run,
            "protected_runs": protected.into_iter().collect::<Vec<_>>(),
            "candidate_count": candidates.len(),
            "candidates": candidates,
            "deleted": deleted,
            "note": if args.apply { "deleted local run directories" } else { "dry run; pass --apply to delete local run directories" }
        }))?
    );
    Ok(())
}

fn inspect_spatial_value(root: &Path, model_filter: Option<&str>) -> Result<Value> {
    if !root.is_dir() {
        bail!("spatial root does not exist: {}", root.display());
    }
    let model_ids = if let Some(model) = model_filter {
        vec![model.to_string()]
    } else {
        list_dirs(root)
    };
    let mut models = Vec::new();
    for model in model_ids {
        let model_dir = root.join(&model);
        if !model_dir.is_dir() {
            bail!("model directory does not exist: {}", model_dir.display());
        }
        let runs = list_dirs(&model_dir);
        let latest_pointer = read_latest_pointer_value(root, &model);
        let latest_run = latest_pointer
            .as_ref()
            .and_then(|pointer| {
                pointer
                    .get("run")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .or_else(|| runs.last().cloned());
        let run_summaries = runs
            .iter()
            .map(|run| spatial_run_summary(root, &model, run))
            .collect::<Result<Vec<_>>>()?;
        models.push(json!({
            "model": model,
            "run_count": runs.len(),
            "latest_run": latest_run,
            "latest_pointer": latest_pointer.unwrap_or_else(|| json!({"status": "missing"})),
            "runs": run_summaries
        }));
    }
    Ok(json!({
        "schema": "wxstore.spatial.inspect.v1",
        "spatial_root": root,
        "models": models
    }))
}

fn spatial_run_summary(root: &Path, model: &str, run: &str) -> Result<Value> {
    let run_dir = root.join(model).join(run);
    let manifest_path = run_manifest_path(root, model, run);
    let manifest = fs::read(&manifest_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let product_count = manifest
        .as_ref()
        .and_then(|value| value.get("product_count").and_then(Value::as_u64))
        .unwrap_or_else(|| count_wxa_files(&run_dir) as u64);
    let updated_at = manifest
        .as_ref()
        .and_then(|value| value.get("updated_at").cloned());
    let source_count = manifest
        .as_ref()
        .and_then(|value| value.get("sources").and_then(Value::as_array))
        .map(Vec::len)
        .unwrap_or(0);
    Ok(json!({
        "run": run,
        "path": display_path(&run_dir),
        "run_manifest": if manifest_path.is_file() { json!(display_path(&manifest_path)) } else { json!(null) },
        "product_count": product_count,
        "source_count": source_count,
        "updated_at": updated_at,
        "bytes": sum_wxa_bytes(&run_dir)
    }))
}

fn write_spatial_run_manifest_batch(
    root: &Path,
    model: &str,
    run: &str,
    source_records: &[(PathBuf, Vec<Value>)],
    elapsed_ms: u128,
) -> Result<Value> {
    let run_dir = root.join(model).join(run);
    if !run_dir.is_dir() {
        bail!("run directory does not exist: {}", run_dir.display());
    }
    let manifest_path = run_manifest_path(root, model, run);
    let mut sources = fs::read(&manifest_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("sources").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    let imported_at = utc_now_string();
    for (source_manifest, blockers) in source_records {
        sources.push(json!({
            "kind": "rustwx_grid_export",
            "source_manifest": display_path(&source_manifest.canonicalize().unwrap_or_else(|_| source_manifest.to_path_buf())),
            "imported_at": imported_at,
            "elapsed_ms": elapsed_ms,
            "blocker_count": blockers.len(),
            "blockers": blockers
        }));
    }
    if sources.len() > 100 {
        sources.drain(0..sources.len() - 100);
    }
    write_spatial_run_manifest_with_sources(root, model, run, sources)
}

fn write_spatial_run_manifest_with_sources(
    root: &Path,
    model: &str,
    run: &str,
    sources: Vec<Value>,
) -> Result<Value> {
    let run_dir = root.join(model).join(run);
    if !run_dir.is_dir() {
        bail!("run directory does not exist: {}", run_dir.display());
    }
    let manifest_path = run_manifest_path(root, model, run);
    let products = collect_spatial_run_products(root, model, run)?;
    let members = products
        .iter()
        .filter_map(|product| {
            product
                .get("member")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let now = utc_now_string();
    let manifest = json!({
        "schema": "wxstore.spatial.run_manifest.v1",
        "model": model,
        "run": run,
        "run_path": display_path(&run_dir),
        "updated_at": now,
        "product_count": products.len(),
        "members": members,
        "products": products,
        "sources": sources
    });
    atomic_write_json(&manifest_path, &manifest)?;
    Ok(json!({
        "path": display_path(&manifest_path),
        "product_count": manifest.get("product_count").cloned().unwrap_or_else(|| json!(0)),
        "updated_at": now
    }))
}

fn reindex_spatial_run_manifest(
    root: &Path,
    model: &str,
    run: &str,
    source: &str,
) -> Result<Value> {
    let manifest_path = run_manifest_path(root, model, run);
    let mut sources = fs::read(&manifest_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("sources").and_then(Value::as_array).cloned())
        .unwrap_or_default();
    sources.push(json!({
        "kind": "local_reindex",
        "source": source,
        "indexed_at": utc_now_string()
    }));
    if sources.len() > 100 {
        sources.drain(0..sources.len() - 100);
    }
    write_spatial_run_manifest_with_sources(root, model, run, sources)
}

fn collect_spatial_run_products(root: &Path, model: &str, run: &str) -> Result<Vec<Value>> {
    let run_dir = root.join(model).join(run);
    let mut products = Vec::new();
    collect_wxa_products_in_dir(root, &run_dir, None, &mut products)?;
    let members_dir = run_dir.join("members");
    for member in list_dirs(&members_dir) {
        collect_wxa_products_in_dir(
            root,
            &members_dir.join(&member),
            Some(member.as_str()),
            &mut products,
        )?;
    }
    products.sort_by_key(|value| {
        format!(
            "{}|{}",
            value.get("member").and_then(Value::as_str).unwrap_or(""),
            value.get("product").and_then(Value::as_str).unwrap_or("")
        )
    });
    Ok(products)
}

fn collect_wxa_products_in_dir(
    root: &Path,
    dir: &Path,
    member: Option<&str>,
    products: &mut Vec<Value>,
) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file() || !path.extension().is_some_and(|ext| ext == "wxa") {
            continue;
        }
        let (meta, index) = read_wxa_dense2d_metadata(&path)
            .with_context(|| format!("inspect WXA {}", path.display()))?;
        let chunks = index.len();
        let valid_points = index
            .iter()
            .map(|record| u64::from(record.valid_count))
            .sum::<u64>();
        products.push(json!({
            "product": meta.variable,
            "member": member,
            "path": relative_path_string(root, &path),
            "format": "wxa_dense2d",
            "bytes": fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0),
            "units": meta.units,
            "nx": meta.nx,
            "ny": meta.ny,
            "forecast_hours": meta.forecast_hours,
            "chunk_y": meta.chunk_y,
            "chunk_x": meta.chunk_x,
            "chunk_count": chunks,
            "valid_points": valid_points,
            "grid": meta.grid
        }));
    }
    Ok(())
}

fn publish_latest_pointer(root: &Path, model: &str, run: &str, source: &str) -> Result<Value> {
    let run_dir = root.join(model).join(run);
    if !run_dir.is_dir() {
        bail!(
            "cannot publish missing run directory: {}",
            run_dir.display()
        );
    }
    let pointer_path = latest_pointer_path(root, model);
    let manifest_path = run_manifest_path(root, model, run);
    if !manifest_path.is_file() {
        reindex_spatial_run_manifest(root, model, run, source)?;
    }
    if let Some(current) = read_latest_pointer_value(root, model) {
        if let Some(current_run) = current.get("run").and_then(Value::as_str) {
            if run_cycle_order(current_run, run) == Some(Ordering::Greater) {
                return Ok(json!({
                    "path": display_path(&pointer_path),
                    "model": model,
                    "run": run,
                    "published": false,
                    "skipped": true,
                    "reason": "existing_latest_is_newer",
                    "current_run": current_run,
                    "current_published_at": current.get("published_at").cloned().unwrap_or(Value::Null)
                }));
            }
        }
    }
    let pointer = json!({
        "schema": "wxstore.spatial.latest.v1",
        "model": model,
        "run": run,
        "published_at": utc_now_string(),
        "source": source,
        "run_path": relative_path_string(root, &run_dir),
        "run_manifest": if manifest_path.is_file() { json!(relative_path_string(root, &manifest_path)) } else { json!(null) }
    });
    atomic_write_json(&pointer_path, &pointer)?;
    Ok(json!({
        "path": display_path(&pointer_path),
        "model": model,
        "run": run,
        "published": true,
        "published_at": pointer.get("published_at").cloned()
    }))
}

fn run_cycle_order(left: &str, right: &str) -> Option<Ordering> {
    Some(parse_run_id_cycle_utc(left)?.cmp(&parse_run_id_cycle_utc(right)?))
}

fn read_latest_pointer_run(root: &Path, model: &str) -> Option<String> {
    let pointer = read_latest_pointer_value(root, model)?;
    let run = pointer.get("run")?.as_str()?.to_string();
    root.join(model).join(&run).is_dir().then_some(run)
}

fn read_latest_pointer_value(root: &Path, model: &str) -> Option<Value> {
    serde_json::from_slice::<Value>(&fs::read(latest_pointer_path(root, model)).ok()?).ok()
}

fn latest_pointer_path(root: &Path, model: &str) -> PathBuf {
    root.join(model).join("latest.json")
}

fn run_manifest_path(root: &Path, model: &str, run: &str) -> PathBuf {
    root.join(model).join(run).join("run-manifest.json")
}

fn atomic_write_json(path: &Path, value: &Value) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(value)?;
    bytes.push(b'\n');
    atomic_write_bytes(path, &bytes)
}

fn atomic_write_bytes(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("wxstore");
    let tmp_path = path.with_file_name(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    {
        let mut file =
            File::create(&tmp_path).with_context(|| format!("create {}", tmp_path.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("write {}", tmp_path.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", tmp_path.display()))?;
    }
    fs::rename(&tmp_path, path)
        .with_context(|| format!("publish {} -> {}", tmp_path.display(), path.display()))?;
    Ok(())
}

fn utc_now_string() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn display_path(path: &Path) -> String {
    path.display().to_string()
}

fn relative_path_string(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn count_wxa_files(dir: &Path) -> usize {
    if !dir.is_dir() {
        return 0;
    }
    let direct = fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            let path = entry.path();
            path.is_file() && path.extension().is_some_and(|ext| ext == "wxa")
        })
        .count();
    let member_count = list_dirs(&dir.join("members"))
        .iter()
        .map(|member| count_wxa_files(&dir.join("members").join(member)))
        .sum::<usize>();
    direct + member_count
}

fn sum_wxa_bytes(dir: &Path) -> u64 {
    if !dir.is_dir() {
        return 0;
    }
    let direct = fs::read_dir(dir)
        .ok()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|ext| ext == "wxa"))
        .filter_map(|path| fs::metadata(path).ok().map(|meta| meta.len()))
        .sum::<u64>();
    let member_bytes = list_dirs(&dir.join("members"))
        .iter()
        .map(|member| sum_wxa_bytes(&dir.join("members").join(member)))
        .sum::<u64>();
    direct + member_bytes
}

struct AppState {
    profile: Option<Arc<ProfileLane>>,
    diagnostic: Option<Arc<DiagnosticLane>>,
    spatial: Option<Arc<SpatialLane>>,
    static_plots: Option<Arc<StaticPlotLane>>,
    evidence: Option<Arc<EvidenceBundleLane>>,
    observations: Option<Arc<ObservationLane>>,
    mesoanalysis_innovation: Option<Arc<MesoanalysisInnovationLane>>,
    satellite_tiles: Option<Arc<SatelliteTileLane>>,
    radar_tiles: Option<Arc<RadarTileLane>>,
    plot_lab: Arc<PlotLabLane>,
    cross_sections: Arc<CrossSectionLane>,
    soundings: Arc<SoundingLane>,
    archive: Option<Arc<ArchiveLane>>,
    ops_root: PathBuf,
    cache: RwLock<ResponseCache>,
}

impl AppState {
    fn profile_lane(&self) -> Result<&ProfileLane, ApiError> {
        self.profile
            .as_deref()
            .ok_or_else(|| service_unavailable("profile store is not configured"))
    }

    fn profile_lane_arc(&self) -> Result<Arc<ProfileLane>, ApiError> {
        self.profile
            .as_ref()
            .cloned()
            .ok_or_else(|| service_unavailable("profile store is not configured"))
    }
}

#[derive(Default)]
struct ResponseCache {
    entries: HashMap<String, Bytes>,
    order: VecDeque<String>,
    bytes: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

#[derive(Debug, Clone, Serialize)]
struct CacheStats {
    entries: usize,
    entries_limit: usize,
    bytes: usize,
    bytes_limit: usize,
    hits: u64,
    misses: u64,
    evictions: u64,
}

struct StaticPlotLane {
    root: PathBuf,
    manifest_cache: RwLock<StaticPlotManifestCache>,
}

struct EvidenceBundleLane {
    root: PathBuf,
}

struct ObservationLane {
    root: PathBuf,
    object_cache: RwLock<ObservationObjectCache>,
}

#[derive(Default)]
struct ObservationObjectCache {
    fingerprint: String,
    objects: Vec<Value>,
    errors: Vec<Value>,
}

struct ObservationObjectSnapshot {
    objects: Vec<Value>,
    errors: Vec<Value>,
}

struct MesoanalysisInnovationLane {
    root: PathBuf,
}

struct SatelliteTileLane {
    root: PathBuf,
}

struct RadarTileLane {
    root: PathBuf,
    sidecar_cache: RwLock<RadarSidecarCache>,
}

#[derive(Default)]
struct RadarSidecarCache {
    entries: HashMap<PathBuf, RadarSidecarCacheEntry>,
    order: VecDeque<PathBuf>,
}

struct RadarSidecarCacheEntry {
    fingerprint: RadarSidecarFingerprint,
    data: Arc<RadarPolarSidecarData>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RadarSidecarFingerprint {
    manifest: RadarFileFingerprint,
    values: RadarFileFingerprint,
    gate_flags: RadarFileFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RadarFileFingerprint {
    len: u64,
    modified: Option<SystemTime>,
}

impl RadarSidecarCache {
    fn insert(
        &mut self,
        path: PathBuf,
        fingerprint: RadarSidecarFingerprint,
        data: Arc<RadarPolarSidecarData>,
    ) {
        self.entries
            .insert(path.clone(), RadarSidecarCacheEntry { fingerprint, data });
        self.touch(&path);
        while self.entries.len() > RADAR_SIDECAR_CACHE_MAX_ENTRIES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if oldest != path {
                self.entries.remove(&oldest);
            }
        }
    }

    fn touch(&mut self, path: &Path) {
        self.order.retain(|entry| entry != path);
        self.order.push_back(path.to_path_buf());
    }

    fn remove(&mut self, path: &Path) {
        self.entries.remove(path);
        self.order.retain(|entry| entry != path);
    }
}

#[derive(Debug, Deserialize)]
struct RadarSampleQuery {
    layer: String,
    frame: String,
    product: Option<String>,
    tilt: Option<String>,
    lat: f64,
    lon: f64,
    method: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RadarPolarSidecarManifest {
    schema: String,
    sidecar_version: u8,
    ok: bool,
    name: String,
    site: RadarPolarSidecarSite,
    product: String,
    product_name: String,
    units: String,
    #[serde(default)]
    value_meanings: Vec<RadarPolarValueMeaning>,
    #[serde(default)]
    product_provenance: Value,
    source_key_or_url: Option<String>,
    scan_time_utc: String,
    sweep_index: usize,
    elevation_deg: f32,
    nyquist_velocity_ms: Option<f32>,
    processing_state: String,
    radial_count: usize,
    max_gate_count: usize,
    gate_count: usize,
    values_path: String,
    values_encoding: String,
    gate_flags_path: String,
    gate_flags_encoding: String,
    #[serde(default)]
    gate_flag_meanings: Vec<RadarPolarGateFlagMeaning>,
    radials: Vec<RadarPolarRadialMeta>,
    #[serde(default)]
    qc: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RadarPolarGateFlagMeaning {
    bit: u8,
    mask: u8,
    name: String,
    description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RadarPolarValueMeaning {
    value: f32,
    name: String,
    label: String,
    description: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RadarPolarSidecarSite {
    id: String,
    name: String,
    state: String,
    lat: f64,
    lon: f64,
    elevation_m: Option<f64>,
    #[serde(default)]
    feedhorn_height_m: Option<f64>,
    #[serde(default)]
    antenna_elevation_m: Option<f64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct RadarPolarRadialMeta {
    radial_index: usize,
    azimuth_deg: f32,
    elevation_deg: f32,
    azimuth_spacing_deg: f32,
    gate_count: usize,
    first_gate_range_m: u16,
    gate_spacing_m: u16,
    nyquist_velocity_ms: Option<f32>,
    data_word_size_bits: Option<u16>,
    scale: Option<f32>,
    offset: Option<f32>,
}

struct RadarPolarSidecarData {
    manifest: RadarPolarSidecarManifest,
    values: Vec<f32>,
    gate_flags: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RadarPolarSampleMethod {
    Nearest,
    Interpolated,
}

#[derive(Debug, Clone, Copy)]
struct RadarRelativePolar {
    azimuth_deg: f32,
    ground_range_m: f64,
}

fn validate_radar_gate_flag_meanings(manifest: &RadarPolarSidecarManifest) -> Result<()> {
    for (name, mask) in RADAR_REQUIRED_GATE_FLAG_MEANINGS {
        let present = manifest.gate_flag_meanings.iter().any(|meaning| {
            meaning.name == *name && meaning.mask == *mask && !meaning.description.trim().is_empty()
        });
        if !present {
            bail!("radar sidecar gate_flag_meanings missing {name} mask {mask}");
        }
    }
    Ok(())
}

struct PlotLabLane {
    root: PathBuf,
    direct_batch_bin: PathBuf,
    cache_root: PathBuf,
}

struct CrossSectionLane {
    renderer: PathBuf,
    store_root: PathBuf,
    artifact_root: PathBuf,
}

struct SoundingLane {
    renderer: PathBuf,
    volume_renderer: PathBuf,
    pressure_volume_root: PathBuf,
    artifact_root: PathBuf,
    cache_root: PathBuf,
}

struct ArchiveLane {
    root: PathBuf,
}

#[derive(Debug, Deserialize)]
struct ArchiveEventsQuery {
    rank: Option<String>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PlotLabBounds {
    west: f64,
    east: f64,
    south: f64,
    north: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct PlotLabRenderRequest {
    model: String,
    date: String,
    cycle_utc: Option<u8>,
    forecast_hour: Option<u16>,
    source: Option<String>,
    region: String,
    product: String,
    domain_slug: Option<String>,
    bounds: Option<PlotLabBounds>,
    projection_variant: Option<String>,
    plot_style: Option<String>,
    output_width: Option<u32>,
    output_height: Option<u32>,
    supersample_factor: Option<u32>,
    chrome_scale: Option<f32>,
    presentation_pad_fraction: Option<f64>,
    inverse_raster_crop_pad_cells: Option<usize>,
    inverse_raster_geo_clip: Option<bool>,
    basemap_graticule: Option<bool>,
    native_fill_level_multiplier: Option<usize>,
    place_label_density: Option<u8>,
    linework_width_boost: Option<u32>,
    linework_alpha_scale: Option<f32>,
    barb_width: Option<u32>,
    barb_length_px: Option<f64>,
    barb_density: Option<f64>,
}

#[derive(Default)]
struct StaticPlotManifestCache {
    loaded_at: Option<Instant>,
    records: Vec<StaticPlotManifestRecord>,
    summary: Option<Value>,
}

impl PlotLabLane {
    fn from_env(ops_root: &Path) -> Result<Self> {
        let root = std::env::var_os("WXSTORE_PLOT_LAB_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| ops_root.join("plot_lab"));
        let direct_batch_bin = std::env::var_os("RUSTWX_DIRECT_BATCH_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from("/opt/free-weather-api/build/rustwx-target/release/direct_batch")
            });
        let cache_root = std::env::var_os("RUSTWX_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| ops_root.join("cache"));
        fs::create_dir_all(&root)
            .with_context(|| format!("create plot lab root {}", root.display()))?;
        Ok(Self {
            root,
            direct_batch_bin,
            cache_root,
        })
    }

    fn config_json(&self) -> Value {
        json!({
            "schema": "wxstore.plot_lab.config.v1",
            "status": "ready",
            "root": self.root,
            "direct_batch_bin": self.direct_batch_bin,
            "cache_root": self.cache_root,
            "models": ["gfs", "hrrr", "rap", "gefs", "ecmwf"],
            "sources": ["nomads", "ecmwf-open-data"],
            "regions": plot_lab_regions_json(),
            "products": [
                "500mb_height_winds",
                "2m_temperature",
                "2m_dewpoint",
                "2m_relative_humidity",
                "10m_winds",
                "10m_wind_gusts",
                "mslp_10m_winds",
                "850mb_temperature_winds",
                "700mb_relative_humidity",
                "total_qpf",
                "composite_reflectivity",
                "total_cloud_cover",
                "sbcape"
            ],
            "projection_variants": ["auto", "rectangular", "pivotal", "albers", "mercator", "robinson"],
            "plot_styles": ["clean_atlas", "default"]
        })
    }

    fn artifact_path(&self, render_id: &str, file_name: &str) -> Result<PathBuf> {
        if !safe_path_component(render_id) || !safe_path_component(file_name) {
            bail!("invalid plot lab artifact path");
        }
        let root = fs::canonicalize(&self.root)
            .with_context(|| format!("canonicalize plot lab root {}", self.root.display()))?;
        let path = fs::canonicalize(self.root.join(render_id).join(file_name))
            .with_context(|| format!("canonicalize plot lab artifact {render_id}/{file_name}"))?;
        if !path.starts_with(&root) {
            bail!("plot lab artifact escapes root: {}", path.display());
        }
        if !path.is_file() {
            bail!("plot lab artifact is missing: {}", path.display());
        }
        Ok(path)
    }

    fn render(&self, request: PlotLabRenderRequest) -> Result<Value> {
        let request = normalize_plot_lab_request(request)?;
        let render_id = plot_lab_render_id(&request);
        let out_dir = self.root.join(&render_id);
        fs::create_dir_all(&out_dir)
            .with_context(|| format!("create plot lab render dir {}", out_dir.display()))?;

        let mut command = ProcessCommand::new(&self.direct_batch_bin);
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .env(
                "RUSTWX_PLOT_STYLE",
                request.plot_style.as_deref().unwrap_or("clean_atlas"),
            )
            .env(
                "RUSTWX_PROJECTION_VARIANT",
                request.projection_variant.as_deref().unwrap_or("auto"),
            )
            .env(
                "RUSTWX_STATIC_OUTPUT_WIDTH",
                request.output_width.unwrap_or(1600).to_string(),
            )
            .env(
                "RUSTWX_STATIC_OUTPUT_HEIGHT",
                request.output_height.unwrap_or(900).to_string(),
            )
            .env(
                "RUSTWX_SUPERSAMPLE_FACTOR",
                request.supersample_factor.unwrap_or(2).to_string(),
            )
            .env(
                "RUSTWX_CHROME_SCALE",
                request.chrome_scale.unwrap_or(0.9).to_string(),
            )
            .env(
                "RUSTWX_PRESENTATION_PAD_FRACTION",
                request
                    .presentation_pad_fraction
                    .unwrap_or(0.06)
                    .to_string(),
            )
            .env(
                "RUSTWX_INVERSE_RASTER_CROP_PAD_CELLS",
                request
                    .inverse_raster_crop_pad_cells
                    .unwrap_or(1000)
                    .to_string(),
            )
            .env(
                "RUSTWX_INVERSE_RASTER_GEO_CLIP",
                bool_env_value(request.inverse_raster_geo_clip.unwrap_or(true)),
            )
            .env(
                "RUSTWX_BASEMAP_GRATICULE",
                bool_env_value(request.basemap_graticule.unwrap_or(true)),
            )
            .env(
                "RUSTWX_LINEWORK_WIDTH_BOOST",
                request.linework_width_boost.unwrap_or(0).to_string(),
            )
            .env(
                "RUSTWX_LINEWORK_ALPHA_SCALE",
                request.linework_alpha_scale.unwrap_or(1.0).to_string(),
            )
            .env(
                "RUSTWX_BARB_WIDTH",
                request.barb_width.unwrap_or(2).to_string(),
            )
            .env(
                "RUSTWX_BARB_LENGTH_PX",
                request.barb_length_px.unwrap_or(20.0).to_string(),
            )
            .env(
                "RUSTWX_BARB_DENSITY",
                request.barb_density.unwrap_or(1.0).to_string(),
            )
            .arg("--model")
            .arg(&request.model)
            .arg("--date")
            .arg(&request.date)
            .arg("--forecast-hour")
            .arg(request.forecast_hour.unwrap_or(0).to_string())
            .arg("--region")
            .arg(&request.region)
            .arg("--recipe")
            .arg(&request.product)
            .arg("--out-dir")
            .arg(&out_dir)
            .arg("--cache-dir")
            .arg(&self.cache_root)
            .arg("--native-fill-level-multiplier")
            .arg(
                request
                    .native_fill_level_multiplier
                    .unwrap_or(1)
                    .to_string(),
            )
            .arg("--place-label-density")
            .arg(request.place_label_density.unwrap_or(0).to_string());
        if let Some(cycle_utc) = request.cycle_utc {
            command.arg("--cycle").arg(cycle_utc.to_string());
        }
        if let Some(source) = request.source.as_deref().filter(|value| !value.is_empty()) {
            command.arg("--source").arg(source);
        }
        if let Some(bounds) = request.bounds.as_ref() {
            command.arg(format!(
                "--bounds={},{},{},{}",
                bounds.west, bounds.east, bounds.south, bounds.north
            ));
            command.arg(format!(
                "--domain-slug={}",
                request.domain_slug.as_deref().unwrap_or("plot_lab_custom")
            ));
        }

        let started = Instant::now();
        let output = command.output().with_context(|| {
            format!("run plot lab renderer {}", self.direct_batch_bin.display())
        })?;
        let elapsed_ms = started.elapsed().as_millis();
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if !output.status.success() {
            bail!(
                "plot lab render failed with status {:?}\nstdout:\n{}\nstderr:\n{}",
                output.status.code(),
                stdout,
                stderr
            );
        }
        let artifact = newest_file_with_extension(&out_dir, "png")?;
        let file_name = artifact
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("plot lab artifact has invalid filename"))?
            .to_string();
        let version = static_plot_artifact_version(&artifact);
        let mut url = format!(
            "/v1/plot-lab/artifacts/{}/{}",
            url_path_segment(&render_id),
            url_path_segment(&file_name)
        );
        if let Some(version) = version.as_deref() {
            url.push_str("?v=");
            url.push_str(&url_encode_query_component(version));
        }
        Ok(json!({
            "schema": "wxstore.plot_lab.render.v1",
            "status": "complete",
            "render_id": render_id,
            "elapsed_ms": elapsed_ms,
            "artifact": {
                "file_name": file_name,
                "path": artifact,
                "url": url,
                "bytes": fs::metadata(&artifact).ok().map(|meta| meta.len()),
                "version": version
            },
            "request": request,
            "stdout": truncate_string(stdout, 8000),
            "stderr": truncate_string(stderr, 8000)
        }))
    }
}

impl CrossSectionLane {
    fn from_env(ops_root: &Path) -> Result<Self> {
        let renderer = std::env::var_os("WXSTORE_CROSS_SECTION_RENDERER")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let node_path = PathBuf::from(
                    "/opt/free-weather-api/build/rustwx-target/release/volume_store_cross_section_render",
                );
                if node_path.exists() {
                    node_path
                } else {
                    ops_root.join("bin").join(if cfg!(windows) {
                        "volume_store_cross_section_render.exe"
                    } else {
                        "volume_store_cross_section_render"
                    })
                }
            });
        let store_root = std::env::var_os("WXSTORE_PRESSURE_VOLUME_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| ops_root.join("pressure_volume"));
        let artifact_root = std::env::var_os("WXSTORE_CROSS_SECTION_ARTIFACT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| ops_root.join("cross_sections"));
        fs::create_dir_all(&artifact_root).with_context(|| {
            format!(
                "create cross-section artifact root {}",
                artifact_root.display()
            )
        })?;
        Ok(Self {
            renderer,
            store_root,
            artifact_root,
        })
    }

    fn status_json(&self) -> Value {
        let stores = self.available_stores();
        let renderer_present = self.renderer.is_file();
        let status = if renderer_present && !stores.is_empty() {
            "ready"
        } else if !renderer_present {
            "renderer_missing"
        } else {
            "store_missing"
        };
        json!({
            "schema": "wxstore.cross_section.status.v1",
            "status": status,
            "renderer": self.renderer,
            "renderer_present": renderer_present,
            "store_root": self.store_root,
            "artifact_root": self.artifact_root,
            "stores": stores
        })
    }

    fn available_stores(&self) -> Vec<Value> {
        let mut stores = Vec::new();
        let Ok(models) = fs::read_dir(&self.store_root) else {
            return stores;
        };
        for model_entry in models.flatten() {
            let model_path = model_entry.path();
            if !model_path.is_dir() {
                continue;
            }
            let model = model_entry.file_name().to_string_lossy().to_string();
            let Ok(runs) = fs::read_dir(&model_path) else {
                continue;
            };
            for run_entry in runs.flatten() {
                let run_path = run_entry.path();
                let store_path = run_path.join("store");
                if pressure_volume_store_complete(&store_path) {
                    stores.push(json!({
                        "model": model,
                        "run": run_entry.file_name().to_string_lossy(),
                        "store": store_path
                    }));
                }
            }
        }
        stores.sort_by(|a, b| {
            let ak = format!("{}|{}", a["model"], a["run"]);
            let bk = format!("{}|{}", b["model"], b["run"]);
            bk.cmp(&ak)
        });
        stores
    }

    fn resolve_run(&self, model: &str, run: &str) -> Result<String> {
        let run = if run == "latest" {
            self.latest_run(model)
                .ok_or_else(|| anyhow!("no latest pressure volume store for {model}"))?
        } else {
            run.to_string()
        };
        let store = self.store_root.join(model).join(&run).join("store");
        if !pressure_volume_store_complete(&store) {
            bail!(
                "pressure volume store is not available for {model}/{run}: {}",
                store.display()
            );
        }
        Ok(run)
    }

    fn resolve_store(&self, model: &str, run: &str) -> Result<PathBuf> {
        let run = self.resolve_run(model, run)?;
        Ok(self.store_root.join(model).join(run).join("store"))
    }

    fn latest_run(&self, model: &str) -> Option<String> {
        let latest = self.store_root.join(model).join("latest.json");
        fs::read(latest)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| value.get("run").and_then(Value::as_str).map(str::to_string))
            .or_else(|| {
                let model_path = self.store_root.join(model);
                let mut runs = list_dirs(&model_path)
                    .into_iter()
                    .filter(|run| {
                        pressure_volume_store_complete(&model_path.join(run).join("store"))
                    })
                    .collect::<Vec<_>>();
                runs.sort();
                runs.pop()
            })
    }

    fn artifact_path(&self, render_id: &str, file_name: &str) -> Result<PathBuf> {
        if !safe_path_component(render_id) || !safe_path_component(file_name) {
            bail!("invalid cross-section artifact path");
        }
        let root = fs::canonicalize(&self.artifact_root).with_context(|| {
            format!(
                "canonicalize cross-section artifact root {}",
                self.artifact_root.display()
            )
        })?;
        let path = root.join(render_id).join(file_name);
        let canonical = fs::canonicalize(&path)
            .with_context(|| format!("cross-section artifact not found: {}", path.display()))?;
        if !canonical.starts_with(&root) {
            bail!("cross-section artifact path escapes root");
        }
        Ok(canonical)
    }
}

impl SoundingLane {
    fn from_env(ops_root: &Path) -> Result<Self> {
        let renderer = std::env::var_os("WXSTORE_SOUNDING_RENDERER")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let node_path = PathBuf::from(
                    "/opt/free-weather-api/build/rustwx-target/release/sounding_plot",
                );
                if node_path.exists() {
                    node_path
                } else {
                    ops_root.join("bin").join(if cfg!(windows) {
                        "sounding_plot.exe"
                    } else {
                        "sounding_plot"
                    })
                }
            });
        let volume_renderer = std::env::var_os("WXSTORE_VOLUME_SOUNDING_RENDERER")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let node_path = PathBuf::from(
                    "/opt/free-weather-api/build/rustwx-target/release/volume_store_sounding_render",
                );
                if node_path.exists() {
                    node_path
                } else {
                    ops_root.join("bin").join(if cfg!(windows) {
                        "volume_store_sounding_render.exe"
                    } else {
                        "volume_store_sounding_render"
                    })
                }
            });
        let pressure_volume_root = std::env::var_os("WXSTORE_PRESSURE_VOLUME_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| ops_root.join("pressure_volume"));
        let artifact_root = std::env::var_os("WXSTORE_SOUNDING_ARTIFACT_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| ops_root.join("soundings"));
        let cache_root = std::env::var_os("WXSTORE_SOUNDING_CACHE_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| ops_root.join("cache"));
        fs::create_dir_all(&artifact_root).with_context(|| {
            format!("create sounding artifact root {}", artifact_root.display())
        })?;
        fs::create_dir_all(&cache_root)
            .with_context(|| format!("create sounding cache root {}", cache_root.display()))?;
        Ok(Self {
            renderer,
            volume_renderer,
            pressure_volume_root,
            artifact_root,
            cache_root,
        })
    }

    fn status_json(&self) -> Value {
        let renderer_present = self.renderer.is_file();
        let volume_renderer_present = self.volume_renderer.is_file();
        let stores = self.available_pressure_stores();
        let status = if volume_renderer_present && !stores.is_empty() {
            "ready"
        } else if renderer_present {
            "legacy_renderer_ready"
        } else {
            "renderer_missing"
        };
        json!({
            "schema": "wxstore.sounding.status.v1",
            "status": status,
            "renderer": self.renderer,
            "renderer_present": renderer_present,
            "volume_renderer": self.volume_renderer,
            "volume_renderer_present": volume_renderer_present,
            "pressure_volume_root": self.pressure_volume_root,
            "pressure_stores": stores,
            "artifact_root": self.artifact_root,
            "cache_root": self.cache_root
        })
    }

    fn available_pressure_stores(&self) -> Vec<Value> {
        let mut stores = Vec::new();
        let Ok(models) = fs::read_dir(&self.pressure_volume_root) else {
            return stores;
        };
        for model_entry in models.flatten() {
            let model_path = model_entry.path();
            if !model_path.is_dir() {
                continue;
            }
            let model = model_entry.file_name().to_string_lossy().to_string();
            let Ok(runs) = fs::read_dir(&model_path) else {
                continue;
            };
            for run_entry in runs.flatten() {
                let store_path = run_entry.path().join("store");
                if pressure_volume_store_complete(&store_path) {
                    stores.push(json!({
                        "model": model,
                        "run": run_entry.file_name().to_string_lossy(),
                        "store": store_path
                    }));
                }
            }
        }
        stores.sort_by(|a, b| {
            let ak = format!("{}|{}", a["model"], a["run"]);
            let bk = format!("{}|{}", b["model"], b["run"]);
            bk.cmp(&ak)
        });
        stores
    }

    fn latest_volume_run(&self, model: &str) -> Option<String> {
        let latest = self.pressure_volume_root.join(model).join("latest.json");
        fs::read(latest)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|value| value.get("run").and_then(Value::as_str).map(str::to_string))
            .or_else(|| {
                let model_path = self.pressure_volume_root.join(model);
                let mut runs = list_dirs(&model_path)
                    .into_iter()
                    .filter(|run| {
                        pressure_volume_store_complete(&model_path.join(run).join("store"))
                    })
                    .collect::<Vec<_>>();
                runs.sort();
                runs.pop()
            })
    }

    fn resolve_volume_run(&self, model: &str, run: &str) -> Result<String> {
        let run = if run == "latest" {
            self.latest_volume_run(model)
                .ok_or_else(|| anyhow!("no latest pressure volume store for {model}"))?
        } else {
            run.to_string()
        };
        let store = self
            .pressure_volume_root
            .join(model)
            .join(&run)
            .join("store");
        if !pressure_volume_store_complete(&store) {
            bail!(
                "pressure volume store is not available for {model}/{run}: {}",
                store.display()
            );
        }
        Ok(run)
    }

    fn resolve_volume_store(&self, model: &str, run: &str) -> Result<(String, PathBuf)> {
        let resolved_run = self.resolve_volume_run(model, run)?;
        let store = self
            .pressure_volume_root
            .join(model)
            .join(&resolved_run)
            .join("store");
        Ok((resolved_run, store))
    }

    fn artifact_path(&self, render_id: &str, file_name: &str) -> Result<PathBuf> {
        if !safe_path_component(render_id) || !safe_path_component(file_name) {
            bail!("invalid sounding artifact path");
        }
        let root = fs::canonicalize(&self.artifact_root).with_context(|| {
            format!(
                "canonicalize sounding artifact root {}",
                self.artifact_root.display()
            )
        })?;
        let path = root.join(render_id).join(file_name);
        let canonical = fs::canonicalize(&path)
            .with_context(|| format!("sounding artifact not found: {}", path.display()))?;
        if !canonical.starts_with(&root) {
            bail!("sounding artifact path escapes root");
        }
        Ok(canonical)
    }
}

impl ArchiveLane {
    fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root.join("events")).with_context(|| {
            format!(
                "create archive events root {}",
                root.join("events").display()
            )
        })?;
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn status_json(&self) -> Value {
        let events = self.event_summaries(None, None).unwrap_or_default();
        let complete_pressure_stores = events
            .iter()
            .flat_map(|event| {
                event
                    .get("runs")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
            })
            .filter(|run| {
                run.get("pressure_store_complete")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
            })
            .count();
        json!({
            "schema": "wxstore.archive.status.v1",
            "status": if events.is_empty() { "empty" } else { "ready" },
            "root": self.root,
            "event_count": events.len(),
            "complete_pressure_stores": complete_pressure_stores,
            "events_url": "/v1/hrrrarchive/events"
        })
    }

    fn event_summaries(&self, rank: Option<&str>, limit: Option<usize>) -> Result<Vec<Value>> {
        let mut summaries = Vec::new();
        for event_id in list_dirs(&self.root.join("events")) {
            let event = self.read_event(&event_id)?;
            if !archive_rank_matches(&event, rank) {
                continue;
            }
            let runs = archive_runs_with_store_status(&event);
            summaries.push(json!({
                "event_id": event_id,
                "convective_day": event.get("convective_day").cloned().unwrap_or(Value::String(event_id.clone())),
                "max_outlook": event.get("max_outlook").cloned().unwrap_or(Value::Null),
                "peak_iso": event.get("peak_iso").cloned().unwrap_or(Value::Null),
                "tornado_count": event.get("tornado_count").cloned().unwrap_or(Value::Null),
                "max_ef": event.get("max_ef").cloned().unwrap_or(Value::Null),
                "mrgl": event.get("mrgl").cloned().unwrap_or(Value::Null),
                "bounds": event.get("bounds").cloned().unwrap_or(Value::Null),
                "runs": runs,
                "url": format!("/v1/hrrrarchive/events/{event_id}"),
                "polygons_url": format!("/v1/hrrrarchive/events/{event_id}/polygons"),
                "runs_url": format!("/v1/hrrrarchive/events/{event_id}/runs")
            }));
        }
        summaries.sort_by(|a, b| {
            let ak = a
                .get("convective_day")
                .and_then(Value::as_str)
                .unwrap_or("");
            let bk = b
                .get("convective_day")
                .and_then(Value::as_str)
                .unwrap_or("");
            ak.cmp(bk)
        });
        if let Some(limit) = limit {
            summaries.truncate(limit);
        }
        Ok(summaries)
    }

    fn read_event(&self, event_id: &str) -> Result<Value> {
        if !safe_path_component(event_id) {
            bail!("invalid archive event id");
        }
        let path = self.root.join("events").join(event_id).join("event.json");
        serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("read archive event {}", path.display()))?,
        )
        .with_context(|| format!("parse archive event {}", path.display()))
    }

    fn event_json(&self, event_id: &str) -> Result<Value> {
        let mut event = self.read_event(event_id)?;
        let runs = archive_runs_with_store_status(&event);
        if let Some(object) = event.as_object_mut() {
            object.insert("runs".to_string(), Value::Array(runs));
            object.insert(
                "polygons_url".to_string(),
                Value::String(format!("/v1/hrrrarchive/events/{event_id}/polygons")),
            );
            object.insert(
                "runs_url".to_string(),
                Value::String(format!("/v1/hrrrarchive/events/{event_id}/runs")),
            );
        }
        Ok(event)
    }

    fn polygons_json(&self, event_id: &str) -> Result<Value> {
        if !safe_path_component(event_id) {
            bail!("invalid archive event id");
        }
        let event_dir = self.root.join("events").join(event_id);
        let mrgl = read_optional_json(event_dir.join("mrgl.geojson"))?;
        let volume_mask = read_optional_json(event_dir.join("volume_mask.geojson"))?;
        let mut features = Vec::new();
        if let Some(value) = mrgl {
            append_geojson_features(&mut features, "mrgl", value);
        }
        if let Some(value) = volume_mask {
            append_geojson_features(&mut features, "volume_mask", value);
        }
        Ok(json!({
            "type": "FeatureCollection",
            "schema": "wxstore.archive.polygons.v1",
            "event_id": event_id,
            "features": features
        }))
    }

    fn runs_json(&self, event_id: &str) -> Result<Value> {
        let event = self.read_event(event_id)?;
        Ok(json!({
            "schema": "wxstore.archive.runs.v1",
            "event_id": event_id,
            "model": "hrrr_archive",
            "runs": archive_runs_with_store_status(&event)
        }))
    }
}

fn archive_rank_matches(event: &Value, rank: Option<&str>) -> bool {
    let Some(rank) = rank.map(|value| value.to_ascii_lowercase()) else {
        return true;
    };
    let outlook = event
        .get("max_outlook")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_ascii_uppercase();
    match rank.as_str() {
        "high" => outlook == "HIGH",
        "mdt" | "moderate" => outlook == "MDT",
        "mdt-plus" | "mdt_plus" | "moderate-plus" | "moderate_plus" => {
            outlook == "MDT" || outlook == "HIGH"
        }
        _ => true,
    }
}

fn archive_runs_with_store_status(event: &Value) -> Vec<Value> {
    event
        .get("runs")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|mut run| {
            let complete = run
                .get("store_path")
                .and_then(Value::as_str)
                .map(|path| pressure_volume_store_complete(Path::new(path)))
                .unwrap_or(false);
            let pressure_hours = if complete {
                run.get("store_path")
                    .and_then(Value::as_str)
                    .and_then(|path| pressure_volume_store_hours(Path::new(path)))
            } else {
                None
            };
            let static_hours = archive_static_plot_hours_from_run(&run);
            let run_id = run
                .get("run_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if let Some(object) = run.as_object_mut() {
                object.insert(
                    "model".to_string(),
                    Value::String("hrrr_archive".to_string()),
                );
                object.insert("pressure_store_complete".to_string(), Value::Bool(complete));
                if let Some(hours) = pressure_hours {
                    object.insert("pressure_fhours".to_string(), hours_to_json_array(&hours));
                } else {
                    object.remove("pressure_fhours");
                }
                if let Some(hours) = static_hours {
                    object.insert("plot_fhours".to_string(), hours_to_json_array(&hours));
                }
                object.insert(
                    "cross_section_model".to_string(),
                    Value::String("hrrr_archive".to_string()),
                );
                object.insert(
                    "cross_section_run".to_string(),
                    Value::String(run_id.clone()),
                );
                object.insert(
                    "cross_section_url".to_string(),
                    Value::String("/v1/cross-section/render".to_string()),
                );
            }
            run
        })
        .collect()
}

fn read_optional_json(path: PathBuf) -> Result<Option<Value>> {
    if !path.is_file() {
        return Ok(None);
    }
    serde_json::from_slice(&fs::read(&path).with_context(|| format!("read {}", path.display()))?)
        .with_context(|| format!("parse {}", path.display()))
        .map(Some)
}

fn append_geojson_features(features: &mut Vec<Value>, layer: &str, value: Value) {
    match value.get("type").and_then(Value::as_str) {
        Some("FeatureCollection") => {
            if let Some(items) = value.get("features").and_then(Value::as_array) {
                for item in items {
                    let mut item = item.clone();
                    add_archive_layer_property(&mut item, layer);
                    features.push(item);
                }
            }
        }
        Some("Feature") => {
            let mut value = value;
            add_archive_layer_property(&mut value, layer);
            features.push(value);
        }
        Some("Polygon") | Some("MultiPolygon") => {
            features.push(json!({
                "type": "Feature",
                "properties": { "layer": layer },
                "geometry": value
            }));
        }
        _ => {}
    }
}

fn add_archive_layer_property(feature: &mut Value, layer: &str) {
    let Some(object) = feature.as_object_mut() else {
        return;
    };
    let properties = object
        .entry("properties".to_string())
        .or_insert_with(|| json!({}));
    if let Some(properties) = properties.as_object_mut() {
        properties.insert("layer".to_string(), Value::String(layer.to_string()));
    }
}

fn pressure_volume_store_complete(store_path: &Path) -> bool {
    store_path.join("manifest.json").is_file()
        && store_path.join("index.bin").is_file()
        && store_path.join("chunks.bin").is_file()
}

fn pressure_volume_store_hours(store_path: &Path) -> Option<Vec<u16>> {
    let manifest_path = store_path.join("manifest.json");
    let manifest: Value = serde_json::from_slice(&fs::read(manifest_path).ok()?).ok()?;
    let mut hours: Vec<u16> = manifest
        .get("forecast_hours")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|hour| hour.as_u64())
        .filter_map(|hour| u16::try_from(hour).ok())
        .collect();
    hours.sort_unstable();
    hours.dedup();
    Some(hours)
}

fn hours_to_json_array(hours: &[u16]) -> Value {
    Value::Array(
        hours
            .iter()
            .map(|hour| Value::Number(serde_json::Number::from(*hour)))
            .collect(),
    )
}

fn archive_static_plot_hours_from_run(run: &Value) -> Option<Vec<u16>> {
    let date = run.get("date_yyyymmdd").and_then(Value::as_str)?;
    let cycle = run.get("cycle_utc").and_then(Value::as_u64)?;
    let weather_root = archive_weather_root_from_run(run)?;
    let static_dir = weather_root.join("static_plots").join("conus");
    let entries = fs::read_dir(static_dir).ok()?;
    let prefix = format!("rustwx_hrrr_{date}_{cycle}z_f");
    let mut hours = BTreeSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !name.starts_with(&prefix) || !name.ends_with("_run_manifest.json") {
            continue;
        }
        let hour_start = prefix.len();
        let Some(hour_text) = name.get(hour_start..hour_start + 3) else {
            continue;
        };
        let Ok(hour) = hour_text.parse::<u16>() else {
            continue;
        };
        let manifest = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let Some(manifest) = manifest else {
            continue;
        };
        if manifest.get("state").and_then(Value::as_str) == Some("failed") {
            continue;
        }
        let artifact_count = manifest
            .get("artifact_count")
            .and_then(Value::as_u64)
            .or_else(|| {
                manifest
                    .get("artifacts")
                    .and_then(Value::as_array)
                    .map(|artifacts| artifacts.len() as u64)
            })
            .unwrap_or(0);
        if artifact_count == 0 {
            continue;
        }
        hours.insert(hour);
    }
    if hours.is_empty() {
        None
    } else {
        Some(hours.into_iter().collect())
    }
}

fn archive_weather_root_from_run(run: &Value) -> Option<PathBuf> {
    let store_path = Path::new(run.get("store_path").and_then(Value::as_str)?);
    for ancestor in store_path.ancestors() {
        if ancestor.file_name().and_then(|name| name.to_str()) == Some("pressure_volume") {
            return ancestor.parent().map(Path::to_path_buf);
        }
    }
    None
}

fn normalize_plot_lab_request(mut request: PlotLabRenderRequest) -> Result<PlotLabRenderRequest> {
    request.model = normalize_cli_token(&request.model, "model")?;
    request.region = normalize_cli_token(&request.region, "region")?;
    request.product = normalize_cli_token(&request.product, "product")?;
    request.date = request.date.trim().to_string();
    if request.date.len() != 8 || !request.date.chars().all(|ch| ch.is_ascii_digit()) {
        bail!("date must be YYYYMMDD");
    }
    if request.cycle_utc.is_some_and(|cycle| cycle > 23) {
        bail!("cycle_utc must be 0-23");
    }
    if request.forecast_hour.unwrap_or(0) > 840 {
        bail!("forecast_hour is too large");
    }
    if let Some(source) = request.source.as_mut() {
        *source = normalize_cli_token(source, "source")?;
    }
    if let Some(value) = request.projection_variant.as_mut() {
        *value = normalize_cli_token(value, "projection_variant")?;
    }
    if let Some(value) = request.plot_style.as_mut() {
        *value = normalize_cli_token(value, "plot_style")?;
    }
    if let Some(slug) = request.domain_slug.as_mut() {
        *slug = normalize_slug_token(slug, "domain_slug")?;
    }
    if let Some(bounds) = request.bounds.as_ref() {
        if !bounds.west.is_finite()
            || !bounds.east.is_finite()
            || !bounds.south.is_finite()
            || !bounds.north.is_finite()
        {
            bail!("bounds must be finite");
        }
        if bounds.south >= bounds.north {
            bail!("bounds south must be less than north");
        }
        if !(-90.0..=90.0).contains(&bounds.south) || !(-90.0..=90.0).contains(&bounds.north) {
            bail!("bounds latitude values must be between -90 and 90");
        }
    }
    request.output_width = Some(request.output_width.unwrap_or(1600).clamp(640, 4096));
    request.output_height = Some(request.output_height.unwrap_or(900).clamp(480, 4096));
    request.supersample_factor = Some(request.supersample_factor.unwrap_or(2).clamp(1, 4));
    request.chrome_scale = Some(request.chrome_scale.unwrap_or(0.9).clamp(0.6, 1.6));
    request.presentation_pad_fraction = Some(
        request
            .presentation_pad_fraction
            .unwrap_or(0.06)
            .clamp(0.0, 0.25),
    );
    request.inverse_raster_crop_pad_cells = Some(
        request
            .inverse_raster_crop_pad_cells
            .unwrap_or(1000)
            .clamp(0, 5000),
    );
    request.native_fill_level_multiplier = Some(
        request
            .native_fill_level_multiplier
            .unwrap_or(1)
            .clamp(1, 8),
    );
    request.place_label_density = Some(request.place_label_density.unwrap_or(0).min(3));
    request.linework_width_boost = Some(request.linework_width_boost.unwrap_or(0).min(4));
    request.linework_alpha_scale =
        Some(request.linework_alpha_scale.unwrap_or(1.0).clamp(0.25, 2.0));
    request.barb_width = Some(request.barb_width.unwrap_or(2).clamp(1, 8));
    request.barb_length_px = Some(request.barb_length_px.unwrap_or(20.0).clamp(6.0, 48.0));
    request.barb_density = Some(request.barb_density.unwrap_or(1.0).clamp(0.25, 4.0));
    Ok(request)
}

fn normalize_cli_token(value: &str, field: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase();
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
    {
        bail!("{field} contains unsupported characters");
    }
    Ok(value)
}

fn normalize_slug_token(value: &str, field: &str) -> Result<String> {
    let value = value.trim().to_ascii_lowercase().replace('-', "_");
    if value.is_empty()
        || !value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
    {
        bail!("{field} contains unsupported characters");
    }
    Ok(value)
}

fn bool_env_value(value: bool) -> &'static str {
    if value {
        "1"
    } else {
        "0"
    }
}

fn plot_lab_render_id(request: &PlotLabRenderRequest) -> String {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let domain = request
        .domain_slug
        .as_deref()
        .unwrap_or(request.region.as_str());
    format!(
        "{}_{}_{}z_f{:03}_{}_{}_{}",
        now_ms,
        request.model,
        request.cycle_utc.unwrap_or(0),
        request.forecast_hour.unwrap_or(0),
        sanitize_file_component(domain),
        sanitize_file_component(&request.product),
        sanitize_file_component(request.projection_variant.as_deref().unwrap_or("auto"))
    )
}

fn safe_path_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains("..")
        && !value.contains('/')
        && !value.contains('\\')
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

fn sanitize_file_component(value: &str) -> String {
    let slug = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if slug.is_empty() {
        "x".to_string()
    } else {
        slug
    }
}

fn newest_file_with_extension(root: &Path, extension: &str) -> Result<PathBuf> {
    let mut newest: Option<(SystemTime, PathBuf)> = None;
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_file()
            || path
                .extension()
                .and_then(|value| value.to_str())
                .map(|value| !value.eq_ignore_ascii_case(extension))
                .unwrap_or(true)
        {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(UNIX_EPOCH);
        if newest
            .as_ref()
            .map(|(current, _)| modified > *current)
            .unwrap_or(true)
        {
            newest = Some((modified, path));
        }
    }
    newest.map(|(_, path)| path).ok_or_else(|| {
        anyhow!(
            "plot lab render did not produce a PNG in {}",
            root.display()
        )
    })
}

fn truncate_string(value: String, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value;
    }
    let mut out = value.chars().take(max_chars).collect::<String>();
    out.push_str("\n[truncated]");
    out
}

fn plot_lab_regions_json() -> Vec<Value> {
    vec![
        plot_lab_region("global", "Global", -180.0, 179.999, -90.0, 90.0),
        plot_lab_region("conus", "CONUS", -127.0, -66.0, 23.0, 51.5),
        plot_lab_region("north-america", "North America", -170.0, -50.0, 5.0, 84.0),
        plot_lab_region("south-america", "South America", -82.0, -34.0, -56.0, 13.0),
        plot_lab_region("europe", "Europe", -25.0, 45.0, 34.0, 72.0),
        plot_lab_region("africa", "Africa", -20.0, 55.0, -35.0, 38.0),
        plot_lab_region("asia", "Asia", 25.0, 179.999, -10.0, 82.0),
        plot_lab_region("australia", "Australia", 110.0, 180.0, -50.0, 0.0),
        plot_lab_region("antarctica", "Antarctica", -180.0, 179.999, -90.0, -60.0),
        plot_lab_region(
            "pacific-northwest",
            "Pacific Northwest",
            -125.0,
            -110.0,
            41.0,
            49.5,
        ),
        plot_lab_region(
            "california-southwest",
            "California / Southwest",
            -125.0,
            -108.0,
            31.0,
            41.5,
        ),
        plot_lab_region(
            "rockies-high-plains",
            "Rockies / High Plains",
            -112.0,
            -96.0,
            37.0,
            49.5,
        ),
        plot_lab_region(
            "southern-plains",
            "Southern Plains",
            -109.0,
            -90.0,
            25.0,
            40.5,
        ),
        plot_lab_region("great-lakes", "Great Lakes", -97.5, -72.0, 39.0, 50.5),
        plot_lab_region("southeast", "Southeast", -96.0, -72.0, 24.0, 38.5),
        plot_lab_region("northeast", "Northeast", -84.5, -65.0, 36.0, 48.5),
    ]
}

fn plot_lab_region(slug: &str, label: &str, west: f64, east: f64, south: f64, north: f64) -> Value {
    json!({
        "slug": slug,
        "label": label,
        "bounds": {
            "west": west,
            "east": east,
            "south": south,
            "north": north
        }
    })
}

#[derive(Debug, Clone, Deserialize)]
struct StaticPlotRunManifest {
    run_kind: String,
    run_label: String,
    output_root: PathBuf,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    date_yyyymmdd: Option<String>,
    #[serde(default)]
    cycle_utc: Option<u8>,
    #[serde(default)]
    forecast_hour: Option<u16>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    domain_slug: Option<String>,
    #[serde(default)]
    member: Option<String>,
    #[serde(default)]
    ensemble_kind: Option<String>,
    #[serde(default)]
    ensemble_stat: Option<String>,
    #[serde(default)]
    projection_variant: Option<String>,
    #[serde(default)]
    plot_variant: Option<String>,
    #[serde(default)]
    variant: Option<String>,
    state: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    artifacts: Vec<StaticPlotArtifact>,
}

#[derive(Debug, Clone, Deserialize)]
struct StaticPlotArtifact {
    artifact_key: String,
    relative_path: PathBuf,
    state: String,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    content_identity: Option<Value>,
    #[serde(default)]
    input_fetch_keys: Vec<String>,
}

#[derive(Debug, Clone)]
struct StaticPlotManifestRecord {
    id: String,
    path: PathBuf,
    manifest: StaticPlotRunManifest,
}

#[derive(Debug, Clone)]
struct StaticPlotIdentity {
    model: Option<String>,
    date_yyyymmdd: Option<String>,
    cycle_utc: Option<u8>,
    forecast_hour: Option<u16>,
    source: Option<String>,
    domain_slug: Option<String>,
    member: Option<String>,
    ensemble_kind: Option<String>,
    ensemble_stat: Option<String>,
    plot_variant: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct StaticPlotCatalogQuery {
    #[serde(default)]
    include_artifacts: bool,
    #[serde(default)]
    include_coverage: bool,
    #[serde(default)]
    catalog_index: bool,
    manifest_id: Option<String>,
    model: Option<String>,
    date: Option<String>,
    cycle_utc: Option<u8>,
    forecast_hour: Option<u16>,
    source: Option<String>,
    domain: Option<String>,
    member: Option<String>,
    ensemble: Option<String>,
    projection: Option<String>,
    variant: Option<String>,
    product: Option<String>,
    state: Option<String>,
    q: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
    manifest_limit: Option<usize>,
    manifest_offset: Option<usize>,
    artifact_limit: Option<usize>,
    artifact_offset: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct WeatherObjectQuery {
    kind: Option<String>,
    lane: Option<String>,
    category: Option<String>,
    model: Option<String>,
    run: Option<String>,
    product: Option<String>,
    member: Option<String>,
    forecast_hour: Option<u32>,
    valid_time: Option<String>,
    frame: Option<String>,
    tilt: Option<String>,
    threshold: Option<String>,
    #[serde(alias = "ensemble_statistic")]
    ensemble_stat: Option<String>,
    source: Option<String>,
    source_kind: Option<String>,
    network: Option<String>,
    parameter: Option<String>,
    quality_tier: Option<u8>,
    max_age_minutes: Option<f64>,
    q: Option<String>,
    bbox: Option<String>,
    lat: Option<f64>,
    lon: Option<f64>,
    radius_km: Option<f64>,
    limit: Option<usize>,
    offset: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct MesoanalysisInnovationQuery {
    kind: Option<String>,
    station: Option<String>,
    station_key: Option<String>,
    station_id: Option<String>,
    source: Option<String>,
    variable: Option<String>,
    min_case_count: Option<u64>,
    q: Option<String>,
    top: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct StaticPlotArtifactQuery {
    path: Option<String>,
    v: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct StaticPlotMp4ExportQuery {
    model: Option<String>,
    date: Option<String>,
    cycle_utc: Option<u8>,
    domain: Option<String>,
    member: Option<String>,
    ensemble: Option<String>,
    projection: Option<String>,
    variant: Option<String>,
    product: Option<String>,
    source: Option<String>,
    #[serde(default)]
    force: bool,
    fps: Option<f64>,
    crf: Option<u8>,
    preset: Option<String>,
}

#[derive(Debug, Clone)]
struct StaticPlotMp4Frame {
    forecast_hour: u16,
    path: PathBuf,
}

struct StaticPlotMp4Export {
    path: PathBuf,
    relative_path: String,
    frame_count: usize,
    forecast_hours: Vec<u16>,
    rebuilt: bool,
    version: Option<String>,
}

impl EvidenceBundleLane {
    fn open(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!("evidence root does not exist: {}", root.display());
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn bundle_dir(&self) -> PathBuf {
        self.root.join("bundles")
    }

    fn lane_manifest_json(&self) -> Value {
        let summary = self.summary_json();
        json!({
            "schema": "wxstore.lane.v1",
            "id": "evidence_bundles",
            "status": summary.get("status").cloned().unwrap_or_else(|| json!("unavailable")),
            "role": "meteorologist_evidence_bundle_lane",
            "root": self.root,
            "bundle_count": summary.get("bundle_count").cloned().unwrap_or_else(|| json!(0)),
        })
    }

    fn summary_json(&self) -> Value {
        match self.bundle_records() {
            Ok(records) => json!({
                "schema": "wxstore.evidence.summary.v1",
                "status": if records.is_empty() { "empty" } else { "ready" },
                "root": self.root,
                "bundle_root": self.bundle_dir(),
                "bundle_count": records.len(),
            }),
            Err(err) => json!({
                "schema": "wxstore.evidence.summary.v1",
                "status": "error",
                "root": self.root,
                "error": err.to_string(),
                "bundle_count": 0,
            }),
        }
    }

    fn bundles_json(&self) -> Result<Value> {
        let records = self.bundle_records()?;
        Ok(json!({
            "schema": "wxstore.evidence.bundles.v1",
            "status": if records.is_empty() { "empty" } else { "ready" },
            "root": self.root,
            "bundle_count": records.len(),
            "bundles": records,
        }))
    }

    fn bundle_json(&self, bundle_id: &str) -> Result<Value> {
        validate_path_component("evidence bundle id", bundle_id)?;
        let path = self.bundle_dir().join(format!("{bundle_id}.json"));
        if !path.is_file() {
            bail!("evidence bundle is missing: {}", path.display());
        }
        let mut value: Value = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))?;
        if let Some(object) = value.as_object_mut() {
            object.insert("bundle_id".to_string(), json!(bundle_id));
            object.insert("wxstore_path".to_string(), json!(display_path(&path)));
        }
        Ok(value)
    }

    fn bundle_records(&self) -> Result<Vec<Value>> {
        let dir = self.bundle_dir();
        if !dir.is_dir() {
            return Ok(Vec::new());
        }
        let mut records = Vec::new();
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry.with_context(|| format!("read entry in {}", dir.display()))?;
            let path = entry.path();
            if !path.is_file() || path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let Some(bundle_id) = path.file_stem().and_then(|value| value.to_str()) else {
                continue;
            };
            if !safe_path_component(bundle_id) {
                continue;
            }
            let value = fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            let artifact_count = value
                .as_ref()
                .and_then(|value| value.get("artifacts"))
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            records.push(json!({
                "id": bundle_id,
                "url": format!("/v1/evidence/bundles/{bundle_id}"),
                "schema": value.as_ref().and_then(|value| value.get("schema")).and_then(Value::as_str),
                "claim": value.as_ref().and_then(|value| value.get("claim")).and_then(Value::as_str),
                "conclusion": value.as_ref().and_then(|value| value.get("conclusion")).and_then(Value::as_str),
                "source": value.as_ref().and_then(|value| value.get("source")).and_then(Value::as_str),
                "artifact_count": artifact_count,
                "updated_at": value.as_ref().and_then(|value| value.get("updated_at")).and_then(Value::as_str),
                "bytes": fs::metadata(&path).map(|meta| meta.len()).unwrap_or(0),
            }));
        }
        records.sort_by(|a, b| {
            a.get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .cmp(b.get("id").and_then(Value::as_str).unwrap_or_default())
        });
        Ok(records)
    }
}

impl ObservationLane {
    fn open(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!("observations root does not exist: {}", root.display());
        }
        Ok(Self {
            root: root.to_path_buf(),
            object_cache: RwLock::new(ObservationObjectCache::default()),
        })
    }

    fn index_path(&self) -> PathBuf {
        self.root.join("index.json")
    }

    fn source_latest_path(&self, source_id: &str) -> PathBuf {
        self.root
            .join("sources")
            .join(source_id)
            .join("latest_observations.json")
    }

    fn index_json(&self) -> Result<Value> {
        let path = self.index_path();
        if !path.is_file() {
            bail!("observation index is missing: {}", path.display());
        }
        serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))
    }

    fn source_records(&self) -> Result<Vec<Value>> {
        let index = self.index_json()?;
        Ok(index
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    fn lane_manifest_json(&self) -> Value {
        let summary = self.summary_json();
        json!({
            "schema": "wxstore.lane.v1",
            "id": "direct_observations",
            "status": summary.get("status").cloned().unwrap_or_else(|| json!("unavailable")),
            "role": "direct_source_surface_observation_lane",
            "root": self.root,
            "source_count": summary.get("source_count").cloned().unwrap_or_else(|| json!(0)),
            "observation_count": summary.get("observation_count").cloned().unwrap_or_else(|| json!(0)),
        })
    }

    fn summary_json(&self) -> Value {
        match self.source_records() {
            Ok(records) => {
                let observation_count = records
                    .iter()
                    .filter_map(|record| record.get("observation_count").and_then(Value::as_u64))
                    .sum::<u64>();
                let raw_record_count = records
                    .iter()
                    .filter_map(|record| record.get("raw_record_count").and_then(Value::as_u64))
                    .sum::<u64>();
                json!({
                    "schema": "wxstore.observations.summary.v1",
                    "status": if records.is_empty() { "empty" } else { "ready" },
                    "root": self.root,
                    "source_count": records.len(),
                    "observation_count": observation_count,
                    "raw_record_count": raw_record_count,
                })
            }
            Err(err) => json!({
                "schema": "wxstore.observations.summary.v1",
                "status": "error",
                "root": self.root,
                "error": err.to_string(),
                "source_count": 0,
                "observation_count": 0,
                "raw_record_count": 0,
            }),
        }
    }

    fn sources_json(&self) -> Result<Value> {
        let records = self.source_records()?;
        Ok(json!({
            "schema": "wxstore.observations.sources.v1",
            "status": if records.is_empty() { "empty" } else { "ready" },
            "root": self.root,
            "source_count": records.len(),
            "observation_count": records.iter().filter_map(|record| record.get("observation_count").and_then(Value::as_u64)).sum::<u64>(),
            "sources": records,
        }))
    }

    fn source_json(&self, source_id: &str) -> Result<Value> {
        validate_path_component("observation source id", source_id)?;
        let path = self.source_latest_path(source_id);
        if !path.is_file() {
            bail!("observation source is missing: {}", path.display());
        }
        let mut value: Value = serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))?;
        if let Some(object) = value.as_object_mut() {
            object.insert("source_id".to_string(), json!(source_id));
            object.insert("wxstore_path".to_string(), json!(display_path(&path)));
        }
        Ok(value)
    }

    fn weather_objects(&self) -> Result<ObservationObjectSnapshot> {
        let records = self.source_records()?;
        let fingerprint = self.weather_objects_fingerprint(&records);
        if let Ok(cache) = self.object_cache.read() {
            if cache.fingerprint == fingerprint {
                return Ok(ObservationObjectSnapshot {
                    objects: cache.objects.clone(),
                    errors: cache.errors.clone(),
                });
            }
        }

        let mut objects = Vec::new();
        let mut errors = Vec::new();
        for record in records {
            let source_id = record.get("id").and_then(Value::as_str).unwrap_or_default();
            let source_kind = record
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match self.source_json(source_id) {
                Ok(source) => {
                    let items = source.get("observations").and_then(Value::as_array);
                    objects.push(direct_observation_source_object(
                        source_id,
                        source_kind,
                        &record,
                        items,
                    ));
                    if let Some(items) = items {
                        for item in items {
                            objects.push(direct_observation_station_object(
                                source_id,
                                source_kind,
                                item,
                            ));
                        }
                    }
                }
                Err(err) => {
                    objects.push(direct_observation_source_object(
                        source_id,
                        source_kind,
                        &record,
                        None,
                    ));
                    errors.push(json!({
                        "lane": "direct_observations",
                        "source": source_id,
                        "error": err.to_string()
                    }));
                }
            }
        }

        if let Ok(mut cache) = self.object_cache.write() {
            cache.fingerprint = fingerprint;
            cache.objects = objects.clone();
            cache.errors = errors.clone();
        }
        Ok(ObservationObjectSnapshot { objects, errors })
    }

    fn weather_objects_fingerprint(&self, records: &[Value]) -> String {
        let mut parts = Vec::with_capacity(records.len() + 1);
        parts.push(format!("index:{}", file_cache_token(&self.index_path())));
        for record in records {
            if let Some(source_id) = record.get("id").and_then(Value::as_str) {
                parts.push(format!(
                    "{source_id}:{}",
                    file_cache_token(&self.source_latest_path(source_id))
                ));
            }
        }
        parts.join("|")
    }
}

impl MesoanalysisInnovationLane {
    fn open(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!(
                "mesoanalysis innovation index root does not exist: {}",
                root.display()
            );
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    fn station_index_path(&self) -> PathBuf {
        self.root.join("station_index.jsonl")
    }

    fn source_index_path(&self) -> PathBuf {
        self.root.join("source_index.jsonl")
    }

    fn station_watchlist_path(&self) -> PathBuf {
        self.root.join("station_watchlist.json")
    }

    fn source_watchlist_path(&self) -> PathBuf {
        self.root.join("source_watchlist.json")
    }

    fn manifest_json(&self) -> Result<Value> {
        let path = self.manifest_path();
        if !path.is_file() {
            bail!(
                "mesoanalysis innovation manifest is missing: {}",
                path.display()
            );
        }
        serde_json::from_slice(
            &fs::read(&path).with_context(|| format!("read {}", path.display()))?,
        )
        .with_context(|| format!("parse {}", path.display()))
    }

    fn lane_manifest_json(&self) -> Value {
        let summary = self.summary_json();
        json!({
            "schema": "wxstore.lane.v1",
            "id": "mesoanalysis_innovation",
            "status": summary.get("status").cloned().unwrap_or_else(|| json!("unavailable")),
            "role": "surface_mesoanalysis_innovation_history_index",
            "root": self.root,
            "history_case_count": summary.get("history_case_count").cloned().unwrap_or_else(|| json!(0)),
            "station_series_count": summary.get("station_series_count").cloned().unwrap_or_else(|| json!(0)),
            "source_series_count": summary.get("source_series_count").cloned().unwrap_or_else(|| json!(0)),
            "endpoints": {
                "status": "/v1/mesoanalysis/innovation/status",
                "query": "/v1/mesoanalysis/innovation/query",
                "watchlist": "/v1/mesoanalysis/innovation/watchlist"
            }
        })
    }

    fn summary_json(&self) -> Value {
        match self.manifest_json() {
            Ok(manifest) => {
                let station_index_present = self.station_index_path().is_file();
                let source_index_present = self.source_index_path().is_file();
                let station_watchlist_present = self.station_watchlist_path().is_file();
                let source_watchlist_present = self.source_watchlist_path().is_file();
                let history_case_count = manifest
                    .get("history_case_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let station_series_count = manifest
                    .get("station_series_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let source_series_count = manifest
                    .get("source_series_count")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                json!({
                    "schema": "wxstore.surface_mesoanalysis.innovation_status.v1",
                    "status": if station_index_present && source_index_present { "ready" } else { "incomplete" },
                    "root": self.root,
                    "manifest_path": display_path(&self.manifest_path()),
                    "station_index_path": display_path(&self.station_index_path()),
                    "source_index_path": display_path(&self.source_index_path()),
                    "station_watchlist_path": display_path(&self.station_watchlist_path()),
                    "source_watchlist_path": display_path(&self.source_watchlist_path()),
                    "station_index_present": station_index_present,
                    "source_index_present": source_index_present,
                    "station_watchlist_present": station_watchlist_present,
                    "source_watchlist_present": source_watchlist_present,
                    "history_case_count": history_case_count,
                    "station_series_count": station_series_count,
                    "source_series_count": source_series_count,
                    "manifest": manifest,
                })
            }
            Err(err) => json!({
                "schema": "wxstore.surface_mesoanalysis.innovation_status.v1",
                "status": "error",
                "root": self.root,
                "error": err.to_string(),
                "history_case_count": 0,
                "station_series_count": 0,
                "source_series_count": 0,
            }),
        }
    }

    fn query_json(&self, query: &MesoanalysisInnovationQuery) -> Result<Value> {
        let manifest = self.manifest_json()?;
        let top = mesoanalysis_innovation_top(query);
        let mut station_records = if mesoanalysis_innovation_include_station(query) {
            read_jsonl_values(&self.station_index_path())?
                .into_iter()
                .filter(|record| mesoanalysis_innovation_station_matches(record, query))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut source_records = if mesoanalysis_innovation_include_source(query) {
            read_jsonl_values(&self.source_index_path())?
                .into_iter()
                .filter(|record| mesoanalysis_innovation_source_matches(record, query))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        sort_mesoanalysis_innovation_records(&mut station_records);
        sort_mesoanalysis_innovation_records(&mut source_records);
        let station_match_count = station_records.len();
        let source_match_count = source_records.len();
        station_records.truncate(top);
        source_records.truncate(top);
        Ok(json!({
            "schema": "wxstore.surface_mesoanalysis.innovation_query.v1",
            "status": if station_match_count == 0 && source_match_count == 0 { "empty" } else { "ready" },
            "root": self.root,
            "generated_at": utc_now_string(),
            "query": mesoanalysis_innovation_query_json(query, top),
            "manifest": manifest,
            "station_match_count": station_match_count,
            "source_match_count": source_match_count,
            "station_records": station_records,
            "source_records": source_records,
        }))
    }

    fn watchlist_json(&self, query: &MesoanalysisInnovationQuery) -> Result<Value> {
        let manifest = self.manifest_json()?;
        let top = mesoanalysis_innovation_top(query);
        let mut station_items = if mesoanalysis_innovation_include_station(query) {
            read_json_array_values(&self.station_watchlist_path())?
                .into_iter()
                .filter(|record| mesoanalysis_innovation_station_matches(record, query))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let mut source_items = if mesoanalysis_innovation_include_source(query) {
            read_json_array_values(&self.source_watchlist_path())?
                .into_iter()
                .filter(|record| mesoanalysis_innovation_source_matches(record, query))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        sort_mesoanalysis_innovation_records(&mut station_items);
        sort_mesoanalysis_innovation_records(&mut source_items);
        let station_match_count = station_items.len();
        let source_match_count = source_items.len();
        station_items.truncate(top);
        source_items.truncate(top);
        Ok(json!({
            "schema": "wxstore.surface_mesoanalysis.innovation_watchlist.v1",
            "status": if station_match_count == 0 && source_match_count == 0 { "empty" } else { "ready" },
            "root": self.root,
            "generated_at": utc_now_string(),
            "query": mesoanalysis_innovation_query_json(query, top),
            "manifest": manifest,
            "station_match_count": station_match_count,
            "source_match_count": source_match_count,
            "station_items": station_items,
            "source_items": source_items,
        }))
    }
}

fn read_jsonl_values(path: &Path) -> Result<Vec<Value>> {
    if !path.is_file() {
        bail!("JSONL index is missing: {}", path.display());
    }
    let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut records = Vec::new();
    for (line_index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line)
            .with_context(|| format!("parse {} line {}", path.display(), line_index + 1))?;
        records.push(value);
    }
    Ok(records)
}

fn read_json_array_values(path: &Path) -> Result<Vec<Value>> {
    if !path.is_file() {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_slice(
        &fs::read(path).with_context(|| format!("read {}", path.display()))?,
    )
    .with_context(|| format!("parse {}", path.display()))?;
    value
        .as_array()
        .cloned()
        .ok_or_else(|| anyhow!("{} must contain a JSON array", path.display()))
}

fn mesoanalysis_innovation_top(query: &MesoanalysisInnovationQuery) -> usize {
    query
        .top
        .unwrap_or(MESO_INNOVATION_DEFAULT_TOP)
        .min(MESO_INNOVATION_MAX_TOP)
}

fn mesoanalysis_innovation_include_station(query: &MesoanalysisInnovationQuery) -> bool {
    let kind = query.kind.as_deref().unwrap_or("all").to_ascii_lowercase();
    !matches!(kind.as_str(), "source" | "sources")
}

fn mesoanalysis_innovation_include_source(query: &MesoanalysisInnovationQuery) -> bool {
    let kind = query.kind.as_deref().unwrap_or("all").to_ascii_lowercase();
    !matches!(kind.as_str(), "station" | "stations")
}

fn mesoanalysis_innovation_query_json(query: &MesoanalysisInnovationQuery, top: usize) -> Value {
    json!({
        "kind": query.kind.as_deref(),
        "station": query.station.as_deref(),
        "station_key": query.station_key.as_deref(),
        "station_id": query.station_id.as_deref(),
        "source": query.source.as_deref(),
        "variable": query.variable.as_deref(),
        "min_case_count": query.min_case_count,
        "q": query.q.as_deref(),
        "top": top,
        "max_top": MESO_INNOVATION_MAX_TOP,
    })
}

fn mesoanalysis_innovation_station_matches(
    record: &Value,
    query: &MesoanalysisInnovationQuery,
) -> bool {
    if let Some(station) = query.station.as_deref() {
        if !mesoanalysis_innovation_string_matches_any(
            record,
            &["station_key", "station_id"],
            station,
        ) {
            return false;
        }
    }
    if let Some(station_key) = query.station_key.as_deref() {
        if !mesoanalysis_innovation_string_matches_any(record, &["station_key"], station_key) {
            return false;
        }
    }
    if let Some(station_id) = query.station_id.as_deref() {
        if !mesoanalysis_innovation_string_matches_any(record, &["station_id"], station_id) {
            return false;
        }
    }
    mesoanalysis_innovation_common_matches(record, query)
}

fn mesoanalysis_innovation_source_matches(
    record: &Value,
    query: &MesoanalysisInnovationQuery,
) -> bool {
    if query.station.is_some() || query.station_key.is_some() || query.station_id.is_some() {
        return false;
    }
    mesoanalysis_innovation_common_matches(record, query)
}

fn mesoanalysis_innovation_common_matches(
    record: &Value,
    query: &MesoanalysisInnovationQuery,
) -> bool {
    if let Some(source) = query.source.as_deref() {
        if !mesoanalysis_innovation_string_matches_any(record, &["source"], source) {
            return false;
        }
    }
    if let Some(variable) = query.variable.as_deref() {
        if !mesoanalysis_innovation_string_matches_any(record, &["variable"], variable) {
            return false;
        }
    }
    if let Some(min_case_count) = query.min_case_count {
        if mesoanalysis_innovation_record_number(record, &["case_count"]).unwrap_or(0.0)
            < min_case_count as f64
        {
            return false;
        }
    }
    if let Some(q) = query.q.as_deref() {
        let q = q.trim();
        if !q.is_empty()
            && !serde_json::to_string(record)
                .unwrap_or_default()
                .to_ascii_lowercase()
                .contains(&q.to_ascii_lowercase())
        {
            return false;
        }
    }
    true
}

fn mesoanalysis_innovation_string_matches_any(record: &Value, keys: &[&str], needle: &str) -> bool {
    let needle = needle.trim();
    keys.iter().any(|key| {
        record
            .get(*key)
            .and_then(Value::as_str)
            .is_some_and(|value| value.eq_ignore_ascii_case(needle))
    })
}

fn sort_mesoanalysis_innovation_records(records: &mut [Value]) {
    records.sort_by(|a, b| {
        let rank = mesoanalysis_innovation_rank(b)
            .partial_cmp(&mesoanalysis_innovation_rank(a))
            .unwrap_or(Ordering::Equal);
        if rank == Ordering::Equal {
            mesoanalysis_innovation_label(a).cmp(&mesoanalysis_innovation_label(b))
        } else {
            rank
        }
    });
}

fn mesoanalysis_innovation_rank(record: &Value) -> f64 {
    mesoanalysis_innovation_record_number(record, &["watchlist", "severity_score"])
        .or_else(|| mesoanalysis_innovation_record_number(record, &["severity_score"]))
        .or_else(|| mesoanalysis_innovation_record_number(record, &["mean_abs_analysis_error"]))
        .or_else(|| mesoanalysis_innovation_record_number(record, &["mean_candidate_mae"]))
        .or_else(|| mesoanalysis_innovation_record_number(record, &["case_count"]))
        .unwrap_or(0.0)
}

fn mesoanalysis_innovation_label(record: &Value) -> String {
    ["station_key", "station_id", "source", "variable", "reason"]
        .iter()
        .filter_map(|key| record.get(*key).and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join(":")
}

fn mesoanalysis_innovation_record_number(record: &Value, path: &[&str]) -> Option<f64> {
    let mut current = record;
    for key in path {
        current = current.get(*key)?;
    }
    current.as_f64()
}

impl SatelliteTileLane {
    fn open(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!("satellite tiles root does not exist: {}", root.display());
        }
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn layers_json(&self) -> Result<Value> {
        let mut layers = Vec::new();
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("read satellite root {}", self.root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(layer_id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let index_path = path.join("frames.json");
            if !index_path.is_file() {
                continue;
            }
            let frames = self.frames_json(layer_id).unwrap_or_else(|err| {
                json!({
                    "ok": false,
                    "layer": layer_id,
                    "error": err.to_string(),
                    "frames": []
                })
            });
            let frame_count = frames
                .get("frames")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let latest = frames
                .get("frames")
                .and_then(Value::as_array)
                .and_then(|items| items.last())
                .cloned()
                .unwrap_or_else(|| json!(null));
            layers.push(json!({
                "id": layer_id,
                "kind": "satellite_tiles",
                "frame_count": frame_count,
                "latest": latest,
                "frames_url": format!("/v1/satellite/layers/{layer_id}/frames.json"),
                "tile_url_template": format!("/v1/satellite/tiles/{layer_id}/frames/{{frame_id}}/{{z}}/{{x}}/{{y}}.png")
            }));
        }
        layers.sort_by(|a, b| {
            a.get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .cmp(b.get("id").and_then(Value::as_str).unwrap_or_default())
        });
        Ok(json!({
            "schema": "wxstore.satellite.layers.v1",
            "root": self.root,
            "layers": layers
        }))
    }

    fn lane_manifest_json(&self) -> Value {
        let layers = self.layers_json().unwrap_or_else(|err| {
            json!({
                "schema": "wxstore.satellite.layers.v1",
                "root": self.root,
                "error": err.to_string(),
                "layers": []
            })
        });
        let layer_count = layers
            .get("layers")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        json!({
            "schema": "wxstore.lane.v1",
            "id": "satellite_tiles",
            "status": if layer_count > 0 { "ready" } else { "empty" },
            "role": "published_temporal_xyz_tile_lane",
            "root": self.root,
            "layer_count": layer_count,
            "api": {
                "layers": "/v1/satellite/layers",
                "frames": "/v1/satellite/layers/{layer_id}/frames.json",
                "tiles": "/v1/satellite/tiles/{layer_id}/frames/{frame_id}/{z}/{x}/{y}.png",
                "viewer": "/satellite"
            }
        })
    }

    fn frames_json(&self, layer_id: &str) -> Result<Value> {
        validate_path_component("satellite layer", layer_id)?;
        let path = self.root.join(layer_id).join("frames.json");
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let mut value: Value =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        if let Some(frames) = value.get_mut("frames").and_then(Value::as_array_mut) {
            for frame in frames {
                if let Some(template) = frame.get("url_template").and_then(Value::as_str) {
                    let normalized = template.trim_start_matches('/');
                    frame["tile_url_template"] = json!(format!("/v1/satellite/tiles/{normalized}"));
                }
            }
        }
        value["frames_url"] = json!(format!("/v1/satellite/layers/{layer_id}/frames.json"));
        Ok(value)
    }

    fn tile_path(&self, layer_id: &str, frame_id: &str, z: u8, x: u32, y: u32) -> Result<PathBuf> {
        validate_path_component("satellite layer", layer_id)?;
        validate_path_component("satellite frame", frame_id)?;
        let path = self
            .root
            .join(layer_id)
            .join("frames")
            .join(frame_id)
            .join(z.to_string())
            .join(x.to_string())
            .join(format!("{y}.png"));
        let root = fs::canonicalize(&self.root)
            .with_context(|| format!("canonicalize satellite root {}", self.root.display()))?;
        let path = fs::canonicalize(&path)
            .with_context(|| format!("satellite tile not found: {}", path.display()))?;
        if !path.starts_with(&root) {
            bail!("satellite tile escapes root: {}", path.display());
        }
        if !path.is_file() {
            bail!("satellite tile is missing: {}", path.display());
        }
        Ok(path)
    }
}

impl RadarTileLane {
    fn open(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!("radar tiles root does not exist: {}", root.display());
        }
        Ok(Self {
            root: root.to_path_buf(),
            sidecar_cache: RwLock::new(RadarSidecarCache::default()),
        })
    }

    fn layers_json(&self) -> Result<Value> {
        let mut layers = Vec::new();
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("read radar root {}", self.root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(layer_id) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let index_path = path.join("frames.json");
            if !index_path.is_file() {
                continue;
            }
            let frames = self.frames_json(layer_id).unwrap_or_else(|err| {
                json!({
                    "ok": false,
                    "layer": layer_id,
                    "error": err.to_string(),
                    "frames": []
                })
            });
            let frame_count = frames
                .get("frames")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0);
            let latest = frames
                .get("frames")
                .and_then(Value::as_array)
                .and_then(|items| items.last())
                .cloned()
                .unwrap_or_else(|| json!(null));
            layers.push(json!({
                "id": layer_id,
                "kind": "nexrad_level2_tiles",
                "capabilities": ["png_xyz_tiles", "polar_numeric_sidecars", "latlon_gate_sampling"],
                "frame_count": frame_count,
                "latest": latest,
                "frames_url": format!("/v1/radar/layers/{layer_id}/frames.json"),
                "tile_url_template": format!("/v1/radar/tiles/{layer_id}/frames/{{frame_id}}/{{z}}/{{x}}/{{y}}.png"),
                "sidecar_url_template": format!("/v1/radar/sidecars/{layer_id}/frames/{{frame_id}}/polar_sidecar_manifest.json"),
                "tilt_sidecar_url_template": format!("/v1/radar/sidecars/{layer_id}/frames/{{frame_id}}/{{tilt_id}}/polar_sidecar_manifest.json"),
                "sample_url": "/v1/radar/sample?layer={layer_id}&frame={frame_id}&product={product}&tilt={tilt_id}&lat={lat}&lon={lon}"
            }));
        }
        layers.sort_by(|a, b| {
            a.get("id")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .cmp(b.get("id").and_then(Value::as_str).unwrap_or_default())
        });
        Ok(json!({
            "schema": "wxstore.radar.layers.v1",
            "root": self.root,
            "layers": layers
        }))
    }

    fn lane_manifest_json(&self) -> Value {
        let layers = self.layers_json().unwrap_or_else(|err| {
            json!({
                "schema": "wxstore.radar.layers.v1",
                "root": self.root,
                "error": err.to_string(),
                "layers": []
            })
        });
        let layer_count = layers
            .get("layers")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        let latest = layers
            .get("layers")
            .and_then(Value::as_array)
            .and_then(|items| items.iter().filter_map(|item| item.get("latest")).last())
            .cloned()
            .unwrap_or_else(|| json!(null));
        json!({
            "schema": "wxstore.lane.v1",
            "id": "radar_tiles",
            "status": if layer_count > 0 { "ready" } else { "empty" },
            "role": "published_nexrad_level2_xyz_tile_lane",
            "root": self.root,
            "layer_count": layer_count,
            "latest": latest,
            "agent_notes": {
                "source": "rustwx-radar first-party Rust Level-II parser and tile renderer",
                "products": "Layer latest frames include site, product, scan_time_utc, bounds, tile_count, source_key_or_url, QC, and native-resolution metadata",
                "site_scope": "Runner supports explicit sites or sites = [\"all\"]"
            },
            "api": {
                "layers": "/v1/radar/layers",
                "frames": "/v1/radar/layers/{layer_id}/frames.json",
                "tiles": "/v1/radar/tiles/{layer_id}/frames/{frame_id}/{z}/{x}/{y}.png",
                "tilt_tiles": "/v1/radar/tiles/{layer_id}/frames/{frame_id}/{tilt_id}/{z}/{x}/{y}.png",
                "sidecars": "/v1/radar/sidecars/{layer_id}/frames/{frame_id}/polar_sidecar_manifest.json",
                "tilt_sidecars": "/v1/radar/sidecars/{layer_id}/frames/{frame_id}/{tilt_id}/polar_sidecar_manifest.json",
                "sample": "/v1/radar/sample?layer={layer_id}&frame={frame_id}&product={product}&tilt={tilt_id}&lat={lat}&lon={lon}",
                "viewer": "/radar"
            }
        })
    }

    fn frames_json(&self, layer_id: &str) -> Result<Value> {
        validate_path_component("radar layer", layer_id)?;
        let path = self.root.join(layer_id).join("frames.json");
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let mut value: Value =
            serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;
        if let Some(frames) = value.get_mut("frames").and_then(Value::as_array_mut) {
            for frame in frames {
                let frame_id = frame.get("id").and_then(Value::as_str).map(str::to_string);
                if let Some(template) = frame.get("url_template").and_then(Value::as_str) {
                    let normalized = template.trim_start_matches('/');
                    frame["tile_url_template"] = json!(format!("/v1/radar/tiles/{normalized}"));
                }
                if radar_value_present(frame.get("numeric_sidecar")) {
                    if let Some(frame_id) = frame_id.as_deref() {
                        frame["numeric_sidecar_url"] =
                            json!(radar_sidecar_manifest_url(layer_id, frame_id, None));
                    }
                }
                if let Some(tilts) = frame.get_mut("tilts").and_then(Value::as_array_mut) {
                    for tilt in tilts {
                        let tilt_id = tilt.get("id").and_then(Value::as_str).map(str::to_string);
                        if let Some(template) = tilt.get("url_template").and_then(Value::as_str) {
                            let normalized = template.trim_start_matches('/');
                            tilt["tile_url_template"] =
                                json!(format!("/v1/radar/tiles/{normalized}"));
                        }
                        if radar_value_present(tilt.get("numeric_sidecar")) {
                            if let (Some(frame_id), Some(tilt_id)) =
                                (frame_id.as_deref(), tilt_id.as_deref())
                            {
                                tilt["numeric_sidecar_url"] = json!(radar_sidecar_manifest_url(
                                    layer_id,
                                    frame_id,
                                    Some(tilt_id)
                                ));
                            }
                        }
                    }
                }
            }
        }
        value["frames_url"] = json!(format!("/v1/radar/layers/{layer_id}/frames.json"));
        Ok(value)
    }

    fn sidecar_path(
        &self,
        layer_id: &str,
        frame_id: &str,
        tilt_id: Option<&str>,
        sidecar_file: &str,
    ) -> Result<PathBuf> {
        validate_path_component("radar layer", layer_id)?;
        validate_path_component("radar frame", frame_id)?;
        if let Some(tilt_id) = tilt_id {
            validate_path_component("radar tilt", tilt_id)?;
        }
        if !radar_sidecar_file_allowed(sidecar_file) {
            bail!("invalid radar sidecar file");
        }
        let mut path = self.root.join(layer_id).join("frames").join(frame_id);
        if let Some(tilt_id) = tilt_id {
            path = path.join(tilt_id);
        }
        let path = path.join(sidecar_file);
        let root = fs::canonicalize(&self.root)
            .with_context(|| format!("canonicalize radar root {}", self.root.display()))?;
        let path = fs::canonicalize(&path)
            .with_context(|| format!("radar sidecar not found: {}", path.display()))?;
        if !path.starts_with(&root) {
            bail!("radar sidecar escapes root: {}", path.display());
        }
        if !path.is_file() {
            bail!("radar sidecar is missing: {}", path.display());
        }
        Ok(path)
    }

    fn open_cached_sidecar(&self, manifest_path: &Path) -> Result<Arc<RadarPolarSidecarData>> {
        let manifest_path = fs::canonicalize(manifest_path)
            .with_context(|| format!("canonicalize radar sidecar {}", manifest_path.display()))?;
        if let Some(data) = self.cached_sidecar(&manifest_path) {
            return Ok(data);
        }

        let data = Arc::new(RadarPolarSidecarData::open(&manifest_path)?);
        let fingerprint = radar_sidecar_fingerprint(&manifest_path, &data.manifest)?;
        if let Ok(mut cache) = self.sidecar_cache.write() {
            cache.insert(manifest_path, fingerprint, Arc::clone(&data));
        }
        Ok(data)
    }

    fn cached_sidecar(&self, manifest_path: &Path) -> Option<Arc<RadarPolarSidecarData>> {
        let cached = {
            let Ok(cache) = self.sidecar_cache.read() else {
                return None;
            };
            let entry = cache.entries.get(manifest_path)?;
            let fresh = radar_sidecar_fingerprint(manifest_path, &entry.data.manifest).ok()?
                == entry.fingerprint;
            fresh.then(|| Arc::clone(&entry.data))
        };

        if let Some(data) = cached {
            if let Ok(mut cache) = self.sidecar_cache.write() {
                cache.touch(manifest_path);
            }
            Some(data)
        } else {
            if let Ok(mut cache) = self.sidecar_cache.write() {
                cache.remove(manifest_path);
            }
            None
        }
    }

    #[cfg(test)]
    fn cached_sidecar_count(&self) -> usize {
        self.sidecar_cache
            .read()
            .map(|cache| cache.entries.len())
            .unwrap_or(0)
    }

    fn sample_json(&self, query: &RadarSampleQuery) -> Result<Value> {
        if !query.lat.is_finite()
            || !query.lon.is_finite()
            || query.lat < -90.0
            || query.lat > 90.0
            || query.lon < -180.0
            || query.lon > 180.0
        {
            bail!("lat/lon must be finite geographic coordinates");
        }
        let method = radar_sample_method(query.method.as_deref())?;
        let sidecar_path = self.sidecar_path(
            &query.layer,
            &query.frame,
            query.tilt.as_deref(),
            RADAR_POLAR_SIDECAR_MANIFEST_FILE,
        )?;
        let sidecar = self.open_cached_sidecar(&sidecar_path)?;
        if let Some(product) = query.product.as_deref().map(str::trim) {
            if !product.is_empty() && !product.eq_ignore_ascii_case(&sidecar.manifest.product) {
                bail!(
                    "requested product {product} does not match sidecar product {}",
                    sidecar.manifest.product
                );
            }
        }
        sidecar
            .sample_lat_lon(query.lat, query.lon, method)
            .ok_or_else(|| anyhow!("lat/lon is outside the sidecar sweep coverage"))
    }

    fn tile_path(
        &self,
        layer_id: &str,
        frame_id: &str,
        tilt_id: Option<&str>,
        z: u8,
        x: u32,
        y: u32,
    ) -> Result<PathBuf> {
        validate_path_component("radar layer", layer_id)?;
        validate_path_component("radar frame", frame_id)?;
        if let Some(tilt_id) = tilt_id {
            validate_path_component("radar tilt", tilt_id)?;
        }
        let mut path = self.root.join(layer_id).join("frames").join(frame_id);
        if let Some(tilt_id) = tilt_id {
            path = path.join(tilt_id);
        }
        let path = path
            .join(z.to_string())
            .join(x.to_string())
            .join(format!("{y}.png"));
        let root = fs::canonicalize(&self.root)
            .with_context(|| format!("canonicalize radar root {}", self.root.display()))?;
        let path = fs::canonicalize(&path)
            .with_context(|| format!("radar tile not found: {}", path.display()))?;
        if !path.starts_with(&root) {
            bail!("radar tile escapes root: {}", path.display());
        }
        if !path.is_file() {
            bail!("radar tile is missing: {}", path.display());
        }
        Ok(path)
    }
}

impl RadarPolarSampleMethod {
    fn as_str(self) -> &'static str {
        match self {
            Self::Nearest => "nearest",
            Self::Interpolated => "interpolated",
        }
    }
}

impl RadarPolarSidecarData {
    fn open(manifest_path: &Path) -> Result<Self> {
        let bytes =
            fs::read(manifest_path).with_context(|| format!("read {}", manifest_path.display()))?;
        let manifest: RadarPolarSidecarManifest = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse {}", manifest_path.display()))?;
        if manifest.schema != RADAR_POLAR_SIDECAR_SCHEMA {
            bail!("unsupported radar sidecar schema {}", manifest.schema);
        }
        if manifest.sidecar_version != 2 {
            bail!(
                "unsupported radar sidecar version {}",
                manifest.sidecar_version
            );
        }
        if !manifest.ok {
            bail!("radar sidecar manifest is not ok");
        }
        validate_radar_gate_flag_meanings(&manifest)?;
        if manifest.radials.len() != manifest.radial_count {
            bail!(
                "radar sidecar radial metadata mismatch: got {}, expected {}",
                manifest.radials.len(),
                manifest.radial_count
            );
        }
        let manifest_root = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        let values_path = radar_sidecar_data_path(manifest_root, &manifest.values_path, "values")?;
        let gate_flags_path =
            radar_sidecar_data_path(manifest_root, &manifest.gate_flags_path, "gate flags")?;
        let values = read_radar_f32_le(&values_path)?;
        let gate_flags = fs::read(&gate_flags_path)
            .with_context(|| format!("read {}", gate_flags_path.display()))?;
        let expected = manifest.radial_count * manifest.max_gate_count;
        if values.len() != expected {
            bail!(
                "radar sidecar value count mismatch: got {}, expected {}",
                values.len(),
                expected
            );
        }
        if gate_flags.len() != expected {
            bail!(
                "radar sidecar gate flag count mismatch: got {}, expected {}",
                gate_flags.len(),
                expected
            );
        }
        Ok(Self {
            manifest,
            values,
            gate_flags,
        })
    }

    fn sample_lat_lon(&self, lat: f64, lon: f64, method: RadarPolarSampleMethod) -> Option<Value> {
        let polar =
            radar_lat_lon_to_polar(self.manifest.site.lat, self.manifest.site.lon, lat, lon);
        let cos_elev = f64::from(self.manifest.elevation_deg)
            .to_radians()
            .cos()
            .max(0.1);
        let slant_range_m = polar.ground_range_m / cos_elev;
        match method {
            RadarPolarSampleMethod::Nearest => {
                self.sample_nearest(lat, lon, polar, slant_range_m, method)
            }
            RadarPolarSampleMethod::Interpolated if self.manifest.value_meanings.is_empty() => self
                .sample_interpolated(lat, lon, polar, slant_range_m)
                .or_else(|| self.sample_nearest(lat, lon, polar, slant_range_m, method)),
            RadarPolarSampleMethod::Interpolated => self.sample_nearest(
                lat,
                lon,
                polar,
                slant_range_m,
                RadarPolarSampleMethod::Nearest,
            ),
        }
    }

    fn sample_nearest(
        &self,
        lat: f64,
        lon: f64,
        polar: RadarRelativePolar,
        slant_range_m: f64,
        method: RadarPolarSampleMethod,
    ) -> Option<Value> {
        let row = self.nearest_radial_row(polar.azimuth_deg)?;
        let radial = self.manifest.radials.get(row)?;
        let gate_f = radar_gate_fraction(radial, slant_range_m)?;
        let gate_index = gate_f.round() as usize;
        if gate_index >= radial.gate_count {
            return None;
        }
        let value = self
            .value_at(row, gate_index)
            .filter(|value| value.is_finite());
        let flags = self.flags_at(row, gate_index);
        Some(self.sample_response(
            method,
            value,
            lat,
            lon,
            polar,
            slant_range_m,
            radial,
            gate_index,
            gate_f,
            flags,
        ))
    }

    fn sample_interpolated(
        &self,
        lat: f64,
        lon: f64,
        polar: RadarRelativePolar,
        slant_range_m: f64,
    ) -> Option<Value> {
        let (lo_row, hi_row, az_t) = self.bracketing_radial_rows(polar.azimuth_deg)?;
        let lo = self.sample_radial_range(lo_row, slant_range_m);
        let hi = self.sample_radial_range(hi_row, slant_range_m);
        let value = match (lo, hi) {
            (Some(a), Some(b)) => Some(a + (b - a) * az_t as f32),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => None,
        }?;
        let row = self.nearest_radial_row(polar.azimuth_deg)?;
        let radial = self.manifest.radials.get(row)?;
        if radial.gate_count == 0 {
            return None;
        }
        let gate_f = radar_gate_fraction(radial, slant_range_m)?;
        let gate_index = gate_f.round().clamp(0.0, (radial.gate_count - 1) as f64) as usize;
        let flags = self.flags_at(row, gate_index);
        Some(self.sample_response(
            RadarPolarSampleMethod::Interpolated,
            value.is_finite().then_some(value),
            lat,
            lon,
            polar,
            slant_range_m,
            radial,
            gate_index,
            gate_f,
            flags,
        ))
    }

    #[allow(clippy::too_many_arguments)]
    fn sample_response(
        &self,
        method: RadarPolarSampleMethod,
        value: Option<f32>,
        lat: f64,
        lon: f64,
        polar: RadarRelativePolar,
        slant_range_m: f64,
        radial: &RadarPolarRadialMeta,
        gate_index: usize,
        gate_fraction: f64,
        flag_bits: u8,
    ) -> Value {
        let gate_flags = radar_gate_flag_names(flag_bits);
        let processing_state = self.manifest.processing_state.to_ascii_lowercase();
        let raw = processing_state.contains("raw");
        let dealiased =
            processing_state.contains("dealiased") || flag_bits & RADAR_GATE_FLAG_DEALIASED != 0;
        let filtered =
            processing_state.contains("filtered") || flag_bits & RADAR_GATE_FLAG_FILTERED != 0;
        let derived =
            processing_state.contains("derived") || flag_bits & RADAR_GATE_FLAG_DERIVED != 0;
        let product_provenance = self.manifest.product_provenance.clone();
        let value_label = value.and_then(|value| self.value_label(value));
        json!({
            "schema": "wxstore.radar.sample.v1",
            "sidecar_schema": self.manifest.schema.as_str(),
            "method": method.as_str(),
            "lat": lat,
            "lon": lon,
            "value": value,
            "value_label": value_label,
            "units": self.manifest.units.as_str(),
            "product": self.manifest.product.as_str(),
            "product_name": self.manifest.product_name.as_str(),
            "sweep_index": self.manifest.sweep_index,
            "elevation_deg": self.manifest.elevation_deg,
            "nyquist_velocity_ms": self.manifest.nyquist_velocity_ms,
            "azimuth_deg": polar.azimuth_deg,
            "radial_index": radial.radial_index,
            "radial_azimuth_deg": radial.azimuth_deg,
            "radial_elevation_deg": radial.elevation_deg,
            "azimuth_spacing_deg": radial.azimuth_spacing_deg,
            "range_m": slant_range_m,
            "ground_range_m": polar.ground_range_m,
            "gate_index": gate_index,
            "gate_fraction": gate_fraction,
            "first_gate_range_m": radial.first_gate_range_m,
            "gate_spacing_m": radial.gate_spacing_m,
            "gate_flags": gate_flags,
            "gate_flag_bits": flag_bits,
            "processing_state": self.manifest.processing_state,
            "raw": raw,
            "dealiased": dealiased,
            "filtered": filtered,
            "derived": derived,
            "product_provenance": product_provenance.clone(),
            "provenance": {
                "product": product_provenance,
                "source_key_or_url": self.manifest.source_key_or_url.as_deref(),
                "sidecar_name": self.manifest.name.as_str()
            },
            "qc": self.manifest.qc.clone(),
            "scan_time_utc": self.manifest.scan_time_utc.as_str(),
            "site": self.manifest.site.clone()
        })
    }

    fn value_label(&self, value: f32) -> Option<String> {
        self.manifest
            .value_meanings
            .iter()
            .find(|meaning| (value - meaning.value).abs() <= 0.001)
            .map(|meaning| meaning.label.clone())
    }

    fn nearest_radial_row(&self, azimuth_deg: f32) -> Option<usize> {
        self.manifest
            .radials
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                radar_azimuth_diff(a.azimuth_deg, azimuth_deg)
                    .partial_cmp(&radar_azimuth_diff(b.azimuth_deg, azimuth_deg))
                    .unwrap_or(Ordering::Equal)
            })
            .map(|(row, _)| row)
    }

    fn bracketing_radial_rows(&self, azimuth_deg: f32) -> Option<(usize, usize, f64)> {
        if self.manifest.radials.len() < 2 {
            return None;
        }
        let azimuth = radar_normalize_azimuth(azimuth_deg);
        let mut sorted = self
            .manifest
            .radials
            .iter()
            .enumerate()
            .map(|(row, radial)| (row, radar_normalize_azimuth(radial.azimuth_deg)))
            .collect::<Vec<_>>();
        sorted.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(Ordering::Equal));
        let insert_pos = match sorted.binary_search_by(|(_, candidate)| {
            candidate.partial_cmp(&azimuth).unwrap_or(Ordering::Equal)
        }) {
            Ok(index) => index,
            Err(index) => index,
        };
        let lo = if insert_pos == 0 {
            sorted.len() - 1
        } else {
            insert_pos - 1
        };
        let hi = if insert_pos >= sorted.len() {
            0
        } else {
            insert_pos
        };
        let lo_az = sorted[lo].1;
        let hi_az = sorted[hi].1;
        let span = radar_azimuth_span(lo_az, hi_az);
        if span <= 0.001 || span > 10.0 {
            return None;
        }
        let offset = radar_azimuth_span(lo_az, azimuth);
        Some((
            sorted[lo].0,
            sorted[hi].0,
            (offset / span).clamp(0.0, 1.0) as f64,
        ))
    }

    fn sample_radial_range(&self, row: usize, slant_range_m: f64) -> Option<f32> {
        let radial = self.manifest.radials.get(row)?;
        let gate_f = radar_gate_fraction(radial, slant_range_m)?;
        let gate_lo = gate_f.floor() as usize;
        if gate_lo >= radial.gate_count {
            return None;
        }
        let v0 = self.value_at(row, gate_lo)?;
        if !v0.is_finite() {
            return None;
        }
        let gate_hi = gate_lo + 1;
        if gate_hi < radial.gate_count {
            if let Some(v1) = self.value_at(row, gate_hi) {
                if v1.is_finite() {
                    let t = (gate_f - gate_lo as f64) as f32;
                    return Some(v0 + (v1 - v0) * t);
                }
            }
        }
        Some(v0)
    }

    fn value_at(&self, row: usize, gate: usize) -> Option<f32> {
        self.values
            .get(row * self.manifest.max_gate_count + gate)
            .copied()
    }

    fn flags_at(&self, row: usize, gate: usize) -> u8 {
        self.gate_flags
            .get(row * self.manifest.max_gate_count + gate)
            .copied()
            .unwrap_or(RADAR_GATE_FLAG_MISSING)
    }
}

fn radar_sidecar_file_allowed(file_name: &str) -> bool {
    matches!(
        file_name,
        RADAR_POLAR_SIDECAR_MANIFEST_FILE | RADAR_POLAR_VALUES_FILE | RADAR_POLAR_GATE_FLAGS_FILE
    )
}

fn radar_sidecar_manifest_url(layer_id: &str, frame_id: &str, tilt_id: Option<&str>) -> String {
    match tilt_id {
        Some(tilt_id) => format!(
            "/v1/radar/sidecars/{layer_id}/frames/{frame_id}/{tilt_id}/{RADAR_POLAR_SIDECAR_MANIFEST_FILE}"
        ),
        None => {
            format!("/v1/radar/sidecars/{layer_id}/frames/{frame_id}/{RADAR_POLAR_SIDECAR_MANIFEST_FILE}")
        }
    }
}

fn radar_value_present(value: Option<&Value>) -> bool {
    value.is_some_and(|value| !value.is_null())
}

fn radar_sample_method(method: Option<&str>) -> Result<RadarPolarSampleMethod> {
    match method
        .unwrap_or("nearest")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "" | "nearest" => Ok(RadarPolarSampleMethod::Nearest),
        "interpolated" | "interpolate" | "linear" => Ok(RadarPolarSampleMethod::Interpolated),
        other => bail!("unsupported radar sample method {other}"),
    }
}

fn radar_sidecar_content_type(file_name: &str) -> &'static str {
    if file_name.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

fn radar_sidecar_fingerprint(
    manifest_path: &Path,
    manifest: &RadarPolarSidecarManifest,
) -> Result<RadarSidecarFingerprint> {
    let root = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let values_path = radar_sidecar_data_path(root, &manifest.values_path, "values")?;
    let gate_flags_path = radar_sidecar_data_path(root, &manifest.gate_flags_path, "gate flags")?;
    Ok(RadarSidecarFingerprint {
        manifest: radar_file_fingerprint(manifest_path)?,
        values: radar_file_fingerprint(&values_path)?,
        gate_flags: radar_file_fingerprint(&gate_flags_path)?,
    })
}

fn radar_file_fingerprint(path: &Path) -> Result<RadarFileFingerprint> {
    let metadata = fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    Ok(RadarFileFingerprint {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

fn radar_sidecar_data_path(root: &Path, value: &str, label: &str) -> Result<PathBuf> {
    let root = fs::canonicalize(root)
        .with_context(|| format!("canonicalize sidecar root {}", root.display()))?;
    let value_path = Path::new(value);
    let candidate = if value_path.is_absolute() {
        value_path.to_path_buf()
    } else {
        root.join(value_path)
    };
    let path = fs::canonicalize(&candidate)
        .with_context(|| format!("radar sidecar {label} not found: {}", candidate.display()))?;
    if !path.starts_with(&root) {
        bail!(
            "radar sidecar {label} path escapes sidecar root: {}",
            path.display()
        );
    }
    if !path.is_file() {
        bail!("radar sidecar {label} path is missing: {}", path.display());
    }
    Ok(path)
}

fn read_radar_f32_le(path: &Path) -> Result<Vec<f32>> {
    let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
    if bytes.len() % 4 != 0 {
        bail!("{} is not a whole f32 little-endian array", path.display());
    }
    Ok(bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect())
}

fn radar_lat_lon_to_polar(site_lat: f64, site_lon: f64, lat: f64, lon: f64) -> RadarRelativePolar {
    let site_lat_rad = site_lat.to_radians();
    let lat_rad = lat.to_radians();
    let dlat = lat_rad - site_lat_rad;
    let dlon = radar_normalized_lon_delta(lon - site_lon).to_radians();

    let half_dlat = (dlat * 0.5).sin();
    let half_dlon = (dlon * 0.5).sin();
    let haversine =
        half_dlat * half_dlat + site_lat_rad.cos() * lat_rad.cos() * half_dlon * half_dlon;
    let haversine = haversine.clamp(0.0, 1.0);
    let central_angle = 2.0 * haversine.sqrt().atan2((1.0 - haversine).sqrt());

    let y = dlon.sin() * lat_rad.cos();
    let x = site_lat_rad.cos() * lat_rad.sin() - site_lat_rad.sin() * lat_rad.cos() * dlon.cos();
    let mut azimuth = y.atan2(x).to_degrees();
    if azimuth < 0.0 {
        azimuth += 360.0;
    }
    RadarRelativePolar {
        azimuth_deg: azimuth as f32,
        ground_range_m: RADAR_EARTH_AUTHALIC_RADIUS_M * central_angle,
    }
}

#[cfg(test)]
fn radar_polar_to_lat_lon(
    site_lat: f64,
    site_lon: f64,
    azimuth_deg: f32,
    ground_range_m: f64,
) -> (f64, f64) {
    let angular_distance = (ground_range_m / RADAR_EARTH_AUTHALIC_RADIUS_M).max(0.0);
    let site_lat_rad = site_lat.to_radians();
    let site_lon_rad = site_lon.to_radians();
    let azimuth_rad = f64::from(azimuth_deg).to_radians();

    let sin_site_lat = site_lat_rad.sin();
    let cos_site_lat = site_lat_rad.cos();
    let sin_distance = angular_distance.sin();
    let cos_distance = angular_distance.cos();

    let lat =
        (sin_site_lat * cos_distance + cos_site_lat * sin_distance * azimuth_rad.cos()).asin();
    let lon = site_lon_rad
        + (azimuth_rad.sin() * sin_distance * cos_site_lat)
            .atan2(cos_distance - sin_site_lat * lat.sin());

    (lat.to_degrees(), normalize_lon(lon.to_degrees()))
}

fn radar_gate_fraction(radial: &RadarPolarRadialMeta, slant_range_m: f64) -> Option<f64> {
    if radial.gate_spacing_m == 0 {
        return None;
    }
    let gate_f =
        (slant_range_m - f64::from(radial.first_gate_range_m)) / f64::from(radial.gate_spacing_m);
    (gate_f >= 0.0).then_some(gate_f)
}

fn radar_gate_flag_names(flags: u8) -> Vec<String> {
    let mut out = Vec::new();
    if flags & RADAR_GATE_FLAG_VALID != 0 {
        out.push("valid".to_string());
    }
    if flags & RADAR_GATE_FLAG_MISSING != 0 {
        out.push("missing".to_string());
    }
    if flags & RADAR_GATE_FLAG_RANGE_FOLDED != 0 {
        out.push("range_folded".to_string());
    }
    if flags & RADAR_GATE_FLAG_FILTERED != 0 {
        out.push("filtered".to_string());
    }
    if flags & RADAR_GATE_FLAG_DERIVED != 0 {
        out.push("derived".to_string());
    }
    if flags & RADAR_GATE_FLAG_DEALIASED != 0 {
        out.push("dealiased".to_string());
    }
    out
}

fn radar_normalize_azimuth(value: f32) -> f32 {
    let mut value = value % 360.0;
    if value < 0.0 {
        value += 360.0;
    }
    value
}

fn radar_azimuth_diff(a: f32, b: f32) -> f32 {
    let diff = (radar_normalize_azimuth(a) - radar_normalize_azimuth(b)).abs();
    diff.min(360.0 - diff)
}

fn radar_azimuth_span(lo: f32, hi: f32) -> f32 {
    let mut span = radar_normalize_azimuth(hi) - radar_normalize_azimuth(lo);
    if span < 0.0 {
        span += 360.0;
    }
    span
}

fn radar_normalized_lon_delta(delta: f64) -> f64 {
    let mut delta = delta;
    while delta > 180.0 {
        delta -= 360.0;
    }
    while delta < -180.0 {
        delta += 360.0;
    }
    delta
}

fn validate_path_component(label: &str, value: &str) -> Result<()> {
    if safe_path_component(value) {
        Ok(())
    } else {
        bail!("invalid {label} path component")
    }
}

impl StaticPlotLane {
    fn open(root: &Path) -> Result<Self> {
        if !root.is_dir() {
            bail!("static plots root does not exist: {}", root.display());
        }
        Ok(Self {
            root: root.to_path_buf(),
            manifest_cache: RwLock::new(StaticPlotManifestCache::default()),
        })
    }

    fn overview_json(&self) -> Value {
        json!({
            "status": "configured",
            "root": self.root,
            "cache_ttl_seconds": static_plot_manifest_cache_ttl_secs()
        })
    }

    fn lane_manifest_json(&self) -> Value {
        let summary = self.summary_json();
        json!({
            "schema": "wxstore.lane.v1",
            "id": "static_plots",
            "status": summary.get("status").cloned().unwrap_or_else(|| json!("unavailable")),
            "role": "published_static_plot_artifact_lane",
            "root": self.root,
            "manifest_count": summary.get("manifest_count").cloned().unwrap_or_else(|| json!(0)),
            "artifact_count": summary.get("artifact_count").cloned().unwrap_or_else(|| json!(0)),
            "complete_count": summary.get("complete_count").cloned().unwrap_or_else(|| json!(0)),
            "blocked_count": summary.get("blocked_count").cloned().unwrap_or_else(|| json!(0)),
            "failed_count": summary.get("failed_count").cloned().unwrap_or_else(|| json!(0)),
        })
    }

    fn summary_json(&self) -> Value {
        if let Some(summary) = self.cached_summary_json() {
            return summary;
        }
        match self.manifests() {
            Ok(records) => self.cached_summary_json().unwrap_or_else(|| {
                static_plot_summary_json_with_coverage(&self.root, &records, false)
            }),
            Err(err) => json!({
                "status": "error",
                "root": self.root,
                "error": err.to_string(),
                "coverage": []
            }),
        }
    }

    fn catalog_json(&self, query: &StaticPlotCatalogQuery) -> Value {
        match self.manifests() {
            Ok(records) => {
                let summary = if query.include_coverage {
                    static_plot_summary_json_with_coverage(&self.root, &records, true)
                } else {
                    self.cached_summary_json().unwrap_or_else(|| {
                        static_plot_summary_json_with_coverage(&self.root, &records, false)
                    })
                };
                let mut matched = records
                    .iter()
                    .filter(|record| static_plot_record_matches(record, query))
                    .collect::<Vec<_>>();
                if query.catalog_index && !query.include_artifacts {
                    matched = static_plot_catalog_index_records(matched);
                }
                let manifest_offset = query.manifest_offset.unwrap_or(0);
                let manifest_limit = query
                    .manifest_limit
                    .or(query.limit)
                    .unwrap_or(STATIC_PLOT_DEFAULT_MANIFEST_LIMIT)
                    .min(STATIC_PLOT_MAX_MANIFEST_LIMIT);
                let matched_count = matched.len();
                let manifests = matched
                    .into_iter()
                    .skip(manifest_offset)
                    .take(manifest_limit)
                    .map(|record| static_plot_record_json(&self.root, record, query))
                    .collect::<Vec<_>>();
                json!({
                    "schema": "wxstore.static_plots.v1",
                    "status": summary.get("status").cloned().unwrap_or_else(|| json!("empty")),
                    "root": self.root,
                    "summary": summary,
                    "query": {
                        "include_artifacts": query.include_artifacts,
                        "include_coverage": query.include_coverage,
                        "catalog_index": query.catalog_index,
                        "manifest_id": query.manifest_id,
                        "model": query.model,
                        "date": query.date,
                        "cycle_utc": query.cycle_utc,
                        "forecast_hour": query.forecast_hour,
                        "source": query.source,
                        "domain": query.domain,
                        "member": query.member,
                        "ensemble": query.ensemble,
                        "projection": query.projection,
                        "variant": query.variant,
                        "product": query.product,
                        "state": query.state,
                        "q": query.q,
                        "limit": query.limit,
                        "offset": query.offset,
                        "manifest_limit": manifest_limit,
                        "manifest_offset": manifest_offset,
                        "artifact_limit": query.artifact_limit,
                        "artifact_offset": query.artifact_offset
                    },
                    "matched_manifest_count": matched_count,
                    "returned_manifest_count": manifests.len(),
                    "manifests": manifests
                })
            }
            Err(err) => json!({
                "schema": "wxstore.static_plots.v1",
                "status": "error",
                "root": self.root,
                "error": err.to_string(),
                "manifests": []
            }),
        }
    }

    fn artifact_path(&self, manifest_id: &str, artifact_index: usize) -> Result<PathBuf> {
        let records = self.manifests()?;
        let record = records
            .iter()
            .find(|record| record.id == manifest_id)
            .ok_or_else(|| anyhow!("static plot manifest '{manifest_id}' is not loaded"))?;
        let artifact = record.manifest.artifacts.get(artifact_index).ok_or_else(|| {
            anyhow!("static plot artifact index {artifact_index} is outside manifest '{manifest_id}'")
        })?;
        let path = resolve_static_artifact_path(&record.manifest, &artifact.relative_path);
        if !path.is_file() {
            bail!("static plot artifact is missing: {}", path.display());
        }
        Ok(path)
    }

    fn artifact_relative_path(&self, relative_path: &str) -> Result<PathBuf> {
        let path = self.root.join(relative_path);
        let root = fs::canonicalize(&self.root)
            .with_context(|| format!("canonicalize static plot root {}", self.root.display()))?;
        let path = fs::canonicalize(&path)
            .with_context(|| format!("canonicalize static plot artifact {}", path.display()))?;
        if !path.starts_with(&root) {
            bail!("static plot artifact escapes root: {}", path.display());
        }
        if !path.is_file() {
            bail!("static plot artifact is missing: {}", path.display());
        }
        Ok(path)
    }

    fn export_mp4(&self, query: &StaticPlotMp4ExportQuery) -> Result<StaticPlotMp4Export> {
        let product = query
            .product
            .as_deref()
            .and_then(|value| normalized_optional_query(Some(value)))
            .ok_or_else(|| anyhow!("product is required"))?;
        let mut catalog_query = StaticPlotCatalogQuery {
            include_artifacts: false,
            include_coverage: false,
            catalog_index: false,
            manifest_id: None,
            model: query.model.clone(),
            date: query.date.clone(),
            cycle_utc: query.cycle_utc,
            forecast_hour: None,
            domain: query.domain.clone(),
            member: query.member.clone(),
            ensemble: query.ensemble.clone(),
            projection: query.projection.clone(),
            variant: query.variant.clone(),
            product: None,
            state: Some("all".to_string()),
            q: None,
            limit: None,
            offset: None,
            manifest_limit: None,
            manifest_offset: None,
            artifact_limit: None,
            artifact_offset: None,
            source: query.source.clone(),
        };
        if catalog_query.ensemble.is_none() {
            catalog_query.ensemble = query.member.clone();
        }
        if catalog_query.projection.is_none() {
            catalog_query.projection = query.variant.clone();
        }

        let mut frames = self
            .manifests()?
            .into_iter()
            .filter(|record| static_plot_record_matches(record, &catalog_query))
            .filter_map(|record| {
                let identity = static_plot_record_identity(&record);
                let forecast_hour = identity.forecast_hour?;
                let artifact = record.manifest.artifacts.iter().find(|artifact| {
                    normalized_static_plot_product_key(&artifact.artifact_key) == product
                        && matches!(
                            normalized_state(&artifact.state).as_str(),
                            "complete" | "cache_hit"
                        )
                })?;
                let path = resolve_static_artifact_path(&record.manifest, &artifact.relative_path);
                if !path.is_file() || !is_static_plot_image_artifact_path(&path) {
                    return None;
                }
                Some(StaticPlotMp4Frame {
                    forecast_hour,
                    path,
                })
            })
            .collect::<Vec<_>>();
        frames.sort_by(|a, b| {
            a.forecast_hour
                .cmp(&b.forecast_hour)
                .then_with(|| a.path.cmp(&b.path))
        });
        frames.dedup_by_key(|frame| frame.forecast_hour);
        if frames.is_empty() {
            bail!("no complete image frames found for product '{product}'");
        }
        if frames.len() > STATIC_PLOT_EXPORT_MAX_FRAMES {
            bail!(
                "refusing to export {} frames; max is {}",
                frames.len(),
                STATIC_PLOT_EXPORT_MAX_FRAMES
            );
        }

        let export_dir = self.root.join(".exports").join("mp4");
        fs::create_dir_all(&export_dir)
            .with_context(|| format!("create {}", export_dir.display()))?;
        let fingerprint = static_plot_mp4_export_fingerprint(query, &product, &frames);
        let first_hour = frames.first().map(|frame| frame.forecast_hour).unwrap_or(0);
        let last_hour = frames.last().map(|frame| frame.forecast_hour).unwrap_or(0);
        let name = format!(
            "{}_{}_{}z_{}_{}_{}_{}_f{:03}-f{:03}_{}.mp4",
            static_plot_export_slug(query.model.as_deref().unwrap_or("plots")),
            static_plot_export_slug(query.date.as_deref().unwrap_or("run")),
            query.cycle_utc.unwrap_or(0),
            static_plot_export_slug(query.domain.as_deref().unwrap_or("domain")),
            static_plot_export_slug(
                query
                    .ensemble
                    .as_deref()
                    .or(query.member.as_deref())
                    .unwrap_or("control")
            ),
            static_plot_export_slug(
                query
                    .projection
                    .as_deref()
                    .or(query.variant.as_deref())
                    .unwrap_or("auto")
            ),
            static_plot_export_slug(&product),
            first_hour,
            last_hour,
            fingerprint
        );
        let output_path = export_dir.join(name);
        let rebuilt = query.force || !output_path.is_file();
        if rebuilt {
            build_static_plot_mp4(
                &frames,
                &output_path,
                query.fps.unwrap_or(2.0),
                query.crf.unwrap_or(18),
                query.preset.as_deref().unwrap_or("faster"),
            )?;
        }
        let relative_path = relative_path_string(&self.root, &output_path);
        Ok(StaticPlotMp4Export {
            version: static_plot_artifact_version(&output_path),
            path: output_path,
            relative_path,
            frame_count: frames.len(),
            forecast_hours: frames.iter().map(|frame| frame.forecast_hour).collect(),
            rebuilt,
        })
    }

    fn manifests(&self) -> Result<Vec<StaticPlotManifestRecord>> {
        let ttl_secs = static_plot_manifest_cache_ttl_secs();
        if let Ok(cache) = self.manifest_cache.read() {
            if cache
                .loaded_at
                .is_some_and(|loaded_at| loaded_at.elapsed().as_secs() < ttl_secs)
            {
                return Ok(cache.records.clone());
            }
        }
        let mut paths = Vec::new();
        collect_static_plot_manifest_paths(&self.root, &mut paths)?;
        paths.sort();
        let mut records = Vec::new();
        for path in paths {
            let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
            let manifest: StaticPlotRunManifest = serde_json::from_slice(&bytes)
                .with_context(|| format!("parse {}", path.display()))?;
            let relative = path.strip_prefix(&self.root).unwrap_or(&path);
            records.push(StaticPlotManifestRecord {
                id: stable_static_plot_id(relative),
                path,
                manifest,
            });
        }
        records.sort_by(static_plot_record_compare_desc);
        let summary = static_plot_summary_json_with_coverage(&self.root, &records, false);
        if let Ok(mut cache) = self.manifest_cache.write() {
            cache.loaded_at = Some(Instant::now());
            cache.records = records.clone();
            cache.summary = Some(summary);
        }
        Ok(records)
    }

    fn cached_summary_json(&self) -> Option<Value> {
        let ttl_secs = static_plot_manifest_cache_ttl_secs();
        let cache = self.manifest_cache.read().ok()?;
        if !cache
            .loaded_at
            .is_some_and(|loaded_at| loaded_at.elapsed().as_secs() < ttl_secs)
        {
            return None;
        }
        cache.summary.clone()
    }
}

fn static_plot_manifest_cache_ttl_secs() -> u64 {
    std::env::var("WXSTORE_STATIC_PLOT_MANIFEST_CACHE_TTL_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(STATIC_PLOT_MANIFEST_CACHE_TTL_DEFAULT_SECS)
}

fn collect_static_plot_manifest_paths(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_static_plot_manifest_paths(&path, out)?;
        } else if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with("_run_manifest.json"))
        {
            out.push(path);
        }
    }
    Ok(())
}

fn static_plot_summary_json_with_coverage(
    root: &Path,
    records: &[StaticPlotManifestRecord],
    include_coverage: bool,
) -> Value {
    let mut artifact_count = 0usize;
    let mut complete_count = 0usize;
    let mut blocked_count = 0usize;
    let mut failed_count = 0usize;
    let mut coverage = Vec::new();
    for record in records {
        let mut complete = 0usize;
        let mut blocked = 0usize;
        let mut failed = 0usize;
        for artifact in &record.manifest.artifacts {
            artifact_count += 1;
            match normalized_state(&artifact.state).as_str() {
                "complete" | "cache_hit" => {
                    complete += 1;
                    complete_count += 1;
                }
                "blocked" => {
                    blocked += 1;
                    blocked_count += 1;
                }
                "failed" => {
                    failed += 1;
                    failed_count += 1;
                }
                _ => {}
            }
        }
        if include_coverage {
            let identity = static_plot_record_identity(record);
            let plot_variant = static_plot_variant_key(&identity);
            coverage.push(json!({
                "manifest_id": record.id,
                "run_kind": record.manifest.run_kind,
                "run_label": record.manifest.run_label,
                "state": record.manifest.state,
                "model": identity.model,
                "date": identity.date_yyyymmdd,
                "cycle_utc": identity.cycle_utc,
                "forecast_hour": identity.forecast_hour,
                "source": identity.source,
                "domain": identity.domain_slug,
                "member": identity.member,
                "ensemble_kind": identity.ensemble_kind,
                "ensemble_stat": identity.ensemble_stat,
                "ensemble_key": static_plot_ensemble_key(&identity),
                "projection_variant": &plot_variant,
                "plot_variant": &plot_variant,
                "variant": &plot_variant,
                "manifest_path": relative_path_string(root, &record.path),
                "artifact_count": record.manifest.artifacts.len(),
                "complete_count": complete,
                "blocked_count": blocked,
                "failed_count": failed,
            }));
        }
    }

    let mut summary = json!({
        "status": if records.is_empty() { "empty" } else { "ready" },
        "root": root,
        "manifest_count": records.len(),
        "artifact_count": artifact_count,
        "complete_count": complete_count,
        "blocked_count": blocked_count,
        "failed_count": failed_count,
    });
    if include_coverage {
        summary["coverage"] = Value::Array(coverage);
    }
    summary
}

fn static_plot_record_compare_desc(
    a: &StaticPlotManifestRecord,
    b: &StaticPlotManifestRecord,
) -> Ordering {
    let a_id = static_plot_record_identity(a);
    let b_id = static_plot_record_identity(b);
    b_id.date_yyyymmdd
        .cmp(&a_id.date_yyyymmdd)
        .then_with(|| b_id.cycle_utc.cmp(&a_id.cycle_utc))
        .then_with(|| a_id.model.cmp(&b_id.model))
        .then_with(|| a_id.domain_slug.cmp(&b_id.domain_slug))
        .then_with(|| static_plot_variant_key(&a_id).cmp(&static_plot_variant_key(&b_id)))
        .then_with(|| a_id.forecast_hour.cmp(&b_id.forecast_hour))
        .then_with(|| a.path.cmp(&b.path))
}

fn static_plot_catalog_index_records<'a>(
    records: Vec<&'a StaticPlotManifestRecord>,
) -> Vec<&'a StaticPlotManifestRecord> {
    let mut seen = BTreeSet::<(
        Option<String>,
        Option<String>,
        Option<u8>,
        Option<String>,
        Option<String>,
        String,
        String,
    )>::new();
    records
        .into_iter()
        .filter(|record| {
            let identity = static_plot_record_identity(record);
            let ensemble_key = static_plot_ensemble_key(&identity);
            let variant_key = static_plot_variant_key(&identity);
            let key = (
                identity.model,
                identity.date_yyyymmdd,
                identity.cycle_utc,
                identity.source,
                identity.domain_slug,
                ensemble_key,
                variant_key,
            );
            seen.insert(key)
        })
        .collect()
}

fn static_plot_record_matches(
    record: &StaticPlotManifestRecord,
    query: &StaticPlotCatalogQuery,
) -> bool {
    if let Some(manifest_id) = query.manifest_id.as_deref() {
        if record.id != manifest_id {
            return false;
        }
    }
    let identity = static_plot_record_identity(record);
    if let Some(model) = query.model.as_deref() {
        if identity.model.as_deref() != Some(model) {
            return false;
        }
    }
    if let Some(date) = query.date.as_deref() {
        if identity.date_yyyymmdd.as_deref() != Some(date) {
            return false;
        }
    }
    if let Some(cycle_utc) = query.cycle_utc {
        if identity.cycle_utc != Some(cycle_utc) {
            return false;
        }
    }
    if let Some(forecast_hour) = query.forecast_hour {
        if identity.forecast_hour != Some(forecast_hour) {
            return false;
        }
    }
    if let Some(source) = query.source.as_deref() {
        if identity.source.as_deref() != Some(source) {
            return false;
        }
    }
    if let Some(domain) = query.domain.as_deref() {
        if identity.domain_slug.as_deref() != Some(domain) {
            return false;
        }
    }
    if let Some(member) = query.member.as_deref() {
        if static_plot_ensemble_key(&identity) != member {
            return false;
        }
    }
    if let Some(ensemble) = query.ensemble.as_deref() {
        if static_plot_ensemble_key(&identity) != ensemble {
            return false;
        }
    }
    if let Some(projection) = query.projection.as_deref() {
        if !static_plot_variant_filter_matches(&identity, projection) {
            return false;
        }
    }
    if let Some(variant) = query.variant.as_deref() {
        if !static_plot_variant_filter_matches(&identity, variant) {
            return false;
        }
    }
    if let Some(q) = normalized_optional_query(query.q.as_deref()) {
        let haystack = format!(
            "{} {} {} {} {} {}",
            record.manifest.run_label,
            record.manifest.run_kind,
            identity.model.as_deref().unwrap_or_default(),
            identity.domain_slug.as_deref().unwrap_or_default(),
            static_plot_variant_key(&identity),
            record.path.display()
        )
        .to_ascii_lowercase();
        if !haystack.contains(&q)
            && !record
                .manifest
                .artifacts
                .iter()
                .any(|artifact| static_plot_artifact_matches_query(artifact, &q))
        {
            return false;
        }
    }
    true
}

fn static_plot_record_json(
    root: &Path,
    record: &StaticPlotManifestRecord,
    query: &StaticPlotCatalogQuery,
) -> Value {
    let identity = static_plot_record_identity(record);
    let plot_variant = static_plot_variant_key(&identity);
    let artifact_limit = query
        .artifact_limit
        .or(query.limit)
        .unwrap_or(250)
        .min(1000);
    let artifact_offset = query.artifact_offset.or(query.offset).unwrap_or(0);
    let artifacts = if query.include_artifacts {
        record
            .manifest
            .artifacts
            .iter()
            .enumerate()
            .filter(|(_, artifact)| static_plot_artifact_matches_filter(artifact, query))
            .skip(artifact_offset)
            .take(artifact_limit)
            .map(|(index, artifact)| {
                let path = resolve_static_artifact_path(&record.manifest, &artifact.relative_path);
                let version = static_plot_artifact_version(&path);
                let relative_path = relative_path_string(root, &path);
                let encoded_path = url_encode_query_component(&relative_path);
                let mut url = format!(
                    "/v1/static-plots/artifacts/{}/{}?path={}",
                    record.id, index, encoded_path
                );
                if let Some(version) = version.as_deref() {
                    url.push_str("&v=");
                    url.push_str(&url_encode_query_component(version));
                }
                json!({
                    "index": index,
                    "artifact_key": artifact.artifact_key,
                    "state": artifact.state,
                    "detail": artifact.detail,
                    "relative_path": artifact.relative_path,
                    "path": relative_path,
                    "exists": path.is_file(),
                    "url": url,
                    "version": version,
                    "content_identity": artifact.content_identity,
                    "input_fetch_keys": artifact.input_fetch_keys,
                })
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let matched_artifact_count = record
        .manifest
        .artifacts
        .iter()
        .filter(|artifact| static_plot_artifact_matches_filter(artifact, query))
        .count();
    let mut complete_count = 0usize;
    let mut blocked_count = 0usize;
    let mut failed_count = 0usize;
    for artifact in &record.manifest.artifacts {
        match normalized_state(&artifact.state).as_str() {
            "complete" | "cache_hit" => complete_count += 1,
            "blocked" => blocked_count += 1,
            "failed" => failed_count += 1,
            _ => {}
        }
    }
    json!({
        "manifest_id": record.id,
        "run_kind": record.manifest.run_kind,
        "run_label": record.manifest.run_label,
        "output_root": record.manifest.output_root,
        "state": record.manifest.state,
        "detail": record.manifest.detail,
        "model": identity.model,
        "date": identity.date_yyyymmdd,
        "cycle_utc": identity.cycle_utc,
        "forecast_hour": identity.forecast_hour,
        "source": identity.source,
        "domain": identity.domain_slug,
        "member": identity.member,
        "ensemble_kind": identity.ensemble_kind,
        "ensemble_stat": identity.ensemble_stat,
        "ensemble_key": static_plot_ensemble_key(&identity),
        "projection_variant": &plot_variant,
        "plot_variant": &plot_variant,
        "variant": &plot_variant,
        "manifest_path": relative_path_string(root, &record.path),
        "artifact_count": record.manifest.artifacts.len(),
        "complete_count": complete_count,
        "blocked_count": blocked_count,
        "failed_count": failed_count,
        "matched_artifact_count": matched_artifact_count,
        "artifact_limit": if query.include_artifacts { json!(artifact_limit) } else { json!(null) },
        "artifact_offset": if query.include_artifacts { json!(artifact_offset) } else { json!(null) },
        "artifacts": artifacts
    })
}

fn static_plot_artifact_matches_filter(
    artifact: &StaticPlotArtifact,
    query: &StaticPlotCatalogQuery,
) -> bool {
    if let Some(product) = normalized_optional_query(query.product.as_deref()) {
        if normalized_static_plot_product_key(&artifact.artifact_key)
            != normalized_static_plot_product_key(&product)
        {
            return false;
        }
    }
    if let Some(state) = query.state.as_deref() {
        if state != "all" && normalized_state(&artifact.state) != state {
            return false;
        }
    }
    if let Some(q) = normalized_optional_query(query.q.as_deref()) {
        return static_plot_artifact_matches_query(artifact, &q);
    }
    true
}

fn static_plot_artifact_version(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    let modified_ms = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some(format!("{modified_ms:x}-{:x}", metadata.len()))
}

fn url_encode_query_component(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn static_plot_artifact_matches_query(artifact: &StaticPlotArtifact, query: &str) -> bool {
    let haystack = format!(
        "{} {} {}",
        artifact.artifact_key,
        artifact.detail.as_deref().unwrap_or_default(),
        artifact.relative_path.display()
    )
    .to_ascii_lowercase();
    haystack.contains(query)
}

fn normalized_static_plot_product_key(key: &str) -> String {
    key.trim()
        .strip_prefix("direct:")
        .or_else(|| key.trim().strip_prefix("derived:"))
        .or_else(|| key.trim().strip_prefix("windowed:"))
        .or_else(|| key.trim().strip_prefix("ensemble:"))
        .or_else(|| key.trim().strip_prefix("animation:"))
        .or_else(|| key.trim().strip_prefix("animation_webp:"))
        .or_else(|| key.trim().strip_prefix("video_mp4:"))
        .unwrap_or_else(|| key.trim())
        .to_ascii_lowercase()
}

fn is_static_plot_image_artifact_path(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "png" | "webp" | "jpg" | "jpeg"
    )
}

fn static_plot_export_slug(value: &str) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
        } else if matches!(ch, '-' | '_' | '.') {
            out.push('_');
        }
    }
    while out.contains("__") {
        out = out.replace("__", "_");
    }
    out.trim_matches('_').to_string()
}

fn static_plot_mp4_export_fingerprint(
    query: &StaticPlotMp4ExportQuery,
    product: &str,
    frames: &[StaticPlotMp4Frame],
) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for part in [
        query.model.as_deref().unwrap_or_default(),
        query.date.as_deref().unwrap_or_default(),
        query.domain.as_deref().unwrap_or_default(),
        query
            .ensemble
            .as_deref()
            .or(query.member.as_deref())
            .unwrap_or_default(),
        query
            .projection
            .as_deref()
            .or(query.variant.as_deref())
            .unwrap_or_default(),
        product,
    ] {
        fnv1a_update(&mut hash, part.as_bytes());
        fnv1a_update(&mut hash, b"\0");
    }
    fnv1a_update(&mut hash, &query.cycle_utc.unwrap_or(0).to_le_bytes());
    fnv1a_update(&mut hash, &query.fps.unwrap_or(2.0).to_bits().to_le_bytes());
    fnv1a_update(&mut hash, &[query.crf.unwrap_or(18)]);
    fnv1a_update(
        &mut hash,
        query.preset.as_deref().unwrap_or("faster").as_bytes(),
    );
    fnv1a_update(&mut hash, b"\0");
    for frame in frames {
        fnv1a_update(&mut hash, &frame.forecast_hour.to_le_bytes());
        fnv1a_update(&mut hash, frame.path.display().to_string().as_bytes());
        if let Some(version) = static_plot_artifact_version(&frame.path) {
            fnv1a_update(&mut hash, version.as_bytes());
        }
        fnv1a_update(&mut hash, b"\0");
    }
    format!("{hash:016x}")
}

fn fnv1a_update(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x100000001b3);
    }
}

fn build_static_plot_mp4(
    frames: &[StaticPlotMp4Frame],
    output_path: &Path,
    fps: f64,
    crf: u8,
    preset: &str,
) -> Result<()> {
    if !(0.1..=30.0).contains(&fps) {
        bail!("fps must be between 0.1 and 30");
    }
    if crf > 35 {
        bail!("crf must be <= 35");
    }
    let preset = match preset {
        "ultrafast" | "superfast" | "veryfast" | "faster" | "fast" | "medium" | "slow"
        | "slower" | "veryslow" => preset,
        _ => "faster",
    };
    let parent = output_path
        .parent()
        .ok_or_else(|| anyhow!("output mp4 has no parent directory"))?;
    fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    let list_path = output_path.with_extension("ffconcat.txt");
    let tmp_path = output_path.with_extension("tmp.mp4");
    let frame_duration = 1.0 / fps;
    let mut list = String::new();
    for frame in frames {
        let path = frame.path.display().to_string();
        if path.contains('\n') || path.contains('\'') {
            bail!("frame path cannot be encoded for ffmpeg concat list: {path}");
        }
        list.push_str("file '");
        list.push_str(&path);
        list.push_str("'\n");
        list.push_str(&format!("duration {frame_duration:.6}\n"));
    }
    if let Some(last) = frames.last() {
        list.push_str("file '");
        list.push_str(&last.path.display().to_string());
        list.push_str("'\n");
    }
    fs::write(&list_path, list).with_context(|| format!("write {}", list_path.display()))?;
    let vf = "scale=trunc(iw/2)*2:trunc(ih/2)*2,fps=30,format=yuv420p";
    let output = ProcessCommand::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-y")
        .arg("-f")
        .arg("concat")
        .arg("-safe")
        .arg("0")
        .arg("-i")
        .arg(&list_path)
        .arg("-vf")
        .arg(vf)
        .arg("-movflags")
        .arg("+faststart")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg(preset)
        .arg("-crf")
        .arg(crf.to_string())
        .arg(&tmp_path)
        .stdout(Stdio::null())
        .output()
        .with_context(|| "run ffmpeg for static plot mp4 export")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffmpeg mp4 export failed: {stderr}");
    }
    fs::rename(&tmp_path, output_path)
        .with_context(|| format!("publish mp4 {}", output_path.display()))?;
    let _ = fs::remove_file(&list_path);
    Ok(())
}

fn normalized_optional_query(query: Option<&str>) -> Option<String> {
    let query = query?.trim().to_ascii_lowercase();
    if query.is_empty() {
        None
    } else {
        Some(query)
    }
}

fn static_plot_record_identity(record: &StaticPlotManifestRecord) -> StaticPlotIdentity {
    static_plot_identity_with_path(&record.manifest, Some(&record.path))
}

fn static_plot_identity_with_path(
    manifest: &StaticPlotRunManifest,
    manifest_path: Option<&Path>,
) -> StaticPlotIdentity {
    let inferred = infer_static_plot_identity_from_label(&manifest.run_label);
    let model = manifest.model.clone().or(inferred.model);
    let plot_variant = manifest
        .projection_variant
        .as_deref()
        .or(manifest.plot_variant.as_deref())
        .or(manifest.variant.as_deref())
        .and_then(normalized_static_plot_variant)
        .or(inferred.plot_variant)
        .or_else(|| infer_static_plot_variant_from_path(manifest_path))
        .or_else(|| infer_static_plot_variant_from_path(Some(&manifest.output_root)));
    StaticPlotIdentity {
        model,
        date_yyyymmdd: manifest.date_yyyymmdd.clone().or(inferred.date_yyyymmdd),
        cycle_utc: manifest.cycle_utc.or(inferred.cycle_utc),
        forecast_hour: manifest.forecast_hour.or(inferred.forecast_hour),
        source: manifest.source.clone(),
        domain_slug: manifest.domain_slug.clone().or(inferred.domain_slug),
        member: manifest.member.clone().or(inferred.member),
        ensemble_kind: manifest.ensemble_kind.clone().or(inferred.ensemble_kind),
        ensemble_stat: manifest.ensemble_stat.clone().or(inferred.ensemble_stat),
        plot_variant,
    }
}

fn infer_static_plot_identity_from_label(run_label: &str) -> StaticPlotIdentity {
    let label = run_label.strip_prefix("rustwx_").unwrap_or(run_label);
    let label = label.strip_suffix("_non_ecape_hour").unwrap_or(label);
    let parts = label.split('_').collect::<Vec<_>>();
    let Some(date_index) = parts
        .iter()
        .position(|part| part.len() == 8 && part.chars().all(|ch| ch.is_ascii_digit()))
    else {
        return StaticPlotIdentity {
            model: None,
            date_yyyymmdd: None,
            cycle_utc: None,
            forecast_hour: None,
            source: None,
            domain_slug: None,
            member: None,
            ensemble_kind: None,
            ensemble_stat: None,
            plot_variant: None,
        };
    };
    let model = if date_index > 0 {
        let slug = parts[..date_index].join("_");
        if slug.is_empty() {
            None
        } else {
            Some(static_plot_model_slug(&slug))
        }
    } else {
        None
    };
    let date_yyyymmdd = Some(parts[date_index].to_string());
    let cycle_utc = parts
        .get(date_index + 1)
        .and_then(|part| part.strip_suffix('z'))
        .and_then(|part| part.parse::<u8>().ok());
    let forecast_hour = parts
        .get(date_index + 2)
        .and_then(|part| part.strip_prefix('f'))
        .and_then(|part| part.parse::<u16>().ok());
    let (domain_slug, plot_variant) = if parts.len() > date_index + 3 {
        let tail = parts[date_index + 3..].to_vec();
        let (domain_parts, plot_variant) = split_static_plot_domain_variant_parts(&tail);
        let domain_slug = if domain_parts.is_empty() {
            None
        } else {
            Some(domain_parts.join("_"))
        };
        (domain_slug, plot_variant)
    } else {
        (None, None)
    };
    StaticPlotIdentity {
        model,
        date_yyyymmdd,
        cycle_utc,
        forecast_hour,
        source: None,
        domain_slug,
        member: None,
        ensemble_kind: None,
        ensemble_stat: None,
        plot_variant,
    }
}

fn static_plot_ensemble_key(identity: &StaticPlotIdentity) -> String {
    if let Some(member) = identity.member.as_deref() {
        return member.to_string();
    }
    if let Some(stat) = identity.ensemble_stat.as_deref() {
        return stat.to_string();
    }
    "control".to_string()
}

fn static_plot_variant_key(identity: &StaticPlotIdentity) -> String {
    identity
        .plot_variant
        .clone()
        .unwrap_or_else(|| "auto".to_string())
}

fn static_plot_variant_filter_matches(identity: &StaticPlotIdentity, filter: &str) -> bool {
    let Some(filter) = normalized_static_plot_variant(filter) else {
        return true;
    };
    filter == "all" || static_plot_variant_key(identity) == filter
}

fn normalized_static_plot_variant(value: &str) -> Option<String> {
    let slug = normalized_static_plot_variant_slug(value)?;
    Some(
        known_static_plot_variant_alias(&slug)
            .unwrap_or(slug.as_str())
            .to_string(),
    )
}

fn normalized_known_static_plot_variant(value: &str) -> Option<String> {
    let slug = normalized_static_plot_variant_slug(value)?;
    known_static_plot_variant_alias(&slug).map(str::to_string)
}

fn known_static_plot_variant_alias(slug: &str) -> Option<&'static str> {
    match slug {
        "all" => Some("all"),
        "auto" | "default" | "standard" => Some("auto"),
        "geo" | "geographic" | "latlon" | "lat_lon" | "plate_carree" | "equirectangular" => {
            Some("geo")
        }
        "lambert" | "lambert_conformal" | "lambert_conformal_conic" | "lcc" => Some("lambert"),
        "albers" | "albers_equal_area" | "aea" => Some("albers"),
        "mercator" | "merc" | "web_mercator" | "webmercator" => Some("mercator"),
        "robinson" | "robin" => Some("robinson"),
        _ => None,
    }
}

fn normalized_static_plot_variant_slug(value: &str) -> Option<String> {
    let mut slug = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' })
        .collect::<String>();
    while slug.contains("__") {
        slug = slug.replace("__", "_");
    }
    let slug = slug.trim_matches('_');
    if slug.is_empty() {
        return None;
    }
    let slug = slug
        .strip_prefix("projection_variant_")
        .or_else(|| slug.strip_prefix("plot_variant_"))
        .or_else(|| slug.strip_prefix("projection_"))
        .or_else(|| slug.strip_prefix("proj_"))
        .or_else(|| slug.strip_prefix("variant_"))
        .or_else(|| slug.strip_prefix("plot_"))
        .or_else(|| slug.strip_prefix("style_"))
        .unwrap_or(slug);
    if slug.is_empty() {
        None
    } else {
        Some(slug.to_string())
    }
}

fn split_static_plot_domain_variant_parts<'a>(parts: &[&'a str]) -> (Vec<&'a str>, Option<String>) {
    if parts.is_empty() {
        return (Vec::new(), None);
    }

    for index in (0..parts.len()).rev() {
        let variant_marker_after_pair = index > 0
            && parts[index].eq_ignore_ascii_case("variant")
            && (parts[index - 1].eq_ignore_ascii_case("projection")
                || parts[index - 1].eq_ignore_ascii_case("plot"));
        let marker_len = match parts[index].to_ascii_lowercase().as_str() {
            "variant" if variant_marker_after_pair => 0,
            "projection" | "proj" | "plot" | "style" | "variant" => 1,
            _ => 0,
        };
        if marker_len == 0 || index + marker_len >= parts.len() {
            continue;
        }
        if parts[index].eq_ignore_ascii_case("projection")
            && parts
                .get(index + 1)
                .is_some_and(|part| part.eq_ignore_ascii_case("variant"))
            && index + 2 < parts.len()
        {
            let candidate = parts[index + 2..].join("_");
            if let Some(variant) = normalized_static_plot_variant(&candidate) {
                return (parts[..index].to_vec(), Some(variant));
            }
        }
        if parts[index].eq_ignore_ascii_case("plot")
            && parts
                .get(index + 1)
                .is_some_and(|part| part.eq_ignore_ascii_case("variant"))
            && index + 2 < parts.len()
        {
            let candidate = parts[index + 2..].join("_");
            if let Some(variant) = normalized_static_plot_variant(&candidate) {
                return (parts[..index].to_vec(), Some(variant));
            }
        }
        let candidate = parts[index + marker_len..].join("_");
        if let Some(variant) = normalized_static_plot_variant(&candidate) {
            return (parts[..index].to_vec(), Some(variant));
        }
    }

    for index in (0..parts.len()).rev() {
        let candidate = parts[index..].join("_");
        if let Some(variant) = normalized_known_static_plot_variant(&candidate) {
            return (parts[..index].to_vec(), Some(variant));
        }
    }

    (parts.to_vec(), None)
}

fn infer_static_plot_variant_from_path(path: Option<&Path>) -> Option<String> {
    let path = path?;
    for component in path.components().rev().take(8) {
        let text = component.as_os_str().to_string_lossy();
        if let Some(variant) = infer_static_plot_variant_from_text(&text) {
            return Some(variant);
        }
    }
    None
}

fn infer_static_plot_variant_from_text(text: &str) -> Option<String> {
    let mut parts = text
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    if parts
        .last()
        .is_some_and(|part| part.eq_ignore_ascii_case("json") || part.eq_ignore_ascii_case("png"))
    {
        parts.pop();
    }
    if parts.len() >= 2
        && parts[parts.len() - 2].eq_ignore_ascii_case("run")
        && parts[parts.len() - 1].eq_ignore_ascii_case("manifest")
    {
        parts.truncate(parts.len() - 2);
    } else if parts
        .last()
        .is_some_and(|part| part.eq_ignore_ascii_case("manifest"))
    {
        parts.pop();
    }
    let (_, variant) = split_static_plot_domain_variant_parts(&parts);
    variant
}

fn static_plot_model_slug(slug: &str) -> String {
    match slug {
        "ecmwf_open_data" => "ecmwf-open-data".to_string(),
        value => value.replace('_', "-"),
    }
}

fn resolve_static_artifact_path(manifest: &StaticPlotRunManifest, artifact_path: &Path) -> PathBuf {
    if artifact_path.is_absolute() {
        artifact_path.to_path_buf()
    } else {
        manifest.output_root.join(artifact_path)
    }
}

fn normalized_state(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn stable_static_plot_id(path: &Path) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in path.display().to_string().as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}")
}

fn no_store_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, no-cache, must-revalidate, max-age=0"),
    );
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers
}

async fn index() -> impl IntoResponse {
    (no_store_headers(), Html(INDEX_HTML))
}

async fn plots() -> impl IntoResponse {
    (no_store_headers(), Html(PLOTS_HTML))
}

async fn satellite_viewer() -> impl IntoResponse {
    (no_store_headers(), Html(SATELLITE_HTML))
}

async fn radar_viewer() -> impl IntoResponse {
    (no_store_headers(), Html(RADAR_HTML))
}

async fn weather_tools() -> impl IntoResponse {
    (no_store_headers(), Html(WEATHER_TOOLS_HTML))
}

async fn plot_lab() -> impl IntoResponse {
    (no_store_headers(), Html(PLOT_LAB_HTML))
}

async fn projection_demo() -> impl IntoResponse {
    (no_store_headers(), Html(PROJECTION_DEMO_HTML))
}

async fn archive_viewer() -> impl IntoResponse {
    (no_store_headers(), Html(ARCHIVE_HTML))
}

async fn ops() -> impl IntoResponse {
    (no_store_headers(), Html(OPS_HTML))
}

const ARCHIVE_HTML: &str = r####"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>HRRR Archive</title>
  <link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css" />
  <style>
    * { box-sizing:border-box; }
    body { margin:0; font-family:Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; color:#111827; background:#f6f8fb; }
    header { display:flex; align-items:center; justify-content:space-between; gap:12px; padding:10px 14px; border-bottom:1px solid #d0d5dd; background:#fff; }
    h1 { margin:0; font-size:18px; }
    .head-actions { display:flex; align-items:center; gap:8px; }
    .mobile-toggle { display:none; }
    main { display:grid; grid-template-columns:350px minmax(0,1fr); height:calc(100vh - 55px); min-height:640px; }
    aside { border-right:1px solid #d0d5dd; background:#fff; overflow:auto; }
    .toolbar { display:flex; gap:8px; padding:10px; border-bottom:1px solid #e2e8f0; }
    button, select { min-height:34px; border:1px solid #111827; border-radius:6px; padding:0 10px; font:inherit; font-size:13px; font-weight:800; background:#111827; color:#fff; }
    button.secondary { background:#fff; color:#111827; border-color:#cbd5e1; }
    button:disabled { opacity:.55; cursor:default; }
    select { background:#fff; color:#111827; border-color:#cbd5e1; min-width:0; }
    label { display:grid; gap:4px; color:#475467; font-size:10px; font-weight:900; text-transform:uppercase; }
    .event { width:100%; display:grid; gap:4px; padding:10px 12px; border:0; border-bottom:1px solid #e5e7eb; border-radius:0; text-align:left; background:#fff; color:#111827; cursor:pointer; }
    .event:hover, .event.active { background:#eef6ff; }
    .event strong { font-size:15px; }
    .event span { color:#475467; font-size:12px; font-weight:700; }
    .stage { position:relative; min-width:0; min-height:0; }
    #map { width:100%; height:100%; background:#e5e7eb; }
    .panel { position:absolute; z-index:900; left:12px; top:12px; width:min(520px, calc(100% - 24px)); display:grid; gap:8px; padding:10px; border-radius:8px; background:rgba(255,255,255,.96); box-shadow:0 8px 24px rgba(0,0,0,.18); }
    .summary { color:#334155; font-size:13px; line-height:1.35; }
    .controls { display:grid; grid-template-columns:1.25fr 1.25fr .7fr; gap:8px; }
    .buttons { display:grid; grid-template-columns:repeat(4,minmax(0,1fr)); gap:6px; }
    .runbar { display:flex; flex-wrap:wrap; gap:6px; }
    .pill { display:inline-flex; align-items:center; min-height:24px; border:1px solid #cbd5e1; border-radius:999px; padding:0 8px; color:#334155; background:#f8fafc; font-size:12px; font-weight:900; }
    .pill.ready { color:#14532d; background:#dcfce7; border-color:#86efac; }
    .output { position:absolute; z-index:910; right:12px; top:12px; width:430px; max-width:calc(100% - 24px); max-height:calc(100% - 24px); overflow:auto; border-radius:8px; background:rgba(255,255,255,.96); box-shadow:0 8px 24px rgba(0,0,0,.18); display:none; }
    .output.show { display:block; }
    .output-head { display:flex; justify-content:space-between; gap:8px; align-items:center; padding:8px 10px; border-bottom:1px solid #e2e8f0; font-weight:900; }
    .output-body { padding:8px; display:grid; gap:8px; color:#334155; font-size:12px; }
    .output img { width:100%; height:auto; display:block; border:1px solid #e2e8f0; border-radius:6px; background:#fff; }
    .static-sections { display:grid; gap:12px; }
    .static-section { display:grid; gap:7px; }
    .static-filter { display:flex; flex-wrap:wrap; gap:6px; }
    .static-filter button.active { background:#111827; color:#fff; border-color:#111827; }
    .static-section-head { display:flex; align-items:center; justify-content:space-between; gap:8px; color:#111827; font-size:12px; font-weight:900; text-transform:uppercase; }
    .static-section-head span { color:#64748b; font-size:11px; }
    .static-domain-head { margin-top:2px; color:#334155; font-size:11px; font-weight:900; text-transform:uppercase; }
    .static-grid { display:grid; grid-template-columns:repeat(2,minmax(0,1fr)); gap:8px; }
    .static-card { display:grid; gap:4px; color:#334155; font-size:11px; font-weight:800; }
    .static-card img { aspect-ratio:16/9; object-fit:cover; }
    .static-card a { color:#2563eb; text-decoration:none; }
    .static-card-actions { display:flex; gap:8px; flex-wrap:wrap; font-size:11px; }
    .status { position:absolute; z-index:880; left:12px; bottom:12px; max-width:min(760px, calc(100% - 24px)); padding:8px 10px; border-radius:7px; background:rgba(17,24,39,.86); color:#e5e7eb; font-size:12px; line-height:1.35; }
    a { color:#2563eb; font-weight:900; }
    @media (max-width:980px) {
      html, body { height:100%; overflow:hidden; }
      header { position:relative; z-index:1300; min-height:55px; padding:8px 10px; }
      h1 { font-size:16px; }
      .head-actions .pill { display:none; }
      .mobile-toggle { display:inline-grid; place-items:center; min-height:34px; padding:0 9px; }
      main { display:block; height:calc(100dvh - 55px); min-height:0; }
      aside {
        display:none;
        position:fixed;
        z-index:1250;
        top:62px;
        left:10px;
        right:10px;
        max-height:min(68dvh, 520px);
        overflow:auto;
        border:1px solid #cbd5e1;
        border-radius:8px;
        box-shadow:0 14px 32px rgba(15,23,42,.28);
      }
      body.events-open aside { display:block; }
      .stage { height:100%; min-height:0; }
      .panel {
        display:none;
        position:absolute;
        left:10px;
        right:10px;
        top:10px;
        width:auto;
        max-height:calc(100% - 72px);
        overflow:auto;
      }
      body.tools-open .panel { display:grid; }
      .controls { grid-template-columns:1fr; }
      .buttons { grid-template-columns:repeat(2,minmax(0,1fr)); }
      .output { position:absolute; left:10px; right:10px; width:auto; top:auto; bottom:10px; max-height:min(74dvh, calc(100% - 28px)); }
      .status { left:10px; right:10px; bottom:10px; max-width:none; }
      .static-grid { grid-template-columns:1fr; }
    }
  </style>
</head>
<body>
  <header>
    <h1>HRRR Severe Archive</h1>
    <div class="head-actions">
      <button id="eventsToggle" class="secondary mobile-toggle" type="button">Events</button>
      <button id="toolsToggle" class="secondary mobile-toggle" type="button">Tools</button>
      <span class="pill">archive only</span>
    </div>
  </header>
  <main>
    <aside>
      <div class="toolbar">
        <select id="rank"><option value="high">HIGH</option><option value="mdt-plus">MDT+</option></select>
        <button id="refresh">Refresh</button>
      </div>
      <div id="events"></div>
    </aside>
    <section class="stage">
      <div id="map"></div>
      <div class="panel">
        <div class="summary" id="summary">Loading archive...</div>
        <div class="runbar" id="runs"></div>
        <div class="controls">
          <label>Product<select id="product"></select></label>
          <label>Run<select id="run"></select></label>
          <label>Hour<select id="hour"></select></label>
        </div>
        <div class="buttons">
          <button id="loadTile">Map Tile</button>
          <button id="loadStatic" class="secondary">Plots</button>
          <button id="loadSounding" class="secondary">Sounding</button>
          <button id="renderCross" class="secondary">Cross</button>
        </div>
        <div class="buttons">
          <button id="setA" class="secondary">Set A</button>
          <button id="setB" class="secondary">Set B</button>
          <button id="clearTile" class="secondary">Clear</button>
          <button id="hideOutput" class="secondary">Hide</button>
        </div>
      </div>
      <div class="output" id="output">
        <div class="output-head"><span id="outputTitle">Archive output</span><button id="closeOutput" class="secondary">Close</button></div>
        <div class="output-body" id="outputBody"></div>
      </div>
      <div class="status" id="status">SPC outlook polygons load first. Archive tiles and soundings appear as event processing completes.</div>
    </section>
  </main>
  <script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js"></script>
  <script>
    const $ = id => document.getElementById(id);
    const els = {
      events:$("events"), rank:$("rank"), refresh:$("refresh"), summary:$("summary"), runs:$("runs"),
      product:$("product"), run:$("run"), hour:$("hour"), loadTile:$("loadTile"), loadStatic:$("loadStatic"),
      loadSounding:$("loadSounding"), renderCross:$("renderCross"), setA:$("setA"), setB:$("setB"),
      clearTile:$("clearTile"), status:$("status"), output:$("output"), outputTitle:$("outputTitle"),
      outputBody:$("outputBody"), closeOutput:$("closeOutput"), hideOutput:$("hideOutput"),
      eventsToggle:$("eventsToggle"), toolsToggle:$("toolsToggle")
    };
    let map, base, polygons, archiveTile, selectedEvent, selectedPoint, pointMarker, pointA, pointB, pathLayer;
    let eventBody = null;
    let variables = {};
    let staticPlotCards = [];
    let staticPlotGroupLimits = {};
    let staticPlotDomainFilter = "conus";
    const STATIC_INITIAL_LIMIT = 4;
    const STATIC_DOMAIN_ORDER = ["conus", "midwest", "southeast", "southern_plains"];
    const STATIC_GROUP_ORDER = ["severe", "surface", "precip", "clouds", "upper_air", "other"];
    const preferred = ["stp_fixed","sbcape","mlcape","mucape","srh_0_1km","srh_0_3km","bulk_shear_0_6km","2m_dewpoint","2m_temperature","composite_reflectivity"];
    function defaultsFor(name) {
      const lower = String(name || "").toLowerCase();
      if (lower.includes("stp") || lower.includes("scp") || lower.includes("ehi")) return ["magma", "0", "5"];
      if (lower.includes("cape")) return ["magma", "0", "5000"];
      if (lower.includes("cin")) return ["magma", "-250", "0"];
      if (lower.includes("srh")) return ["magma", "-150", "500"];
      if (lower.includes("shear") || lower.includes("wind")) return ["wind", "0", "80"];
      if (lower.includes("rh") || lower.includes("humidity") || lower.includes("cloud")) return ["humidity", "0", "100"];
      if (lower.includes("qpf") || lower.includes("precip")) return ["magma", "0", "75"];
      if (lower.includes("reflectivity")) return ["magma", "0", "75"];
      if (lower.includes("temp") || lower.includes("dewpoint") || lower.includes("wetbulb")) return ["temperature", "-30", "35"];
      return ["temperature", "0", "1"];
    }
    function esc(v) { return String(v ?? "").replace(/[&<>"']/g, ch => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[ch])); }
    function setStatus(text) { els.status.textContent = text; }
    function showOutput(title, html) { els.outputTitle.textContent = title; els.outputBody.innerHTML = html; els.output.classList.add("show"); }
    function hideOutput() { els.output.classList.remove("show"); }
    function isMobileLayout() { return window.matchMedia("(max-width: 980px)").matches; }
    function closeMobileDrawer(name) {
      if (!isMobileLayout()) return;
      document.body.classList.remove(name);
    }
    async function fetchJson(url, options) {
      const res = await fetch(url, options);
      if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
      return res.json();
    }
    function ensureMap() {
      if (map) return;
      map = L.map("map", { preferCanvas:true, zoomControl:true }).setView([38, -97], 4);
      base = L.tileLayer("https://tile.openstreetmap.org/{z}/{x}/{y}.png", { maxZoom:12, attribution:"&copy; OpenStreetMap" }).addTo(map);
      map.on("click", event => setSelectedPoint(event.latlng));
    }
    async function loadEvents() {
      ensureMap();
      const body = await fetchJson(`/v1/hrrrarchive/events?rank=${encodeURIComponent(els.rank.value)}`);
      const events = body.events || [];
      els.events.innerHTML = events.map(ev => {
        const ready = (ev.runs || []).some(run => run.pressure_store_complete);
        return `<button class="event ${ev.event_id === selectedEvent ? "active" : ""}" data-id="${esc(ev.event_id)}"><strong>${esc(ev.convective_day)}</strong><span>${esc(ev.max_outlook)} | peak ${esc(ev.peak_iso)} | ${esc(ev.tornado_count)} tor | ${ready ? "store ready" : "planned"}</span></button>`;
      }).join("");
      for (const button of els.events.querySelectorAll("button")) button.onclick = () => {
        closeMobileDrawer("events-open");
        loadEvent(button.dataset.id);
      };
      if (!selectedEvent && events.length) {
        const ready = events.find(ev => (ev.runs || []).some(run => run.pressure_store_complete));
        loadEvent((ready || events[events.length - 1]).event_id);
      }
    }
    async function loadEvent(id) {
      selectedEvent = id;
      eventBody = await fetchJson(`/v1/hrrrarchive/events/${encodeURIComponent(id)}`);
      const poly = await fetchJson(`/v1/hrrrarchive/events/${encodeURIComponent(id)}/polygons`);
      for (const button of els.events.querySelectorAll("button")) button.classList.toggle("active", button.dataset.id === id);
      drawPolygons(poly);
      populateRuns(eventBody.runs || []);
      els.summary.innerHTML = `<strong>${esc(eventBody.convective_day)}</strong> ${esc(eventBody.max_outlook)} | peak ${esc(eventBody.peak_iso)} | EF${esc(eventBody.max_ef)} max | ${esc(eventBody.tornado_count)} tornadoes`;
      await loadArchiveVariables();
      setStatus(`${eventBody.convective_day}: SPC outlook and WxStore coverage loaded. Click the map for sounding/cross-section points.`);
    }
    function populateRuns(runs) {
      els.run.innerHTML = runs.map(run => `<option value="${esc(run.run_id)}">${esc(run.kind)} ${esc(run.init_iso)}${run.pressure_store_complete ? " ready" : " planned"}</option>`).join("");
      const ready = runs.find(run => run.pressure_store_complete) || runs[0];
      if (ready) els.run.value = ready.run_id;
      els.runs.innerHTML = runs.map(run => `<button class="secondary" data-run="${esc(run.run_id)}">${esc(run.kind)} f${esc(run.peak_fhour)} ${run.pressure_store_complete ? "ready" : "planned"}</button>`).join("");
      for (const button of els.runs.querySelectorAll("button")) button.onclick = () => { els.run.value = button.dataset.run; loadArchiveVariables(); };
    }
    function selectedRun() {
      return (eventBody?.runs || []).find(run => run.run_id === els.run.value) || (eventBody?.runs || [])[0] || {};
    }
    async function loadArchiveVariables() {
      const run = els.run.value;
      variables = {};
      els.product.innerHTML = `<option value="">processing...</option>`;
      els.hour.innerHTML = `<option value="">--</option>`;
      try {
        const data = await fetchJson(`/v1/variables?model=hrrr_archive&run=${encodeURIComponent(run)}`);
        variables = data.available_hours || {};
        const names = Object.keys(variables).sort();
        const ordered = preferred.filter(name => names.includes(name)).concat(names.filter(name => !preferred.includes(name)));
        els.product.innerHTML = ordered.map(name => `<option value="${esc(name)}">${esc(name.replaceAll("_"," "))}</option>`).join("");
        if (ordered.length) els.product.value = ordered[0];
        populateHours();
      } catch (err) {
        const runInfo = selectedRun();
        const hours = runInfo.pressure_fhours || runInfo.plot_fhours || [];
        els.product.innerHTML = `<option value="">archive tiles pending</option>`;
        els.hour.innerHTML = hours.map(hour => `<option value="${hour}">f${String(hour).padStart(3,"0")}</option>`).join("");
        if (runInfo.peak_fhour != null) els.hour.value = String(runInfo.peak_fhour);
        setStatus(`Archive tiles pending for ${run}. Polygons, pressure soundings, and static plots may still be available.`);
      }
    }
    function populateHours() {
      const hours = variables[els.product.value] || [];
      els.hour.innerHTML = hours.map(hour => `<option value="${hour}">f${String(hour).padStart(3,"0")}</option>`).join("");
      const runInfo = selectedRun();
      if (runInfo.peak_fhour != null && hours.map(String).includes(String(runInfo.peak_fhour))) els.hour.value = String(runInfo.peak_fhour);
    }
    function drawPolygons(poly) {
      ensureMap();
      if (polygons) polygons.remove();
      polygons = L.geoJSON(poly, {
        style: feature => {
          const layer = feature.properties && feature.properties.layer;
          if (layer === "volume_mask") return { color:"#0891b2", weight:3, opacity:.95, fill:false, dashArray:"8 5" };
          return { color:"#991b1b", weight:3, opacity:.95, fillColor:"#ef4444", fillOpacity:.16 };
        },
        onEachFeature: (feature, layer) => {
          const kind = feature.properties && feature.properties.layer === "volume_mask" ? "WxStore coverage" : "SPC outlook";
          layer.bindTooltip(kind, { sticky:true });
        }
      }).addTo(map);
      const bounds = polygons.getBounds();
      if (bounds.isValid()) map.fitBounds(bounds.pad(.12));
      setTimeout(() => map.invalidateSize(), 50);
    }
    async function loadTile() {
      if (!els.product.value) { setStatus("No archive tile product is available for this run yet."); return; }
      const run = els.run.value;
      const product = els.product.value;
      const hour = els.hour.value;
      const [palette, min, max] = defaultsFor(product);
      setStatus(`Loading archive tile ${product} f${String(hour).padStart(3,"0")}...`);
      const params = new URLSearchParams({ hours:String(hour), palette, range:`${min},${max}`, base_url:location.origin });
      const url = `/v1/mapbox/layers/hrrr_archive/${encodeURIComponent(run)}/${encodeURIComponent(product)}?${params.toString()}`;
      const data = await fetchJson(url);
      const frame = data.frames && data.frames[0];
      if (!frame) throw new Error("archive layer returned no tile frame");
      if (archiveTile) map.removeLayer(archiveTile);
      archiveTile = L.tileLayer(frame.tiles[0], { opacity:.72, maxZoom:data.maxzoom || 9 }).addTo(map);
      if (data.bounds) map.fitBounds([[data.bounds[1], data.bounds[0]], [data.bounds[3], data.bounds[2]]], { padding:[18,18] });
      setStatus(`${eventBody.convective_day} ${product} f${String(hour).padStart(3,"0")} loaded from hrrr_archive.`);
    }
    function setSelectedPoint(latlng) {
      selectedPoint = { lat:latlng.lat, lng:latlng.lng };
      if (pointMarker) pointMarker.remove();
      pointMarker = L.circleMarker([selectedPoint.lat, selectedPoint.lng], { radius:6, color:"#111827", weight:2, fillColor:"#facc15", fillOpacity:.95 }).addTo(map);
      setStatus(`Selected ${selectedPoint.lat.toFixed(4)}, ${selectedPoint.lng.toFixed(4)} for archive sounding/cross-section.`);
    }
    function setRoutePoint(which) {
      if (!selectedPoint) { setStatus("Click the map first."); return; }
      if (which === "A") pointA = {...selectedPoint}; else pointB = {...selectedPoint};
      if (pathLayer) pathLayer.remove();
      const items = [];
      if (pointA) items.push(L.circleMarker([pointA.lat, pointA.lng], { radius:6, color:"#0f766e", weight:3, fillColor:"#ccfbf1", fillOpacity:.95 }).bindTooltip("A", { permanent:true }));
      if (pointB) items.push(L.circleMarker([pointB.lat, pointB.lng], { radius:6, color:"#be123c", weight:3, fillColor:"#ffe4e6", fillOpacity:.95 }).bindTooltip("B", { permanent:true }));
      if (pointA && pointB) items.push(L.polyline([[pointA.lat, pointA.lng], [pointB.lat, pointB.lng]], { color:"#facc15", weight:3, opacity:.95, dashArray:"8 6" }));
      pathLayer = L.layerGroup(items).addTo(map);
      setStatus(`Set ${which}: ${selectedPoint.lat.toFixed(4)}, ${selectedPoint.lng.toFixed(4)}`);
    }
    async function loadSounding() {
      if (!selectedPoint) { setStatus("Click the map first."); return; }
      const runInfo = selectedRun();
      setStatus("Rendering archive sounding...");
      const report = await fetchJson("/v1/sounding/render", {
        method:"POST",
        headers:{"Content-Type":"application/json"},
        body:JSON.stringify({ model:"hrrr_archive", run:els.run.value, hour:Number(els.hour.value || runInfo.peak_fhour || 0), lat:selectedPoint.lat, lon:selectedPoint.lng, source:"aws", sample_method:"inverse-distance4", crop_radius_deg:1.25 })
      });
      const url = report.png_url || report.output?.png_url;
      showOutput("Archive sounding", `<a href="${esc(url)}" target="_blank" rel="noopener"><img src="${esc(url)}" alt="sounding"></a><div>${esc(report.resolved_run)} f${String(report.request?.forecast_hour ?? els.hour.value).padStart(3,"0")} | ${esc(report.profile?.levels || "--")} levels | ${esc(report.server_elapsed_ms || report.timing?.total_ms || "--")} ms</div>`);
      setStatus("Archive sounding rendered.");
    }
    async function renderCross() {
      if (!pointA || !pointB) { setStatus("Set A and B first."); return; }
      const runInfo = selectedRun();
      setStatus("Rendering archive cross section...");
      const report = await fetchJson("/v1/cross-section/render", {
        method:"POST",
        headers:{"Content-Type":"application/json"},
        body:JSON.stringify({ model:"hrrr_archive", run:els.run.value, hour:Number(els.hour.value || runInfo.peak_fhour || 0), start_lat:pointA.lat, start_lon:pointA.lng, end_lat:pointB.lat, end_lon:pointB.lng, product:"wind_speed", width:1400, height:820 })
      });
      const first = (report.outputs || [])[0];
      const url = first && (first.webp_url || first.png_url);
      showOutput("Archive cross section", `<a href="${esc(url)}" target="_blank" rel="noopener"><img src="${esc(url)}" alt="cross section"></a><div>${esc(report.resolved_run || els.run.value)} f${String(report.hour ?? els.hour.value).padStart(3,"0")} | ${esc(report.product || "wind_speed")}</div>`);
      setStatus("Archive cross section rendered.");
    }
    function artifactProductKey(artifact) {
      const key = String(artifact?.artifact_key || "");
      if (key.includes(":")) return key.split(":").pop();
      const path = String(artifact?.relative_path || artifact?.path || "");
      const match = path.match(/_f\d{3}_[^_]+_(.+)\.(png|webp)$/);
      return match ? match[1] : key;
    }
    function staticPlotPriority(item) {
      const key = artifactProductKey(item.artifact);
      const selected = els.product.value;
      if (selected && key === selected) return -100;
      const preferredIndex = preferred.indexOf(key);
      if (preferredIndex >= 0) return preferredIndex;
      const lower = key.toLowerCase();
      if (/cape|cin|stp|srh|shear|helicity|lapse|ehi|supercell/.test(lower)) return 20;
      if (/2m|10m|surface|mslp|dewpoint|relative_humidity|apparent|gust|temperature|wetbulb/.test(lower)) return 30;
      if (/qpf|precip|rain|snow|ice|reflectivity/.test(lower)) return 40;
      if (/cloud|visibility|fog|satellite/.test(lower)) return 50;
      if (/850mb|700mb|500mb|300mb|250mb|200mb|height_winds|vorticity/.test(lower)) return 70;
      return 60;
    }
    function staticPlotCategory(item) {
      const lower = artifactProductKey(item.artifact).toLowerCase();
      if (/cape|cin|stp|srh|shear|helicity|lapse|ehi|supercell|uh_/.test(lower)) return "severe";
      if (/2m|10m|surface|mslp|dewpoint|relative_humidity|apparent|gust|temperature|wetbulb/.test(lower)) return "surface";
      if (/qpf|precip|rain|snow|ice|reflectivity/.test(lower)) return "precip";
      if (/cloud|visibility|fog|satellite/.test(lower)) return "clouds";
      if (/850mb|700mb|500mb|300mb|250mb|200mb|height_winds|vorticity/.test(lower)) return "upper_air";
      return "other";
    }
    function staticPlotCategoryLabel(group) {
      return {
        severe:"Severe",
        surface:"Surface",
        precip:"Precip / Radar",
        clouds:"Clouds / Visibility",
        upper_air:"Upper Air",
        other:"Other"
      }[group] || group.replaceAll("_", " ");
    }
    function staticPlotCardHtml(item) {
      const key = artifactProductKey(item.artifact);
      const url = item.artifact.url;
      return `<div class="static-card"><a href="${esc(url)}" target="_blank" rel="noopener"><img loading="lazy" decoding="async" src="${esc(url)}" alt="${esc(key)}"></a><span>${esc(item.manifest.domain)} ${esc(key)}</span><div class="static-card-actions"><a href="${esc(url)}" target="_blank" rel="noopener">Full size</a><a href="${esc(url)}" download>Download</a></div></div>`;
    }
    function renderStaticPlotGroups(hour) {
      const domains = STATIC_DOMAIN_ORDER;
      const groups = new Map();
      for (const item of staticPlotCards) {
        const group = staticPlotCategory(item);
        if (!groups.has(group)) groups.set(group, []);
        groups.get(group).push(item);
      }
      const filterHtml = `<div class="static-filter">${domains.map(domain => `<button class="secondary ${staticPlotDomainFilter === domain ? "active" : ""}" type="button" data-static-domain="${esc(domain)}">${esc(domain.replaceAll("_", " "))}</button>`).join("")}</div>`;
      const sections = STATIC_GROUP_ORDER
        .filter(group => groups.has(group))
        .map(group => {
          const items = groups.get(group);
          const limit = staticPlotGroupLimits[group] || STATIC_INITIAL_LIMIT;
          const shown = items.slice(0, limit);
          const more = items.length > shown.length
            ? `<button class="secondary" type="button" data-static-more="${esc(group)}">Show ${Math.min(5, items.length - shown.length)} more</button>`
            : "";
          return `<div class="static-section"><div class="static-section-head">${esc(staticPlotCategoryLabel(group))}<span>${shown.length}/${items.length}</span></div><div class="static-grid">${shown.map(staticPlotCardHtml).join("")}</div>${more}</div>`;
        });
      const body = sections.length
        ? `<div class="static-sections">${sections.join("")}</div><div>${esc(staticPlotDomainFilter.replaceAll("_", " "))} plots for f${String(hour).padStart(3,"0")}. Pick another region above to fetch that region only.</div>`
        : `<div>No ${esc(staticPlotDomainFilter.replaceAll("_", " "))} static plots are published for f${String(hour).padStart(3,"0")} yet.</div>`;
      showOutput("Archive static plots", `${filterHtml}${body}`);
      els.outputBody.querySelectorAll("[data-static-domain]").forEach(button => {
        button.onclick = () => {
          loadStaticPlots(button.dataset.staticDomain).catch(err => setStatus(err.message));
        };
      });
      els.outputBody.querySelectorAll("[data-static-more]").forEach(button => {
        button.onclick = () => {
          const key = button.dataset.staticMore;
          staticPlotGroupLimits[key] = (staticPlotGroupLimits[key] || STATIC_INITIAL_LIMIT) + 5;
          renderStaticPlotGroups(hour);
        };
      });
    }
    async function loadStaticPlots() {
      const runInfo = selectedRun();
      const hour = Number(els.hour.value || runInfo.peak_fhour || 0);
      const domain = arguments[0] || staticPlotDomainFilter || "conus";
      staticPlotDomainFilter = domain;
      setStatus(`Loading ${domain.replaceAll("_", " ")} static plots for f${String(hour).padStart(3,"0")}...`);
      const params = new URLSearchParams({ model:"hrrr", date:runInfo.date_yyyymmdd || "", cycle_utc:String(runInfo.cycle_utc ?? 6), forecast_hour:String(hour), domain, include_artifacts:"true", manifest_limit:"40", artifact_limit:"1000" });
      const data = await fetchJson(`/v1/static-plots?${params.toString()}`);
      const cards = (data.manifests || [])
        .flatMap(manifest => (manifest.artifacts || [])
          .filter(a => a.exists === true || (a.state === "complete" && a.exists !== false))
          .map(a => ({ manifest, artifact:a })))
        .sort((a, b) => staticPlotPriority(a) - staticPlotPriority(b)
          || String(a.manifest.domain || "").localeCompare(String(b.manifest.domain || ""))
          || artifactProductKey(a.artifact).localeCompare(artifactProductKey(b.artifact)));
      staticPlotCards = cards;
      staticPlotGroupLimits = {};
      renderStaticPlotGroups(hour);
      setStatus(`Loaded ${cards.length} ${domain.replaceAll("_", " ")} plot artifacts for f${String(hour).padStart(3,"0")}.`);
    }
    els.refresh.onclick = loadEvents;
    els.rank.onchange = () => { selectedEvent = null; loadEvents().catch(err => setStatus(err.message)); };
    els.run.onchange = loadArchiveVariables;
    els.product.onchange = populateHours;
    els.loadTile.onclick = () => loadTile().catch(err => setStatus(err.message));
    els.clearTile.onclick = () => { if (archiveTile) map.removeLayer(archiveTile); archiveTile = null; setStatus("Archive tile cleared."); };
    els.loadStatic.onclick = () => loadStaticPlots().catch(err => setStatus(err.message));
    els.loadSounding.onclick = () => loadSounding().catch(err => setStatus(err.message));
    els.setA.onclick = () => setRoutePoint("A");
    els.setB.onclick = () => setRoutePoint("B");
    els.renderCross.onclick = () => renderCross().catch(err => setStatus(err.message));
    els.closeOutput.onclick = hideOutput;
    els.hideOutput.onclick = hideOutput;
    els.eventsToggle.onclick = () => {
      document.body.classList.toggle("events-open");
      document.body.classList.remove("tools-open");
      setTimeout(() => map && map.invalidateSize(), 60);
    };
    els.toolsToggle.onclick = () => {
      document.body.classList.toggle("tools-open");
      document.body.classList.remove("events-open");
      setTimeout(() => map && map.invalidateSize(), 60);
    };
    window.addEventListener("resize", () => {
      if (!isMobileLayout()) {
        document.body.classList.remove("events-open", "tools-open");
      }
      setTimeout(() => map && map.invalidateSize(), 60);
    });
    loadEvents().catch(err => setStatus(err.message));
  </script>
</body>
</html>"####;

const WEATHER_TOOLS_HTML: &str = r####"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>WxStore Tools</title>
  <style>
    * { box-sizing: border-box; }
    body { margin:0; font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; color:#111827; background:#f6f8fb; }
    header { position:sticky; top:0; z-index:5; display:flex; align-items:center; justify-content:space-between; gap:12px; padding:10px 14px; border-bottom:1px solid #d0d5dd; background:rgba(246,248,251,.96); }
    h1 { margin:0; font-size:18px; }
    nav { display:flex; gap:8px; }
    a, button { min-height:34px; border:1px solid #111827; border-radius:6px; background:#111827; color:#fff; padding:0 10px; font:inherit; font-size:13px; font-weight:800; text-decoration:none; cursor:pointer; }
    a.secondary, button.secondary { background:#fff; color:#111827; border-color:#cbd5e1; }
    button:disabled { opacity:.55; cursor:default; }
    main { display:grid; grid-template-columns:minmax(320px,420px) minmax(0,1fr); min-height:calc(100vh - 56px); }
    aside { display:grid; align-content:start; gap:10px; padding:12px; border-right:1px solid #d0d5dd; background:#fff; max-height:calc(100vh - 56px); overflow:auto; }
    section { display:grid; gap:8px; padding:10px; border:1px solid #e2e8f0; border-radius:8px; }
    h2 { margin:0; font-size:13px; text-transform:uppercase; color:#475467; }
    label { display:grid; gap:5px; min-width:0; color:#475467; font-size:11px; font-weight:900; text-transform:uppercase; }
    input, select { min-height:34px; border:1px solid #cbd5e1; border-radius:6px; padding:0 8px; color:#111827; background:#fff; font:inherit; font-size:13px; text-transform:none; }
    .row { display:grid; grid-template-columns:1fr 1fr; gap:8px; }
    .stage { display:grid; grid-template-rows:minmax(320px,1fr) auto; gap:10px; padding:12px; min-width:0; }
    .panel { border:1px solid #d0d5dd; border-radius:8px; background:#fff; min-width:0; overflow:hidden; }
    svg { width:100%; height:100%; min-height:320px; display:block; background:#fff; }
    .status { min-height:42px; border:1px solid #d0d5dd; border-radius:8px; background:#fff; padding:10px; font-size:13px; color:#334155; white-space:pre-wrap; }
    img { width:100%; height:auto; display:block; background:#fff; }
  </style>
</head>
<body>
  <header>
    <h1>WxStore Tools</h1>
    <nav>
      <a class="secondary" href="/satellite">Satellite</a>
      <a class="secondary" href="/plots">Plots</a>
      <a class="secondary" href="/ops">Ops</a>
    </nav>
  </header>
  <main>
    <aside>
      <section>
        <h2>Meteogram</h2>
        <div class="row">
          <label>Model<select id="model"></select></label>
          <label>Run<select id="run"></select></label>
        </div>
        <div class="row">
          <label>Lat<input id="lat" value="35.0" /></label>
          <label>Lon<input id="lon" value="-97.0" /></label>
        </div>
        <label>Hours<input id="hours" value="0-18" /></label>
        <label>Variables<select id="vars" multiple size="8"></select></label>
        <button id="loadMeteogram" type="button">Load Meteogram</button>
      </section>
      <section>
        <h2>Cross Section</h2>
        <div class="row">
          <label>Start Lat<input id="startLat" value="34.05" /></label>
          <label>Start Lon<input id="startLon" value="-118.25" /></label>
        </div>
        <div class="row">
          <label>End Lat<input id="endLat" value="39.32" /></label>
          <label>End Lon<input id="endLon" value="-120.18" /></label>
        </div>
        <div class="row">
          <label>Hour<input id="crossHour" value="0" /></label>
          <label>Product<select id="crossProduct"></select></label>
        </div>
        <button id="loadCross" type="button">Render Cross Section</button>
      </section>
    </aside>
    <div class="stage">
      <div class="panel" id="output"><svg id="chart" viewBox="0 0 1200 620"></svg></div>
      <div class="status" id="status">Loading...</div>
    </div>
  </main>
  <script>
    const els = {
      model: document.getElementById("model"),
      run: document.getElementById("run"),
      lat: document.getElementById("lat"),
      lon: document.getElementById("lon"),
      hours: document.getElementById("hours"),
      vars: document.getElementById("vars"),
      loadMeteogram: document.getElementById("loadMeteogram"),
      startLat: document.getElementById("startLat"),
      startLon: document.getElementById("startLon"),
      endLat: document.getElementById("endLat"),
      endLon: document.getElementById("endLon"),
      crossHour: document.getElementById("crossHour"),
      crossProduct: document.getElementById("crossProduct"),
      loadCross: document.getElementById("loadCross"),
      output: document.getElementById("output"),
      chart: document.getElementById("chart"),
      status: document.getElementById("status"),
    };
    const preferred = ["temperature_2m", "dew_point_2m", "relative_humidity_2m", "wind_gusts_10m", "wind_speed_10m", "qpf_total", "composite_reflectivity"];
    const colors = ["#dc2626", "#2563eb", "#16a34a", "#9333ea", "#ea580c", "#0891b2", "#4f46e5"];
    let models = [];
    function setStatus(text) { els.status.textContent = text; }
    async function fetchJson(url, options) {
      const res = await fetch(url, options);
      if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
      return res.json();
    }
    function modelRuns(model) {
      return (models.find(item => item.id === model)?.runs || []).slice().sort().reverse();
    }
    async function loadCatalog() {
      const data = await fetchJson("/v1/models");
      models = data.spatial_loaded?.models || [];
      els.model.innerHTML = models.map(item => `<option value="${item.id}">${item.id}</option>`).join("");
      if (models.some(item => item.id === "hrrr")) els.model.value = "hrrr";
      populateRuns();
      await populateVariables();
      const products = await fetchJson("/v1/cross-section/products");
      els.crossProduct.innerHTML = (products.products || []).map(item => `<option value="${item.product}">${item.label}</option>`).join("");
      const cs = await fetchJson("/v1/cross-section/status");
      setStatus(`WxStore: ${models.map(item => `${item.id}:${item.latest_run}`).join(" | ")}\nCross sections: ${cs.status}`);
    }
    function populateRuns() {
      const runs = modelRuns(els.model.value);
      els.run.innerHTML = [`<option value="latest">latest</option>`].concat(runs.map(run => `<option value="${run}">${run}</option>`)).join("");
    }
    async function populateVariables() {
      const data = await fetchJson(`/v1/variables?model=${encodeURIComponent(els.model.value)}&run=${encodeURIComponent(els.run.value)}`);
      const vars = (data.variables || []).map(item => item.name || item.id || item.variable || item).filter(Boolean);
      const ordered = preferred.filter(v => vars.includes(v)).concat(vars.filter(v => !preferred.includes(v))).slice(0, 80);
      els.vars.innerHTML = ordered.map(v => `<option value="${v}" ${preferred.slice(0,4).includes(v) ? "selected" : ""}>${v}</option>`).join("");
    }
    function selectedVars() {
      return Array.from(els.vars.selectedOptions).map(option => option.value).slice(0, 8);
    }
    function drawForecast(data, vars) {
      const svg = els.chart;
      const W = 1200, H = 620, L = 74, R = 28, T = 34, B = 82;
      const hours = data.hourly?.time || [];
      const series = vars.map(v => (data.hourly?.[v] || []).map(x => x == null ? NaN : Number(x)));
      const values = series.flat().filter(Number.isFinite);
      if (!hours.length || !values.length) {
        svg.innerHTML = `<text x="40" y="60" font-size="22" fill="#64748b">No data</text>`;
        return;
      }
      const min = Math.min(...values), max = Math.max(...values);
      const span = Math.max(1e-6, max - min);
      const x = i => L + (W - L - R) * (i / Math.max(1, hours.length - 1));
      const y = v => T + (H - T - B) * (1 - (v - min) / span);
      let html = `<rect x="0" y="0" width="${W}" height="${H}" fill="#fff"/>`;
      for (let g=0; g<=5; g++) {
        const yy = T + (H - T - B) * g / 5;
        const val = max - span * g / 5;
        html += `<line x1="${L}" y1="${yy}" x2="${W-R}" y2="${yy}" stroke="#e2e8f0"/><text x="12" y="${yy+5}" font-size="16" fill="#475467">${val.toFixed(1)}</text>`;
      }
      vars.forEach((v, idx) => {
        const pts = series[idx].map((val, i) => Number.isFinite(val) ? `${x(i).toFixed(1)},${y(val).toFixed(1)}` : "").filter(Boolean).join(" ");
        html += `<polyline points="${pts}" fill="none" stroke="${colors[idx % colors.length]}" stroke-width="4" stroke-linejoin="round" stroke-linecap="round"/>`;
        html += `<text x="${L + idx * 170}" y="${H-28}" font-size="17" font-weight="700" fill="${colors[idx % colors.length]}">${v}</text>`;
      });
      html += `<line x1="${L}" y1="${T}" x2="${L}" y2="${H-B}" stroke="#111827"/><line x1="${L}" y1="${H-B}" x2="${W-R}" y2="${H-B}" stroke="#111827"/>`;
      svg.innerHTML = html;
    }
    async function loadMeteogram() {
      els.loadMeteogram.disabled = true;
      try {
        const vars = selectedVars();
        const params = new URLSearchParams({
          model: els.model.value,
          run: els.run.value,
          lat: els.lat.value,
          lon: els.lon.value,
          hours: els.hours.value,
          hourly: vars.join(","),
        });
        const data = await fetchJson(`/v1/forecast?${params.toString()}`);
        els.output.innerHTML = `<svg id="chart" viewBox="0 0 1200 620"></svg>`;
        els.chart = document.getElementById("chart");
        drawForecast(data, vars);
        setStatus(`${data.model} ${data.run} ${vars.length} variable(s) in ${Number(data.generationtime_ms || 0).toFixed(1)} ms`);
      } catch (err) {
        setStatus(err.message);
      } finally {
        els.loadMeteogram.disabled = false;
      }
    }
    async function loadCross() {
      els.loadCross.disabled = true;
      try {
        const report = await fetchJson("/v1/cross-section/render", {
          method: "POST",
          headers: {"Content-Type": "application/json"},
          body: JSON.stringify({
            model: "hrrr",
            run: "latest",
            start_lat: Number(els.startLat.value),
            start_lon: Number(els.startLon.value),
            end_lat: Number(els.endLat.value),
            end_lon: Number(els.endLon.value),
            hour: Number(els.crossHour.value),
            product: els.crossProduct.value,
            width: 1400,
            height: 820,
          }),
        });
        const first = (report.outputs || [])[0];
        if (first?.webp_url || first?.png_url) {
          els.output.innerHTML = `<img src="${first.webp_url || first.png_url}" alt="cross section" />`;
        }
        setStatus(`Cross section ${report.cache_hit ? "cached" : "rendered"}: ${report.rendered_count || 0} output(s), ${report.total_ms || report.server_elapsed_ms || "--"} ms`);
      } catch (err) {
        setStatus(err.message);
      } finally {
        els.loadCross.disabled = false;
      }
    }
    els.model.addEventListener("change", async () => { populateRuns(); await populateVariables(); });
    els.run.addEventListener("change", populateVariables);
    els.loadMeteogram.addEventListener("click", loadMeteogram);
    els.loadCross.addEventListener("click", loadCross);
    loadCatalog().then(loadMeteogram).catch(err => setStatus(err.message));
  </script>
</body>
</html>"####;

const PLOT_LAB_HTML: &str = r####"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>WxStore Plot Lab</title>
  <style>
    :root { color-scheme: light; --ink:#111827; --muted:#64748b; --line:#d0d5dd; --panel:#fff; --bg:#f6f8fb; --accent:#1d4ed8; }
    * { box-sizing: border-box; }
    body { margin:0; font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; background:var(--bg); color:var(--ink); }
    header { position:sticky; top:0; z-index:10; display:grid; grid-template-columns:minmax(180px,1fr) auto; gap:12px; align-items:center; padding:10px 14px; border-bottom:1px solid var(--line); background:rgba(246,248,251,.97); }
    h1 { margin:0; font-size:18px; line-height:1.2; }
    nav { display:flex; gap:8px; }
    nav a, button { display:inline-grid; place-items:center; min-height:34px; border:1px solid #111827; border-radius:6px; background:#111827; color:#fff; padding:0 10px; font:inherit; font-size:13px; font-weight:800; text-decoration:none; cursor:pointer; }
    button.secondary, nav a.secondary { background:#fff; color:#111827; border-color:#cbd5e1; }
    button:disabled { opacity:.55; cursor:default; }
    main { display:grid; grid-template-columns:360px minmax(0,1fr); min-height:calc(100vh - 56px); }
    aside { display:grid; align-content:start; gap:10px; padding:12px; border-right:1px solid var(--line); background:#fff; overflow:auto; max-height:calc(100vh - 56px); }
    fieldset { display:grid; gap:8px; margin:0; padding:10px; border:1px solid #e2e8f0; border-radius:8px; }
    legend { padding:0 4px; color:#475467; font-size:11px; font-weight:900; text-transform:uppercase; }
    label { display:grid; gap:5px; min-width:0; color:#475467; font-size:11px; font-weight:900; text-transform:uppercase; }
    select, input { width:100%; min-height:34px; border:1px solid #cbd5e1; border-radius:6px; background:#fff; color:#111827; padding:0 8px; font:inherit; font-size:13px; text-transform:none; }
    input[type="checkbox"] { width:auto; min-height:auto; }
    .row { display:grid; grid-template-columns:1fr 1fr; gap:8px; }
    .row3 { display:grid; grid-template-columns:1fr 1fr 1fr; gap:8px; }
    .check { display:flex; align-items:center; gap:8px; min-height:32px; color:#111827; font-size:13px; font-weight:800; text-transform:none; }
    .actions { display:grid; grid-template-columns:1fr 1fr; gap:8px; }
    .status { color:#475467; font-size:12px; line-height:1.35; overflow-wrap:anywhere; }
    .stage { display:grid; grid-template-rows:auto minmax(0,1fr) auto; min-width:0; min-height:0; }
    .bar { display:grid; grid-template-columns:minmax(0,1fr) auto; gap:10px; align-items:center; padding:10px 12px; border-bottom:1px solid var(--line); background:#fff; }
    .meta { display:flex; flex-wrap:wrap; gap:8px; color:#475467; font-size:12px; font-weight:800; }
    .meta span { padding:5px 7px; border:1px solid #e2e8f0; border-radius:6px; background:#f8fafc; }
    .preview { position:relative; display:grid; place-items:center; min-width:0; min-height:0; padding:14px; background:#111827; }
    .preview img { display:block; max-width:100%; max-height:calc(100vh - 188px); width:auto; height:auto; object-fit:contain; background:#f8fafc; box-shadow:0 14px 34px rgba(0,0,0,.28); }
    .empty { width:min(620px,92vw); padding:18px; border:1px dashed #64748b; border-radius:8px; background:rgba(15,23,42,.72); color:#e5e7eb; text-align:center; }
    .history { display:flex; gap:8px; overflow-x:auto; padding:10px 12px; border-top:1px solid var(--line); background:#fff; }
    .thumb { display:grid; gap:4px; width:150px; min-width:150px; padding:6px; border:1px solid #e2e8f0; border-radius:8px; background:#f8fafc; cursor:pointer; }
    .thumb.active { outline:3px solid var(--accent); outline-offset:1px; }
    .thumb img { width:100%; height:82px; object-fit:cover; background:#fff; border:1px solid #e2e8f0; }
    .thumb span { font-size:11px; color:#475467; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
    pre { margin:0; max-height:150px; overflow:auto; padding:8px; border-top:1px solid var(--line); background:#0f172a; color:#dbeafe; font-size:12px; line-height:1.35; white-space:pre-wrap; }
    @media (max-width:1050px) { main { grid-template-columns:1fr; } aside { max-height:none; border-right:0; border-bottom:1px solid var(--line); } .preview img { max-height:64vh; } }
  </style>
</head>
<body>
  <header>
    <h1>WxStore Plot Lab</h1>
    <nav>
      <a class="secondary" href="/">Map</a>
      <a class="secondary" href="/plots">Plots</a>
      <a class="secondary" href="/ops">Ops</a>
    </nav>
  </header>
  <main>
    <aside>
      <fieldset>
        <legend>Data</legend>
        <div class="row">
          <label>Model<select id="model"></select></label>
          <label>Source<select id="source"></select></label>
        </div>
        <div class="row3">
          <label>Date<input id="date" value="20260504" /></label>
          <label>Cycle<input id="cycle" type="number" min="0" max="23" value="12" /></label>
          <label>Hour<input id="hour" type="number" min="0" max="840" value="0" /></label>
        </div>
        <label>Product<input id="product" list="products" value="500mb_height_winds" /><datalist id="products"></datalist></label>
      </fieldset>
      <fieldset>
        <legend>Domain</legend>
        <label>Region<select id="region"></select></label>
        <label class="check"><input id="customBounds" type="checkbox" /> Custom bounds</label>
        <div class="row">
          <label>West<input id="west" type="number" step="0.001" /></label>
          <label>East<input id="east" type="number" step="0.001" /></label>
        </div>
        <div class="row">
          <label>South<input id="south" type="number" step="0.001" /></label>
          <label>North<input id="north" type="number" step="0.001" /></label>
        </div>
      </fieldset>
      <fieldset>
        <legend>Projection</legend>
        <div class="row">
          <label>Projection<select id="projection"></select></label>
          <label>Style<select id="style"></select></label>
        </div>
        <div class="row">
          <label>Width<input id="width" type="number" min="640" max="4096" value="1600" /></label>
          <label>Height<input id="height" type="number" min="480" max="4096" value="900" /></label>
        </div>
        <div class="row">
          <label>Supersample<input id="supersample" type="number" min="1" max="4" value="2" /></label>
          <label>Chrome<input id="chrome" type="number" step="0.05" min="0.6" max="1.6" value="0.9" /></label>
        </div>
        <label>Frame pad<input id="pad" type="number" step="0.005" min="0" max="0.25" value="0.06" /></label>
      </fieldset>
      <fieldset>
        <legend>Linework</legend>
        <div class="row">
          <label>Line boost<input id="lineBoost" type="number" min="0" max="4" value="0" /></label>
          <label>Line alpha<input id="lineAlpha" type="number" step="0.05" min="0.25" max="2" value="1" /></label>
        </div>
        <div class="row3">
          <label>Barb width<input id="barbWidth" type="number" min="1" max="8" value="2" /></label>
          <label>Barb length<input id="barbLength" type="number" min="6" max="48" value="20" /></label>
          <label>Barb density<input id="barbDensity" type="number" step="0.1" min="0.25" max="4" value="1" /></label>
        </div>
      </fieldset>
      <fieldset>
        <legend>Raster</legend>
        <div class="row">
          <label>Crop pad cells<input id="cropPad" type="number" min="0" max="5000" value="1000" /></label>
          <label>Fill levels<input id="fillMult" type="number" min="1" max="8" value="1" /></label>
        </div>
        <label class="check"><input id="geoClip" type="checkbox" checked /> Geographic clip</label>
        <label class="check"><input id="graticule" type="checkbox" checked /> Graticule</label>
      </fieldset>
      <div class="actions">
        <button id="render" type="button">Render</button>
        <button id="reset" type="button" class="secondary">Reset View</button>
      </div>
      <div class="status" id="status">Loading config...</div>
    </aside>
    <section class="stage">
      <div class="bar">
        <div class="meta" id="meta"></div>
        <a id="open" class="secondary" href="" target="_blank" rel="noopener" hidden>Open</a>
      </div>
      <div class="preview">
        <img id="image" alt="" hidden />
        <div id="empty" class="empty">No render yet.</div>
      </div>
      <div class="history" id="history"></div>
      <pre id="log" hidden></pre>
    </section>
  </main>
  <script>
    const $ = id => document.getElementById(id);
    const els = {
      model: $("model"), source: $("source"), date: $("date"), cycle: $("cycle"), hour: $("hour"),
      product: $("product"), products: $("products"), region: $("region"), customBounds: $("customBounds"),
      west: $("west"), east: $("east"), south: $("south"), north: $("north"), projection: $("projection"),
      style: $("style"), width: $("width"), height: $("height"), supersample: $("supersample"),
      chrome: $("chrome"), pad: $("pad"), lineBoost: $("lineBoost"), lineAlpha: $("lineAlpha"),
      barbWidth: $("barbWidth"), barbLength: $("barbLength"), barbDensity: $("barbDensity"),
      cropPad: $("cropPad"), fillMult: $("fillMult"), geoClip: $("geoClip"), graticule: $("graticule"),
      render: $("render"), reset: $("reset"), status: $("status"), image: $("image"), empty: $("empty"),
      meta: $("meta"), open: $("open"), history: $("history"), log: $("log")
    };
    let config = null;
    let history = [];

    function option(value, label = value) { return `<option value="${value}">${label}</option>`; }
    function selectedRegion() { return config.regions.find(item => item.slug === els.region.value) || config.regions[0]; }
    function applyRegionBounds(force = false) {
      if (!force && els.customBounds.checked) return;
      const b = selectedRegion().bounds;
      els.west.value = b.west; els.east.value = b.east; els.south.value = b.south; els.north.value = b.north;
    }
    function number(id, fallback) {
      const value = Number(els[id].value);
      return Number.isFinite(value) ? value : fallback;
    }
    function requestBody() {
      const body = {
        model: els.model.value,
        source: els.source.value,
        date: els.date.value.trim(),
        cycle_utc: number("cycle", 0),
        forecast_hour: number("hour", 0),
        region: els.region.value,
        product: els.product.value.trim(),
        projection_variant: els.projection.value,
        plot_style: els.style.value,
        output_width: number("width", 1600),
        output_height: number("height", 900),
        supersample_factor: number("supersample", 2),
        chrome_scale: number("chrome", 0.9),
        presentation_pad_fraction: number("pad", 0.06),
        inverse_raster_crop_pad_cells: number("cropPad", 1000),
        inverse_raster_geo_clip: els.geoClip.checked,
        basemap_graticule: els.graticule.checked,
        native_fill_level_multiplier: number("fillMult", 1),
        place_label_density: 0,
        linework_width_boost: number("lineBoost", 0),
        linework_alpha_scale: number("lineAlpha", 1),
        barb_width: number("barbWidth", 2),
        barb_length_px: number("barbLength", 20),
        barb_density: number("barbDensity", 1)
      };
      if (els.customBounds.checked) {
        const base = selectedRegion().slug.replaceAll("-", "_");
        body.domain_slug = `${base}_lab`;
        body.bounds = {
          west: number("west", -127),
          east: number("east", -66),
          south: number("south", 23),
          north: number("north", 51.5)
        };
      }
      return body;
    }
    function renderHistory() {
      els.history.innerHTML = history.map((item, index) => `
        <button class="thumb ${index === 0 ? "active" : ""}" data-index="${index}" type="button">
          <img src="${item.artifact.url}" alt="" />
          <span>${item.request.region} ${item.request.product}</span>
          <span>${item.elapsed_ms} ms</span>
        </button>`).join("");
    }
    function showResult(result) {
      els.empty.hidden = true;
      els.image.hidden = false;
      els.image.src = result.artifact.url;
      els.open.hidden = false;
      els.open.href = result.artifact.url;
      els.meta.innerHTML = [
        `${result.request.model} ${result.request.date} ${result.request.cycle_utc}z f${String(result.request.forecast_hour).padStart(3, "0")}`,
        result.request.region,
        result.request.product,
        `${result.request.output_width}x${result.request.output_height}`,
        `${result.elapsed_ms} ms`
      ].map(text => `<span>${text}</span>`).join("");
      els.log.hidden = false;
      els.log.textContent = [result.stdout, result.stderr].filter(Boolean).join("\n");
      history.unshift(result);
      history = history.slice(0, 18);
      renderHistory();
    }
    async function render() {
      els.render.disabled = true;
      els.status.textContent = "Rendering real data on the node...";
      try {
        const res = await fetch("/v1/plot-lab/render", {
          method: "POST",
          headers: { "content-type": "application/json" },
          body: JSON.stringify(requestBody())
        });
        const data = await res.json();
        if (!res.ok) throw new Error(data.message || data.reason || `render failed: ${res.status}`);
        showResult(data);
        els.status.textContent = "Render complete.";
      } catch (err) {
        els.status.textContent = err.message;
      } finally {
        els.render.disabled = false;
      }
    }
    async function init() {
      const res = await fetch("/v1/plot-lab/config", { cache: "no-store" });
      config = await res.json();
      els.model.innerHTML = config.models.map(item => option(item)).join("");
      els.model.value = "gfs";
      els.source.innerHTML = config.sources.map(item => option(item)).join("");
      els.source.value = "nomads";
      els.region.innerHTML = config.regions.map(item => option(item.slug, item.label)).join("");
      els.region.value = "global";
      els.products.innerHTML = config.products.map(item => option(item)).join("");
      els.projection.innerHTML = config.projection_variants.map(item => option(item)).join("");
      els.style.innerHTML = config.plot_styles.map(item => option(item)).join("");
      applyRegionBounds(true);
      els.status.textContent = "Ready.";
    }
    els.region.addEventListener("change", () => applyRegionBounds(false));
    els.customBounds.addEventListener("change", () => applyRegionBounds(false));
    els.render.addEventListener("click", render);
    els.reset.addEventListener("click", () => applyRegionBounds(true));
    els.history.addEventListener("click", event => {
      const button = event.target.closest("[data-index]");
      if (!button) return;
      const item = history[Number(button.dataset.index)];
      if (item) showResult({...item, stdout: item.stdout || "", stderr: item.stderr || ""});
    });
    init().catch(err => { els.status.textContent = err.message; });
  </script>
</body>
</html>"####;

const PROJECTION_DEMO_HTML: &str = r###"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Projection Style Demo</title>
  <style>
    :root {
      color-scheme: dark;
      --bg: #101418;
      --panel: #171d24;
      --panel-2: #202833;
      --line: #344050;
      --text: #eef3f8;
      --muted: #aab5c2;
      --accent: #4ea1ff;
      --bad: #f87171;
    }
    * { box-sizing: border-box; }
    html, body { margin: 0; min-height: 100%; }
    body {
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: var(--bg);
      color: var(--text);
    }
    header {
      position: sticky;
      top: 0;
      z-index: 10;
      display: grid;
      gap: 10px;
      padding: 12px;
      border-bottom: 1px solid var(--line);
      background: rgba(16, 20, 24, 0.96);
      backdrop-filter: blur(12px);
    }
    .topline {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
    }
    h1 { margin: 0; font-size: 18px; letter-spacing: 0; }
    nav { display: flex; gap: 8px; align-items: center; }
    a, button {
      color: var(--text);
      background: var(--panel-2);
      border: 1px solid var(--line);
      border-radius: 7px;
      height: 34px;
      padding: 0 10px;
      font: inherit;
      font-weight: 750;
      text-decoration: none;
      cursor: pointer;
    }
    button.primary { background: #1c5f9f; border-color: #2a78c3; }
    .controls {
      display: grid;
      grid-template-columns: repeat(7, minmax(110px, 1fr));
      gap: 8px;
      align-items: end;
    }
    label {
      display: grid;
      gap: 4px;
      min-width: 0;
      color: var(--muted);
      font-size: 11px;
      font-weight: 800;
      text-transform: uppercase;
    }
    select {
      width: 100%;
      height: 34px;
      border-radius: 7px;
      border: 1px solid var(--line);
      background: #0f151d;
      color: var(--text);
      padding: 0 8px;
      font: inherit;
      font-size: 13px;
      text-transform: none;
    }
    main { padding: 12px; }
    .status {
      margin-bottom: 12px;
      color: var(--muted);
      font-size: 13px;
      line-height: 1.4;
    }
    .grid {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(420px, 1fr));
      gap: 12px;
      align-items: start;
    }
    .tile {
      overflow: hidden;
      border: 1px solid var(--line);
      border-radius: 8px;
      background: var(--panel);
    }
    .tile-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      padding: 8px 10px;
      border-bottom: 1px solid var(--line);
      background: var(--panel-2);
    }
    .variant {
      min-width: 0;
      font-size: 14px;
      font-weight: 850;
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .meta {
      color: var(--muted);
      font-size: 12px;
      white-space: nowrap;
    }
    .image-wrap {
      display: grid;
      place-items: center;
      min-height: 280px;
      background: #0a0e13;
    }
    img {
      display: block;
      width: 100%;
      height: auto;
      background: #0a0e13;
    }
    .missing {
      padding: 28px;
      color: var(--bad);
      font-size: 13px;
      text-align: center;
    }
    .empty {
      border: 1px solid var(--line);
      border-radius: 8px;
      padding: 18px;
      color: var(--muted);
      background: var(--panel);
    }
    @media (max-width: 980px) {
      .controls { grid-template-columns: repeat(2, minmax(0, 1fr)); }
      .grid { grid-template-columns: 1fr; }
      .topline { align-items: flex-start; flex-direction: column; }
    }
  </style>
</head>
<body>
  <header>
    <div class="topline">
      <h1>Projection / Plot Style Demo</h1>
      <nav>
        <a href="/plots">Plots</a>
        <a href="/ops">Ops</a>
        <button id="refresh" class="primary">Refresh</button>
      </nav>
    </div>
    <div class="controls">
      <label>Model<select id="model"></select></label>
      <label>Run<select id="run"></select></label>
      <label>Domain<select id="domain"></select></label>
      <label>Member<select id="member"></select></label>
      <label>Hour<select id="hour"></select></label>
      <label>Product<select id="product"></select></label>
      <label>Variant Filter<select id="variant"></select></label>
    </div>
  </header>
  <main>
    <div id="status" class="status">Loading catalog...</div>
    <section id="grid" class="grid"></section>
  </main>
  <script>
    const els = {
      model: document.getElementById("model"),
      run: document.getElementById("run"),
      domain: document.getElementById("domain"),
      member: document.getElementById("member"),
      hour: document.getElementById("hour"),
      product: document.getElementById("product"),
      variant: document.getElementById("variant"),
      refresh: document.getElementById("refresh"),
      status: document.getElementById("status"),
      grid: document.getElementById("grid"),
    };
    let catalog = [];
    let series = [];
    let lastSeriesKey = "";

    function setStatus(text) { els.status.textContent = text; }
    function opt(select, value, label = value) {
      const node = document.createElement("option");
      node.value = value;
      node.textContent = label;
      select.appendChild(node);
    }
    function setOptions(select, values, previous, labels = new Map()) {
      select.textContent = "";
      values.forEach(value => opt(select, value, labels.get(value) || value));
      if (values.includes(previous)) select.value = previous;
      else if (values.length) select.value = values[0];
    }
    function modelOf(item) { return item.model || "unknown"; }
    function runKey(item) { return `${item.date || "unknown"}_${String(item.cycle_utc ?? 0).padStart(2, "0")}z`; }
    function runLabel(key) { return key.replace("_", " "); }
    function memberOf(item) { return item.ensemble_key || item.member || "control"; }
    function variantOf(item) { return item.projection_variant || item.plot_variant || item.variant || "auto"; }
    function productLabel(key) {
      return String(key || "")
        .replace(/^(direct|derived|windowed|ensemble|animation|animation_webp):/, "")
        .replaceAll("_", " ");
    }
    function productKey(artifact) { return artifact?.artifact_key || ""; }
    function stripProductPrefix(key) {
      return String(key || "").replace(/^(direct|derived|windowed|ensemble|animation|animation_webp):/, "");
    }
    function normalizedProductKey(artifact) {
      return stripProductPrefix(productKey(artifact));
    }
    function artifactAvailable(artifact) { return artifact && artifact.exists !== false && artifact.url; }
    function isProjectionGalleryManifest(item) {
      return String(item.manifest_path || "").includes("experiments/projection_gallery/")
        || String(item.output_root || "").includes("/experiments/projection_gallery/");
    }
    function unique(values) { return Array.from(new Set(values.filter(Boolean))).sort(); }
    function selectedRunParts() {
      const [date, cycleText] = els.run.value.split("_");
      return { date, cycle: parseInt(cycleText, 10) || 0 };
    }
    function selectPreferred(select, preferred) {
      for (const item of preferred) {
        const found = Array.from(select.options).find(option => option.value === item);
        if (found) {
          select.value = item;
          return;
        }
      }
    }
    function filteredCatalog() {
      const parts = selectedRunParts();
      return catalog.filter(item =>
        modelOf(item) === els.model.value &&
        item.date === parts.date &&
        Number(item.cycle_utc) === Number(parts.cycle)
      );
    }
    function populateModels(preserve = true) {
      const previous = preserve ? els.model.value : "";
      const models = unique(catalog.map(modelOf));
      setOptions(els.model, models, previous);
      selectPreferred(els.model, ["gfs", "gefs", "hrrr", "rap", "ecmwf-open-data"]);
    }
    function populateRuns(preserve = true) {
      const previous = preserve ? els.run.value : "";
      const labels = new Map();
      const runs = unique(catalog.filter(item => modelOf(item) === els.model.value).map(item => {
        const key = runKey(item);
        labels.set(key, runLabel(key));
        return key;
      })).sort().reverse();
      setOptions(els.run, runs, previous, labels);
    }
    function populateDomains(preserve = true) {
      const previous = preserve ? els.domain.value : "";
      const domains = unique(filteredCatalog().map(item => item.domain));
      setOptions(els.domain, domains, previous);
      selectPreferred(els.domain, ["conus", "global"]);
    }
    function populateMembers(preserve = true) {
      const previous = preserve ? els.member.value : "";
      const members = unique(filteredCatalog().filter(item => item.domain === els.domain.value).map(memberOf));
      setOptions(els.member, members, previous);
    }
    function populateHours(preserve = true) {
      const previous = preserve ? els.hour.value : "";
      const hours = unique(filteredCatalog()
        .filter(item => item.domain === els.domain.value && memberOf(item) === els.member.value)
        .map(item => String(item.forecast_hour ?? 0).padStart(3, "0")));
      setOptions(els.hour, hours, previous);
    }
    function populateVariants(preserve = true) {
      const previous = preserve ? els.variant.value : "";
      const variants = ["all", ...unique(series.map(variantOf))];
      setOptions(els.variant, variants, previous);
    }
    function populateProducts(preserve = true) {
      const previous = preserve ? els.product.value : "";
      const products = unique(series.flatMap(item => (item.artifacts || []).map(productKey)));
      const labels = new Map(products.map(key => [key, productLabel(key)]));
      setOptions(els.product, products, previous, labels);
      selectPreferred(els.product, [
        "direct:500mb_height_winds",
        "500mb_height_winds",
        "direct:2m_temperature",
        "direct:500mb_height_winds",
        "direct:mslp_10m_winds",
        "derived:2m_temperature",
      ]);
    }
    async function fetchCatalog(preserve = true) {
      setStatus("Loading plot manifest catalog...");
      const url = `/v1/static-plots?include_artifacts=false&state=all&manifest_limit=20000&_=${Date.now()}`;
      const res = await fetch(url, { cache: "no-store" });
      if (!res.ok) throw new Error(`catalog ${res.status}`);
      const data = await res.json();
      catalog = data.manifests || [];
      populateModels(preserve);
      populateRuns(preserve);
      populateDomains(preserve);
      populateMembers(preserve);
      populateHours(preserve);
    }
    async function fetchSeries(preserveProduct = true) {
      if (!els.model.value || !els.run.value || !els.domain.value || !els.member.value || !els.hour.value) return;
      const parts = selectedRunParts();
      const hour = String(parseInt(els.hour.value, 10));
      const key = [els.model.value, parts.date, parts.cycle, els.domain.value, els.member.value, hour].join("|");
      setStatus("Loading images for selected run/domain/hour...");
      const params = new URLSearchParams({
        include_artifacts: "true",
        state: "all",
        model: els.model.value,
        date: parts.date,
        cycle_utc: String(parts.cycle),
        domain: els.domain.value,
        ensemble: els.member.value,
        forecast_hour: hour,
        artifact_limit: "1000",
        manifest_limit: "20000",
        _: String(Date.now()),
      });
      const res = await fetch(`/v1/static-plots?${params.toString()}`, { cache: "no-store" });
      if (!res.ok) throw new Error(`series ${res.status}`);
      const data = await res.json();
      series = data.manifests || [];
      lastSeriesKey = key;
      populateProducts(preserveProduct);
      populateVariants(true);
      renderGrid();
    }
    function renderGrid() {
      const product = els.product.value;
      const variantFilter = els.variant.value || "all";
      const candidates = series.filter(item => variantFilter === "all" || variantOf(item) === variantFilter);
      const galleryCandidates = candidates.filter(isProjectionGalleryManifest);
      const sourceRows = galleryCandidates.length >= 2 ? galleryCandidates : candidates;
      const rows = sourceRows
        .map(item => {
          const normalizedProduct = stripProductPrefix(product);
          const artifact = (item.artifacts || []).find(candidate => normalizedProductKey(candidate) === normalizedProduct);
          return { item, artifact };
        })
        .sort((a, b) => variantOf(a.item).localeCompare(variantOf(b.item)));
      els.grid.textContent = "";
      if (!rows.length) {
        els.grid.innerHTML = `<div class="empty">No matching manifests for this selection yet.</div>`;
        setStatus("No matching manifests. Try another model/run/domain/hour.");
        return;
      }
      for (const row of rows) {
        const card = document.createElement("article");
        card.className = "tile";
        const variant = variantOf(row.item);
        const state = row.artifact?.state || row.item.state || "unknown";
        const manifest = row.item.manifest_path || row.item.run_label || "";
        card.innerHTML = `
          <div class="tile-head">
            <div class="variant">${variant}</div>
            <div class="meta">f${String(row.item.forecast_hour ?? 0).padStart(3, "0")} | ${state}</div>
          </div>
          <div class="image-wrap">
            ${artifactAvailable(row.artifact)
              ? `<img src="${row.artifact.url}" alt="${variant} ${productLabel(product)}" loading="lazy" />`
              : `<div class="missing">No image for ${productLabel(product)}<br>${manifest}</div>`}
          </div>`;
        els.grid.appendChild(card);
      }
      const visible = rows.filter(row => artifactAvailable(row.artifact)).length;
      setStatus(`${visible}/${rows.length} variants have an image for ${productLabel(product)}. ${els.model.value} ${runLabel(els.run.value)} ${els.domain.value} ${els.member.value} f${els.hour.value}.`);
    }
    async function reload(preserve = true) {
      try {
        await fetchCatalog(preserve);
        await fetchSeries(preserve);
      } catch (err) {
        console.error(err);
        setStatus(`Error: ${err.message}`);
      }
    }
    function hook(select, fn) {
      select.addEventListener("change", async () => {
        try { await fn(); } catch (err) { setStatus(`Error: ${err.message}`); }
      });
    }
    hook(els.model, async () => {
      populateRuns(false); populateDomains(false); populateMembers(false); populateHours(false); await fetchSeries(false);
    });
    hook(els.run, async () => {
      populateDomains(false); populateMembers(false); populateHours(false); await fetchSeries(false);
    });
    hook(els.domain, async () => {
      populateMembers(false); populateHours(false); await fetchSeries(false);
    });
    hook(els.member, async () => {
      populateHours(false); await fetchSeries(false);
    });
    hook(els.hour, async () => { await fetchSeries(true); });
    hook(els.product, async () => { renderGrid(); });
    hook(els.variant, async () => { renderGrid(); });
    els.refresh.addEventListener("click", () => reload(true));
    reload(false);
  </script>
</body>
</html>"###;

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
      grid-template-columns: 130px 118px 118px 145px 145px minmax(180px, 260px) 76px 92px 74px 74px 72px;
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
    .nav-button {
      display: inline-grid;
      place-items: center;
      height: 32px;
      border-radius: 6px;
      background: #334155;
      color: #fff;
      font-size: 13px;
      font-weight: 800;
      text-decoration: none;
    }
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
    .layer-stack-panel {
      position: absolute;
      z-index: 1000;
      left: 12px;
      top: 78px;
      width: 390px;
      max-width: calc(100vw - 24px);
      padding: 10px;
      border-radius: 8px;
      background: rgba(255,255,255,0.94);
      box-shadow: 0 8px 24px rgba(0,0,0,0.22);
      color: #111827;
      font-size: 12px;
    }
    .layer-stack-head {
      display: grid;
      grid-template-columns: 1fr auto auto;
      gap: 6px;
      align-items: center;
      margin-bottom: 8px;
    }
    .layer-stack-title { font-weight: 850; color: #111827; }
    .layer-stack-head button { height: 30px; font-size: 12px; padding: 0 8px; }
    .layer-stack {
      display: grid;
      gap: 6px;
      max-height: 190px;
      overflow: auto;
    }
    .layer-row {
      display: grid;
      grid-template-columns: 1fr 82px 28px;
      gap: 6px;
      align-items: center;
      padding: 7px 8px;
      border: 1px solid #dbe3ee;
      border-radius: 6px;
      background: #f8fafc;
    }
    .layer-row.primary { border-color: #2563eb; box-shadow: inset 3px 0 0 #2563eb; }
    .layer-row-title { font-weight: 850; color: #0f172a; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .layer-row-meta { margin-top: 2px; color: #64748b; font-size: 11px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
    .layer-row input { width: 82px; padding: 0; }
    .layer-row button {
      width: 28px;
      height: 28px;
      padding: 0;
      border-radius: 6px;
      background: #e2e8f0;
      color: #0f172a;
      border-color: #cbd5e1;
    }
    .layer-empty { color: #64748b; line-height: 1.35; padding: 4px 0; }
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
    .analysis-panel {
      position: absolute;
      z-index: 1000;
      right: 12px;
      top: 244px;
      width: 440px;
      max-height: min(640px, calc(100vh - 330px));
      overflow: auto;
      padding: 10px;
      border-radius: 8px;
      background: rgba(255,255,255,0.95);
      box-shadow: 0 8px 24px rgba(0,0,0,0.22);
      color: #111827;
      font-size: 12px;
    }
    .analysis-head {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      margin-bottom: 8px;
    }
    .analysis-title { font-weight: 850; color: #111827; }
    .analysis-head a { color: #2563eb; font-size: 11px; font-weight: 800; text-decoration: none; }
    .analysis-point, .analysis-path, .analysis-message {
      padding: 7px 8px;
      border: 1px solid #e2e8f0;
      border-radius: 6px;
      background: #f8fafc;
      color: #475569;
      line-height: 1.35;
      overflow-wrap: anywhere;
    }
    .analysis-path { display: grid; grid-template-columns: 1fr 1fr; gap: 6px; margin: 8px 0; }
    .analysis-path strong { color: #111827; }
    .analysis-buttons { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 6px; margin: 8px 0; }
    .analysis-buttons button { height: 30px; font-size: 12px; padding: 0 6px; }
    .analysis-label { margin-top: 8px; }
    .analysis-output { margin-top: 8px; min-height: 54px; }
    .analysis-output svg, .analysis-output img { display: block; width: 100%; border: 1px solid #e2e8f0; border-radius: 6px; background: #fff; }
    .analysis-output img { height: auto; }
    .analysis-meta { margin-top: 6px; color: #64748b; font-size: 11px; line-height: 1.35; }
    .analysis-output a { color: #2563eb; font-weight: 800; text-decoration: none; }
    .meteogram-card {
      overflow: hidden;
      border: 1px solid #cbd5e1;
      border-radius: 8px;
      background: #ffffff;
      box-shadow: inset 0 1px 0 rgba(255,255,255,.85);
    }
    .meteogram-hero {
      display: grid;
      gap: 6px;
      padding: 10px 12px;
      background: linear-gradient(135deg, #0f172a, #1e3a8a 60%, #0369a1);
      color: #fff;
    }
    .meteogram-kicker { font-size: 10px; font-weight: 900; letter-spacing: .08em; text-transform: uppercase; color: #bae6fd; }
    .meteogram-title { font-size: 17px; font-weight: 900; line-height: 1.15; }
    .meteogram-subtitle { color: #dbeafe; font-size: 11px; line-height: 1.3; overflow-wrap: anywhere; }
    .meteogram-stats { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); gap: 6px; padding: 8px; background: #f8fafc; }
    .meteogram-stat { display: grid; gap: 2px; min-width: 0; padding: 7px; border: 1px solid #e2e8f0; border-radius: 6px; background: #fff; }
    .meteogram-stat span { color: #64748b; font-size: 9px; font-weight: 900; text-transform: uppercase; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .meteogram-stat strong { color: #0f172a; font-size: 16px; line-height: 1.1; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
    .meteogram-chart { padding: 8px; }
    .meteogram-chart svg { border: 0; border-radius: 0; }
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
      .layer-stack-panel { left: 12px; right: 12px; top: 156px; width: auto; }
      .picker-panel { left: 12px; right: 12px; top: auto; bottom: 292px; width: auto; }
      .analysis-panel { left: 12px; right: 12px; top: auto; bottom: 432px; width: auto; max-height: 240px; }
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
  <div id="analysisPanel" class="analysis-panel">
    <div class="analysis-head">
      <div class="analysis-title">Map Tools</div>
      <a href="/tools">full tools</a>
    </div>
    <div id="selectedPoint" class="analysis-point">Click the map to choose a point.</div>
    <div class="analysis-buttons">
      <button id="loadMeteogram" type="button">Meteogram</button>
      <button id="loadSounding" type="button">Sounding</button>
      <button id="setCrossStart" type="button">Set A</button>
      <button id="setCrossEnd" type="button">Set B</button>
    </div>
    <label class="analysis-label">Cross Product<select id="crossProduct"></select></label>
    <div class="analysis-path">
      <div><strong>A</strong><br><span id="crossStartText">unset</span></div>
      <div><strong>B</strong><br><span id="crossEndText">unset</span></div>
    </div>
    <button id="renderCrossSection" type="button">Render Cross Section</button>
    <div id="analysisOutput" class="analysis-output">
      <div class="analysis-message">Meteograms and soundings use the clicked point. Cross sections use A to B.</div>
    </div>
  </div>
  <div class="panel">
    <label>Model<select id="model"></select></label>
    <label>Domain<select id="domain"></select></label>
    <label>Basemap<select id="basemap"></select></label>
    <label>Run A<select id="runA"></select></label>
    <label>Run B<select id="runB"></select></label>
    <label class="wide">Layer<select id="layer"></select></label>
    <label>Hour<select id="hour"></select></label>
    <label>Palette<select id="palette">
      <option value="auto">auto</option>
      <option value="vpd">vpd</option>
      <option value="temperature">temperature</option>
      <option value="humidity">humidity</option>
      <option value="wind">wind</option>
      <option value="magma">magma</option>
      <option value="fire_weather">fire</option>
      <option value="gray">gray</option>
    </select></label>
    <label>Min<input id="min" inputmode="decimal" /></label>
    <label>Max<input id="max" inputmode="decimal" /></label>
    <label>Compare<span class="check"><input id="compare" type="checkbox" /> side</span></label>
    <button id="apply">Apply</button>
    <a class="nav-button" href="/satellite">Satellite</a>
    <a class="nav-button" href="/plots">Plots</a>
  </div>
  <div id="layerStackPanel" class="layer-stack-panel">
    <div class="layer-stack-head">
      <div class="layer-stack-title">Layer Stack</div>
      <button id="addLayer" type="button">Add</button>
      <button id="clearLayers" type="button">Clear</button>
    </div>
    <div id="layerStack" class="layer-stack">
      <div class="layer-empty">Apply replaces the map. Add stacks the selected product over existing layers.</div>
    </div>
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
      domain: document.getElementById("domain"),
      basemap: document.getElementById("basemap"),
      runA: document.getElementById("runA"),
      runB: document.getElementById("runB"),
      layer: document.getElementById("layer"),
      hour: document.getElementById("hour"),
      palette: document.getElementById("palette"),
      min: document.getElementById("min"),
      max: document.getElementById("max"),
      compare: document.getElementById("compare"),
      apply: document.getElementById("apply"),
      addLayer: document.getElementById("addLayer"),
      clearLayers: document.getElementById("clearLayers"),
      layerStack: document.getElementById("layerStack"),
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
      selectedPoint: document.getElementById("selectedPoint"),
      loadMeteogram: document.getElementById("loadMeteogram"),
      loadSounding: document.getElementById("loadSounding"),
      setCrossStart: document.getElementById("setCrossStart"),
      setCrossEnd: document.getElementById("setCrossEnd"),
      crossProduct: document.getElementById("crossProduct"),
      crossStartText: document.getElementById("crossStartText"),
      crossEndText: document.getElementById("crossEndText"),
      renderCrossSection: document.getElementById("renderCrossSection"),
      analysisOutput: document.getElementById("analysisOutput"),
    };
    const BASEMAPS = {
      dark: {
        label: "Dark",
        url: "https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png",
        options: { maxZoom: 19, subdomains: "abcd", attribution: "&copy; OpenStreetMap &copy; CARTO" },
      },
      light: {
        label: "Light",
        url: "https://{s}.basemaps.cartocdn.com/light_all/{z}/{x}/{y}{r}.png",
        options: { maxZoom: 19, subdomains: "abcd", attribution: "&copy; OpenStreetMap &copy; CARTO" },
      },
      osm: {
        label: "OSM",
        url: "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
        options: { maxZoom: 19, attribution: "&copy; OpenStreetMap" },
      },
      topo: {
        label: "Topo",
        url: "https://{s}.tile.opentopomap.org/{z}/{x}/{y}.png",
        options: { maxZoom: 17, attribution: "&copy; OpenStreetMap &copy; OpenTopoMap" },
      },
      satellite: {
        label: "Satellite",
        url: "https://server.arcgisonline.com/ArcGIS/rest/services/World_Imagery/MapServer/tile/{z}/{y}/{x}",
        options: { maxZoom: 19, attribution: "Tiles &copy; Esri" },
      },
    };
    const mapA = L.map("mapA", { zoomControl: true }).setView([36.5, -116.5], 5);
    const mapB = L.map("mapB", { zoomControl: false }).setView([36.5, -116.5], 5);
    let baseA = null;
    let baseB = null;
    let syncing = false;
    function syncMaps(source, target) {
      if (syncing) return;
      syncing = true;
      target.setView(source.getCenter(), source.getZoom(), { animate: false });
      syncing = false;
    }
    mapA.on("moveend", () => syncMaps(mapA, mapB));
    mapB.on("moveend", () => syncMaps(mapB, mapA));
    let activeLayers = [];
    let applyGeneration = 0;
    let variablesByRun = {};
    let modelInfoById = {};
    let runs = [];
    let pickerAbort = null;
    let pickerLastAt = 0;
    let selectedPoint = null;
    let crossStart = null;
    let crossEnd = null;
    const analysisLayerA = L.layerGroup().addTo(mapA);
    const analysisLayerB = L.layerGroup().addTo(mapB);
    const meteogramPreferred = ["2m_temperature", "2m_dewpoint", "2m_relative_humidity", "10m_wind_gusts", "10m_wind_1h_max", "qpf_1h", "qpf_total", "composite_reflectivity"];
    const analysisColors = ['#ef4444', '#2563eb', '#16a34a', '#7c3aed', '#f97316', '#0891b2', '#334155', '#db2777'];
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

    async function fetchJson(url, options) {
      const res = await fetch(url, options);
      if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
      return res.json();
    }

    function populateBasemaps() {
      els.basemap.innerHTML = Object.entries(BASEMAPS)
        .map(([key, config]) => `<option value="${key}">${config.label}</option>`)
        .join("");
      els.basemap.value = localStorage.getItem("wxstore_basemap") || "dark";
      if (!BASEMAPS[els.basemap.value]) els.basemap.value = "dark";
    }

    function addBase(map, key) {
      const config = BASEMAPS[key] || BASEMAPS.dark;
      return L.tileLayer(config.url, config.options).addTo(map);
    }

    function setBasemap(key) {
      if (baseA) mapA.removeLayer(baseA);
      if (baseB) mapB.removeLayer(baseB);
      baseA = addBase(mapA, key);
      baseB = addBase(mapB, key);
      localStorage.setItem("wxstore_basemap", key);
      updateUsageDisplay();
    }

    function defaultsFor(name) {
      const lower = name.toLowerCase();
      if (lower.includes("rh") || lower.includes("humidity") || lower.includes("cloud")) return ["humidity", "0", "100"];
      if (lower.includes("vpd")) return ["vpd", "0", "40"];
      if (lower.includes("fire_weather")) return ["fire_weather", "0", "100"];
      if (lower.includes("stp") || lower.includes("scp") || lower.includes("ehi")) return ["magma", "0", "5"];
      if (lower.includes("cape")) return ["magma", "0", "5000"];
      if (lower.includes("cin")) return ["magma", "-250", "0"];
      if (lower.includes("qpf") || lower.includes("precip")) return ["magma", "0", "75"];
      if (lower.includes("temp") || lower.includes("dewpoint") || lower.includes("wetbulb") || lower.includes("heat_index") || lower.includes("wind_chill")) return ["temperature", "-35", "45"];
      if (lower.includes("wind") || lower.includes("shear")) return ["wind", "0", "45"];
      if (lower.includes("visibility")) return ["gray", "0", "16093"];
      return ["temperature", "0", "1"];
    }

    function setStatus(text) {
      els.status.textContent = text;
    }

    function activeRunForMap(map) {
      return map === mapB && els.compare.checked ? els.runB.value : els.runA.value;
    }

    function activeSampleConfig(map) {
      const top = activeLayers[activeLayers.length - 1];
      if (top) {
        return {
          model: top.config.model,
          run: map === mapB && top.config.compare ? top.config.runB : top.config.runA,
          layer: top.config.layer,
          hour: top.config.hour,
        };
      }
      return {
        model: els.model.value,
        run: activeRunForMap(map),
        layer: els.layer.value,
        hour: els.hour.value,
      };
    }

    function formatLayerValue(value, units) {
      if (value === null || value === undefined || !Number.isFinite(Number(value))) {
        return "no data";
      }
      const n = Number(value);
      const precision = Math.abs(n) >= 100 ? 0 : Math.abs(n) >= 10 ? 1 : 2;
      return `${n.toFixed(precision)} ${units || ""}`.trim();
    }

    function setPickerWaiting(latlng, run, layer, hour) {
      els.pickerValue.textContent = "sampling...";
      els.pickerMeta.textContent = `${layer} f${String(hour).padStart(3, "0")} | ${run} | ${latlng.lat.toFixed(4)}, ${latlng.lng.toFixed(4)}`;
    }

    async function samplePicker(map, latlng) {
      const now = performance.now();
      if (now - pickerLastAt < 120) return;
      pickerLastAt = now;
      const sample = activeSampleConfig(map);
      const run = sample.run;
      const layer = sample.layer;
      const hour = sample.hour;
      if (!run || !layer || hour === "") return;
      if (pickerAbort) pickerAbort.abort();
      pickerAbort = new AbortController();
      setPickerWaiting(latlng, run, layer, hour);
      const params = new URLSearchParams({
        model: sample.model,
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

    function escapeHtml(value) {
      return String(value).replace(/[&<>"']/g, ch => ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;",
      }[ch]));
    }

    function pointLabel(point) {
      return point ? `${point.lat.toFixed(4)}, ${point.lng.toFixed(4)}` : "unset";
    }

    function analysisMessage(text) {
      els.analysisOutput.innerHTML = `<div class="analysis-message">${escapeHtml(text)}</div>`;
    }

    function redrawAnalysisMarkers() {
      analysisLayerA.clearLayers();
      analysisLayerB.clearLayers();
      const drawOn = group => {
        if (selectedPoint) {
          L.circleMarker([selectedPoint.lat, selectedPoint.lng], {
            radius: 6,
            color: '#111827',
            weight: 2,
            fillColor: '#facc15',
            fillOpacity: 0.95,
          }).addTo(group);
        }
        if (crossStart) {
          L.circleMarker([crossStart.lat, crossStart.lng], {
            radius: 6,
            color: '#0f766e',
            weight: 3,
            fillColor: '#ccfbf1',
            fillOpacity: 0.95,
          }).bindTooltip("A", { permanent: true, direction: "top" }).addTo(group);
        }
        if (crossEnd) {
          L.circleMarker([crossEnd.lat, crossEnd.lng], {
            radius: 6,
            color: '#be123c',
            weight: 3,
            fillColor: '#ffe4e6',
            fillOpacity: 0.95,
          }).bindTooltip("B", { permanent: true, direction: "top" }).addTo(group);
        }
        if (crossStart && crossEnd) {
          L.polyline([[crossStart.lat, crossStart.lng], [crossEnd.lat, crossEnd.lng]], {
            color: '#facc15',
            weight: 3,
            opacity: 0.95,
            dashArray: "8 6",
          }).addTo(group);
        }
      };
      drawOn(analysisLayerA);
      drawOn(analysisLayerB);
    }

    function updateCrossText() {
      els.crossStartText.textContent = pointLabel(crossStart);
      els.crossEndText.textContent = pointLabel(crossEnd);
    }

    function setSelectedPoint(latlng) {
      selectedPoint = { lat: latlng.lat, lng: latlng.lng };
      els.selectedPoint.textContent = `Selected ${pointLabel(selectedPoint)} | ${els.model.value} ${els.runA.value || "latest"}`;
      redrawAnalysisMarkers();
    }

    function setCrossPoint(which) {
      if (!selectedPoint) {
        analysisMessage("Click the map first, then set A or B.");
        return;
      }
      const point = { lat: selectedPoint.lat, lng: selectedPoint.lng };
      if (which === "start") {
        crossStart = point;
      } else {
        crossEnd = point;
      }
      updateCrossText();
      redrawAnalysisMarkers();
      analysisMessage(`Cross-section point ${which === "start" ? "A" : "B"} set to ${pointLabel(point)}.`);
    }

    function compactHours(hours) {
      const sorted = Array.from(new Set(hours.map(Number).filter(Number.isFinite))).sort((a, b) => a - b);
      if (!sorted.length) return "";
      const parts = [];
      let start = sorted[0];
      let prev = sorted[0];
      for (let i = 1; i < sorted.length; i += 1) {
        const hour = sorted[i];
        if (hour === prev + 1) {
          prev = hour;
          continue;
        }
        parts.push(start === prev ? String(start) : `${start}-${prev}`);
        start = hour;
        prev = hour;
      }
      parts.push(start === prev ? String(start) : `${start}-${prev}`);
      return parts.join(",");
    }

    function commonForecastHours(vars, availableHours) {
      let common = null;
      for (const variable of vars) {
        const hours = availableHours[variable] || [];
        if (!hours.length) continue;
        const set = new Set(hours.map(Number));
        common = common === null ? Array.from(set) : common.filter(hour => set.has(hour));
      }
      return (common || []).sort((a, b) => a - b);
    }

    async function meteogramRequest() {
      const data = await loadVariablesFor(els.runA.value);
      const available = new Set(data.variables || []);
      const candidates = [els.layer.value].concat(meteogramPreferred);
      const vars = [];
      for (const variable of candidates) {
        if (available.has(variable) && !vars.includes(variable)) vars.push(variable);
        if (vars.length >= 8) break;
      }
      if (!vars.length && data.variables && data.variables.length) vars.push(data.variables[0]);
      let hours = commonForecastHours(vars, data.hours || {});
      if (!hours.length && els.hour.value !== "") hours = [Number(els.hour.value)];
      hours = hours.slice(0, 49);
      return { vars, hours };
    }

    function variableLabel(variable) {
      const labels = {
        "2m_temperature": "2m Temp",
        "2m_dewpoint": "2m Dewpoint",
        "2m_relative_humidity": "2m RH",
        "10m_wind_gusts": "10m Gust",
        "10m_wind_1h_max": "1h Wind Max",
        "qpf_1h": "1h QPF",
        "qpf_total": "Run QPF",
        "composite_reflectivity": "Comp Refl",
        "cloud_cover": "Cloud Cover",
        "vpd_2m": "2m VPD",
        "stp_fixed": "STP",
      };
      return labels[variable] || variable.replaceAll("_", " ");
    }

    function hourlySeries(data, variable) {
      return (data.hourly?.[variable] || []).map(value => value == null ? NaN : Number(value));
    }

    function finiteValues(values) {
      return values.filter(Number.isFinite);
    }

    function firstFinite(values) {
      return values.find(Number.isFinite);
    }

    function maxFinite(values) {
      const finite = finiteValues(values);
      return finite.length ? Math.max(...finite) : NaN;
    }

    function sumFinite(values) {
      return finiteValues(values).reduce((sum, value) => sum + value, 0);
    }

    function unitsFor(data, variable) {
      return data.hourly_units?.[variable] || "";
    }

    function formatMetric(value, unit) {
      if (!Number.isFinite(value)) return "--";
      const abs = Math.abs(value);
      const precision = abs >= 100 ? 0 : abs >= 10 ? 1 : 2;
      return `${value.toFixed(precision)}${unit ? " " + unit : ""}`;
    }

    function htmlStat(label, value, unit) {
      return `<div class='meteogram-stat'><span>${escapeHtml(label)}</span><strong>${escapeHtml(formatMetric(value, unit))}</strong></div>`;
    }

    function timeLabel(iso) {
      const date = new Date(iso);
      if (!Number.isFinite(date.getTime())) return "";
      return `${String(date.getUTCHours()).padStart(2, "0")}z`;
    }

    function dayLabel(iso) {
      const date = new Date(iso);
      if (!Number.isFinite(date.getTime())) return "";
      return date.toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric", timeZone: "UTC" });
    }

    function buildPanelSvg(panel, times, data, layout) {
      const { W, L, R } = layout;
      const H = panel.height;
      const T = panel.y;
      const B = 24;
      const plotTop = T + 30;
      const plotBottom = T + H - B;
      const plotHeight = Math.max(1, plotBottom - plotTop);
      const series = panel.variables
        .filter(variable => data.hourly?.[variable])
        .map((variable, idx) => ({
          variable,
          values: hourlySeries(data, variable),
          color: panel.colors[idx % panel.colors.length],
          kind: panel.kind || "line",
          unit: unitsFor(data, variable),
        }));
      if (!series.length) return "";
      const values = series.flatMap(item => finiteValues(item.values));
      if (!values.length) return "";
      let min = panel.min ?? Math.min(...values);
      let max = panel.max ?? Math.max(...values);
      if (panel.pad !== false) {
        const span = Math.max(1.0e-6, max - min);
        min -= span * 0.12;
        max += span * 0.12;
      }
      if (min === max) {
        min -= 1;
        max += 1;
      }
      const x = index => L + (W - L - R) * (index / Math.max(1, times.length - 1));
      const y = value => plotTop + plotHeight * (1 - (value - min) / (max - min));
      let html = `<g>`;
      html += `<rect x='12' y='${T}' width='${W - 24}' height='${H - 6}' rx='10' fill='${panel.fill}' stroke='#dbe3ee'/>`;
      html += `<text x='24' y='${T + 20}' font-size='16' font-weight='900' fill='#0f172a'>${escapeHtml(panel.title)}</text>`;
      for (let grid = 0; grid <= 3; grid += 1) {
        const yy = plotTop + plotHeight * grid / 3;
        const val = max - (max - min) * grid / 3;
        html += `<line x1='${L}' y1='${yy.toFixed(1)}' x2='${W - R}' y2='${yy.toFixed(1)}' stroke='#e2e8f0'/>`;
        html += `<text x='18' y='${(yy + 4).toFixed(1)}' font-size='11' fill='#64748b'>${formatMetric(val, panel.axisUnit || series[0].unit)}</text>`;
      }
      if (panel.kind === "bar") {
        const barWidth = Math.max(3, (W - L - R) / Math.max(2, times.length) * 0.58);
        series.forEach(item => {
          item.values.forEach((value, index) => {
            if (!Number.isFinite(value)) return;
            const xx = x(index) - barWidth / 2;
            const yy = y(value);
            html += `<rect x='${xx.toFixed(1)}' y='${yy.toFixed(1)}' width='${barWidth.toFixed(1)}' height='${Math.max(1, plotBottom - yy).toFixed(1)}' rx='2' fill='${item.color}' opacity='.78'/>`;
          });
        });
      } else {
        series.forEach(item => {
          const points = item.values
            .map((value, index) => Number.isFinite(value) ? `${x(index).toFixed(1)},${y(value).toFixed(1)}` : "")
            .filter(Boolean)
            .join(" ");
          html += `<polyline points='${points}' fill='none' stroke='${item.color}' stroke-width='4' stroke-linejoin='round' stroke-linecap='round'/>`;
          item.values.forEach((value, index) => {
            if (!Number.isFinite(value) || index % Math.max(1, Math.ceil(times.length / 10)) !== 0) return;
            html += `<circle cx='${x(index).toFixed(1)}' cy='${y(value).toFixed(1)}' r='3.2' fill='#fff' stroke='${item.color}' stroke-width='2'/>`;
          });
        });
      }
      let legendX = W - R - 8;
      series.slice().reverse().forEach(item => {
        const label = variableLabel(item.variable);
        const width = Math.max(70, label.length * 6.5 + 22);
        legendX -= width;
        html += `<rect x='${legendX}' y='${T + 8}' width='${width - 8}' height='18' rx='9' fill='#fff' stroke='#e2e8f0'/>`;
        html += `<circle cx='${legendX + 10}' cy='${T + 17}' r='4' fill='${item.color}'/>`;
        html += `<text x='${legendX + 18}' y='${T + 21}' font-size='11' font-weight='800' fill='#334155'>${escapeHtml(label)}</text>`;
      });
      html += `</g>`;
      return html;
    }

    function drawMeteogram(data, vars) {
      const times = data.hourly && data.hourly.time ? data.hourly.time : [];
      const values = vars.flatMap(variable => finiteValues(hourlySeries(data, variable)));
      if (!times.length || !values.length) {
        analysisMessage("No meteogram values came back for that point.");
        return;
      }
      const temp = hourlySeries(data, "2m_temperature");
      const dew = hourlySeries(data, "2m_dewpoint");
      const gust = hourlySeries(data, "10m_wind_gusts");
      const qpf = hourlySeries(data, "qpf_1h");
      const statHtml = [
        htmlStat("temp", firstFinite(temp), unitsFor(data, "2m_temperature")),
        htmlStat("dewpoint", firstFinite(dew), unitsFor(data, "2m_dewpoint")),
        htmlStat("gust max", maxFinite(gust), unitsFor(data, "10m_wind_gusts")),
        htmlStat("qpf", sumFinite(qpf), unitsFor(data, "qpf_1h")),
      ].join("");
      const panels = [
        { title: "Temperature / Moisture", variables: ["2m_temperature", "2m_dewpoint"], colors: ['#dc2626', '#2563eb'], fill: '#fff7ed', height: 150 },
        { title: "Humidity", variables: ["2m_relative_humidity"], colors: ['#16a34a'], fill: '#f0fdf4', min: 0, max: 100, axisUnit: "%", pad: false, height: 118 },
        { title: "Wind Gusts", variables: ["10m_wind_gusts", "10m_wind_1h_max"], colors: ['#7c3aed', '#a855f7'], fill: '#faf5ff', height: 130 },
        { title: "Precip / Reflectivity", variables: ["qpf_1h", "composite_reflectivity"], colors: ['#0891b2', '#f97316'], fill: '#ecfeff', height: 135 },
      ];
      const standardPanelVars = new Set(panels.flatMap(panel => panel.variables));
      const selectedExtras = vars.filter(variable => !standardPanelVars.has(variable));
      if (selectedExtras.length) {
        panels.unshift({ title: "Selected Map Layer", variables: selectedExtras.slice(0, 2), colors: ['#db2777', '#334155'], fill: '#fdf2f8', height: 125 });
      }
      const W = 980;
      const L = 96;
      const R = 28;
      const panelGap = 12;
      let yOffset = 18;
      const layout = { W, L, R };
      const chartHeight = 18 + panels.reduce((sum, panel) => sum + panel.height + panelGap, 0) + 44;
      let chart = `<svg viewBox='0 0 ${W} ${chartHeight}' role='img' aria-label='meteogram'>`;
      chart += `<rect x='0' y='0' width='${W}' height='${chartHeight}' fill='#f8fafc'/>`;
      for (const panel of panels) {
        panel.y = yOffset;
        chart += buildPanelSvg(panel, times, data, layout);
        yOffset += panel.height + panelGap;
      }
      const axisY = yOffset - 10;
      const x = index => L + (W - L - R) * (index / Math.max(1, times.length - 1));
      for (let index = 0; index < times.length; index += Math.max(1, Math.ceil(times.length / 8))) {
        const xx = x(index);
        chart += `<line x1='${xx.toFixed(1)}' y1='18' x2='${xx.toFixed(1)}' y2='${axisY}' stroke='#cbd5e1' stroke-dasharray='3 7' opacity='.45'/>`;
        chart += `<text x='${xx.toFixed(1)}' y='${axisY + 20}' font-size='12' font-weight='800' text-anchor='middle' fill='#475569'>${timeLabel(times[index])}</text>`;
      }
      chart += `<text x='${L}' y='${axisY + 42}' font-size='13' font-weight='900' fill='#0f172a'>${escapeHtml(dayLabel(times[0]))} to ${escapeHtml(dayLabel(times[times.length - 1]))}</text>`;
      chart += `</svg>`;
      const subtitle = `${escapeHtml(data.model)} ${escapeHtml(data.run)} | ${pointLabel(selectedPoint)} | ${times.length} forecast hours | ${Number(data.generationtime_ms || 0).toFixed(1)} ms`;
      els.analysisOutput.innerHTML =
        `<div class='meteogram-card'>` +
        `<div class='meteogram-hero'><div class='meteogram-kicker'>WxStore Point Forecast</div><div class='meteogram-title'>${escapeHtml(pointLabel(selectedPoint))}</div><div class='meteogram-subtitle'>${subtitle}</div></div>` +
        `<div class='meteogram-stats'>${statHtml}</div>` +
        `<div class='meteogram-chart'>${chart}</div>` +
        `</div>`;
    }

    async function loadMeteogramFromMap() {
      if (!selectedPoint) {
        analysisMessage("Click the map first to choose the meteogram point.");
        return;
      }
      els.loadMeteogram.disabled = true;
      analysisMessage("Loading meteogram...");
      try {
        const request = await meteogramRequest();
        if (!request.vars.length || !request.hours.length) throw new Error("no available forecast variables/hours for this run");
        const params = new URLSearchParams({
          model: els.model.value,
          run: els.runA.value || "latest",
          lat: selectedPoint.lat.toString(),
          lon: selectedPoint.lng.toString(),
          hours: compactHours(request.hours),
          hourly: request.vars.join(","),
        });
        const data = await fetchJson(`/v1/forecast?${params.toString()}`);
        drawMeteogram(data, request.vars);
      } catch (err) {
        analysisMessage(`Meteogram failed: ${err.message}`);
      } finally {
        els.loadMeteogram.disabled = false;
      }
    }

    async function loadSoundingFromMap() {
      if (!selectedPoint) {
        analysisMessage("Click the map first to choose the sounding point.");
        return;
      }
      els.loadSounding.disabled = true;
      analysisMessage("Rendering sounding...");
      try {
        const report = await fetchJson("/v1/sounding/render", {
          method: "POST",
          headers: {"Content-Type": "application/json"},
          body: JSON.stringify({
            model: els.model.value,
            run: els.runA.value || "latest",
            lat: selectedPoint.lat,
            lon: selectedPoint.lng,
            hour: Number(els.hour.value || 0),
            sample_method: "inverse-distance4",
            crop_radius_deg: 1.25,
          }),
        });
        const imageUrl = report.png_url || report.output?.png_url;
        if (!imageUrl) throw new Error("sounding renderer returned no image");
        const profile = report.profile || {};
        els.analysisOutput.innerHTML =
          `<a href='${escapeHtml(imageUrl)}' target='_blank' rel='noopener'><img src='${escapeHtml(imageUrl)}' alt='sounding'></a>` +
          `<div class='analysis-meta'>${escapeHtml(report.model || els.model.value)} ${escapeHtml(report.resolved_run || els.runA.value || "latest")} f${String(report.request?.forecast_hour ?? els.hour.value ?? 0).padStart(3, "0")} | ${escapeHtml(profile.levels || "--")} levels | ${report.cache_hit ? "cached" : "rendered"} | ${report.server_elapsed_ms || report.timing?.total_ms || "--"} ms</div>`;
      } catch (err) {
        analysisMessage(`Sounding failed: ${err.message}`);
      } finally {
        els.loadSounding.disabled = false;
      }
    }

    async function loadCrossProducts() {
      try {
        const data = await fetchJson("/v1/cross-section/products");
        const products = data.products || [];
        els.crossProduct.innerHTML = products
          .map(item => `<option value='${escapeHtml(item.product)}'>${escapeHtml(item.label || item.product)}</option>`)
          .join("");
        if (products.some(item => item.product === "wind_speed")) els.crossProduct.value = "wind_speed";
      } catch (err) {
        els.crossProduct.innerHTML = "<option value='wind_speed'>Wind Speed</option>";
      }
    }

    async function renderCrossSectionFromMap() {
      if (!crossStart || !crossEnd) {
        analysisMessage("Set A and B from clicked map points first.");
        return;
      }
      els.renderCrossSection.disabled = true;
      analysisMessage("Rendering cross section...");
      try {
        const report = await fetchJson("/v1/cross-section/render", {
          method: "POST",
          headers: {"Content-Type": "application/json"},
          body: JSON.stringify({
            model: els.model.value,
            run: els.runA.value || "latest",
            start_lat: crossStart.lat,
            start_lon: crossStart.lng,
            end_lat: crossEnd.lat,
            end_lon: crossEnd.lng,
            hour: Number(els.hour.value || 0),
            product: els.crossProduct.value || "wind_speed",
            width: 1400,
            height: 820,
          }),
        });
        const first = (report.outputs || [])[0];
        const imageUrl = first && (first.webp_url || first.png_url);
        if (!imageUrl) throw new Error("renderer returned no image");
        els.analysisOutput.innerHTML =
          `<a href='${escapeHtml(imageUrl)}' target='_blank' rel='noopener'><img src='${escapeHtml(imageUrl)}' alt='cross section'></a>` +
          `<div class='analysis-meta'>${escapeHtml(report.model || els.model.value)} ${escapeHtml(report.run || els.runA.value || "latest")} f${String(report.hour ?? els.hour.value ?? 0).padStart(3, "0")} | ${escapeHtml(els.crossProduct.value)} | ${report.cache_hit ? "cached" : "rendered"} | ${report.total_ms || report.server_elapsed_ms || "--"} ms</div>`;
      } catch (err) {
        analysisMessage(`Cross section failed: ${err.message}`);
      } finally {
        els.renderCrossSection.disabled = false;
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
      return activeLayers.reduce((count, entry) => {
        return count + [entry.layerA, entry.layerB].reduce((layerCount, layer) => {
          if (!layer || !layer._tiles) return layerCount;
          return layerCount + Object.keys(layer._tiles).length;
        }, 0);
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
      loadDomainList();
    }

    function loadDomainList() {
      const model = modelInfoById[els.model.value];
      const readiness = model && model.latest_readiness ? model.latest_readiness : {};
      const domains = readiness.domains && readiness.domains.length ? readiness.domains : ["native"];
      els.domain.innerHTML = "";
      for (const domain of domains) {
        const opt = document.createElement("option");
        opt.value = domain;
        opt.textContent = domain;
        els.domain.appendChild(opt);
      }
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
      const previous = els.layer.value;
      const data = await loadVariablesFor(run);
      els.layer.innerHTML = "";
      for (const name of data.variables) {
        const opt = document.createElement("option");
        opt.value = name;
        opt.textContent = name;
        els.layer.appendChild(opt);
      }
      const preferred = ["vpd_2m", "stp_fixed", "2m_temperature", "composite_reflectivity"].find(v => data.hours[v]);
      if (previous && data.variables.includes(previous)) {
        els.layer.value = previous;
      } else if (preferred) {
        els.layer.value = preferred;
      }
      await refreshHours();
    }

    async function refreshHours() {
      const runA = await loadVariablesFor(els.runA.value);
      const runB = await loadVariablesFor(els.runB.value);
      const previous = els.hour.value;
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
      if (previous !== "" && common.map(String).includes(String(previous))) {
        els.hour.value = previous;
      }
      const [palette, min, max] = defaultsFor(els.layer.value);
      els.palette.value = palette;
      els.min.value = min;
      els.max.value = max;
    }

    function currentLayerConfig() {
      const layer = els.layer.value;
      const hour = els.hour.value;
      const palette = els.palette.value === "auto" ? defaultsFor(layer)[0] : els.palette.value;
      return {
        id: `${Date.now()}_${Math.random().toString(16).slice(2)}`,
        model: els.model.value,
        runA: els.runA.value,
        runB: els.runB.value,
        compare: els.compare.checked,
        layer,
        hour,
        palette,
        range: `${els.min.value},${els.max.value}`,
        opacity: 0.72,
      };
    }

    function layerLabel(config) {
      return `${config.model} ${config.layer} f${String(config.hour).padStart(3, "0")}`;
    }

    async function fetchLayerDoc(config, run) {
      const url = `/v1/mapbox/layers/${config.model}/${run}/${config.layer}?hours=${config.hour}&palette=${encodeURIComponent(config.palette)}&range=${encodeURIComponent(config.range)}&base_url=${encodeURIComponent(tileBase())}`;
      const res = await fetch(url);
      if (!res.ok) throw new Error(`${run} layer failed: ${res.status}`);
      const data = await res.json();
      const frame = data.frames && data.frames[0];
      if (!frame) throw new Error(`${run} has no frame`);
      return { data, frame };
    }

    function addTileLayer(map, frame, data, opacity) {
      const next = L.tileLayer(frame.tiles[0], { opacity, maxZoom: data.maxzoom || 9 }).addTo(map);
      next.on("tileload tileerror loading load", updateUsageDisplay);
      return next;
    }

    function fitToBounds(bounds) {
      if (!bounds) return;
      mapA.fitBounds([[bounds[1], bounds[0]], [bounds[3], bounds[2]]], { padding: [18, 18] });
    }

    async function createLayerEntry(config, fit) {
      if (!config.runA || !config.layer || config.hour === "") throw new Error("choose a run, layer, and hour first");
      const a = await fetchLayerDoc(config, config.runA);
      const entry = {
        id: config.id,
        config,
        opacity: config.opacity,
        layerA: addTileLayer(mapA, a.frame, a.data, config.opacity),
        layerB: null,
        bounds: a.data.bounds,
      };
      try {
        if (config.compare) {
          const b = await fetchLayerDoc(config, config.runB);
          entry.layerB = addTileLayer(mapB, b.frame, b.data, config.opacity);
        }
      } catch (err) {
        removeLayerEntry(entry);
        throw err;
      }
      if (fit) fitToBounds(entry.bounds);
      return entry;
    }

    function removeLayerEntry(entry) {
      if (entry.layerA) mapA.removeLayer(entry.layerA);
      if (entry.layerB) mapB.removeLayer(entry.layerB);
    }

    function clearActiveLayers() {
      for (const entry of activeLayers) removeLayerEntry(entry);
      activeLayers = [];
      syncLayerBadges();
      renderLayerStack();
      updateUsageDisplay();
    }

    function syncLayerBadges() {
      const top = activeLayers[activeLayers.length - 1];
      if (!top) {
        els.badgeA.textContent = "";
        els.badgeB.textContent = "";
        return;
      }
      els.badgeA.textContent = `${top.config.runA} ${top.config.layer} f${String(top.config.hour).padStart(3, "0")}`;
      els.badgeB.textContent = top.config.compare ? `${top.config.runB} ${top.config.layer} f${String(top.config.hour).padStart(3, "0")}` : "";
    }

    function renderLayerStack() {
      if (!activeLayers.length) {
        els.layerStack.innerHTML = `<div class="layer-empty">Apply replaces the map. Add stacks the selected product over existing layers.</div>`;
        return;
      }
      els.layerStack.innerHTML = activeLayers.map((entry, index) => {
        const isPrimary = index === activeLayers.length - 1;
        const meta = `${entry.config.runA}${entry.config.compare ? " / " + entry.config.runB : ""} | ${entry.config.palette} ${entry.config.range}`;
        return `<div class="layer-row ${isPrimary ? "primary" : ""}" data-layer-id="${entry.id}">` +
          `<div><div class="layer-row-title">${escapeHtml(layerLabel(entry.config))}</div><div class="layer-row-meta">${escapeHtml(meta)}</div></div>` +
          `<input type="range" min="0" max="1" step="0.05" value="${entry.opacity}" data-opacity="${entry.id}" title="Opacity" />` +
          `<button type="button" data-remove-layer="${entry.id}" title="Remove layer">x</button>` +
          `</div>`;
      }).join("");
      els.layerStack.querySelectorAll("[data-opacity]").forEach(input => {
        input.addEventListener("input", () => {
          const entry = activeLayers.find(item => item.id === input.dataset.opacity);
          if (!entry) return;
          entry.opacity = Number(input.value);
          if (entry.layerA) entry.layerA.setOpacity(entry.opacity);
          if (entry.layerB) entry.layerB.setOpacity(entry.opacity);
        });
      });
      els.layerStack.querySelectorAll("[data-remove-layer]").forEach(button => {
        button.addEventListener("click", () => {
          const index = activeLayers.findIndex(item => item.id === button.dataset.removeLayer);
          if (index < 0) return;
          const [entry] = activeLayers.splice(index, 1);
          removeLayerEntry(entry);
          syncLayerBadges();
          renderLayerStack();
          updateUsageDisplay();
        });
      });
    }

    async function applyLayer() {
      const generation = ++applyGeneration;
      const config = currentLayerConfig();
      document.body.classList.toggle("compare", config.compare);
      setTimeout(() => { mapA.invalidateSize(); mapB.invalidateSize(); }, 40);
      setStatus(`Loading ${layerLabel(config)}...`);
      const entry = await createLayerEntry(config, true);
      if (generation !== applyGeneration) {
        removeLayerEntry(entry);
        return;
      }
      clearActiveLayers();
      activeLayers.push(entry);
      syncLayerBadges();
      renderLayerStack();
      setStatus(`${layerLabel(config)} | ${config.palette} ${config.range} | A=${config.runA}${config.compare ? " B=" + config.runB : ""}`);
    }

    async function addLayerFromControls() {
      const generation = ++applyGeneration;
      const config = currentLayerConfig();
      document.body.classList.toggle("compare", config.compare);
      setTimeout(() => { mapA.invalidateSize(); mapB.invalidateSize(); }, 40);
      setStatus(`Adding ${layerLabel(config)}...`);
      const entry = await createLayerEntry(config, activeLayers.length === 0);
      if (generation !== applyGeneration) {
        removeLayerEntry(entry);
        return;
      }
      activeLayers.push(entry);
      syncLayerBadges();
      renderLayerStack();
      updateUsageDisplay();
      setStatus(`Added ${layerLabel(config)}. ${activeLayers.length} active overlay${activeLayers.length === 1 ? "" : "s"}.`);
    }

    els.model.addEventListener("change", () => {
      loadRunList();
      refreshLayerList()
        .then(() => setStatus(`Prepared ${els.model.value}. Click Apply to replace the map or Add to stack this product.`))
        .catch(err => setStatus(err.message));
    });
    els.domain.addEventListener("change", () => setStatus(`Domain set to ${els.domain.value}. Click Apply or Add when ready.`));
    els.basemap.addEventListener("change", () => setBasemap(els.basemap.value));
    els.runA.addEventListener("change", () => refreshLayerList().then(() => setStatus("Run A changed. Click Apply or Add to load it.")).catch(err => setStatus(err.message)));
    els.runB.addEventListener("change", () => refreshHours().then(() => setStatus("Run B changed. Click Apply or Add to load it.")).catch(err => setStatus(err.message)));
    els.layer.addEventListener("change", () => refreshHours().then(() => setStatus(`${els.layer.value} selected. Click Apply or Add to load it.`)).catch(err => setStatus(err.message)));
    els.compare.addEventListener("change", () => {
      document.body.classList.toggle("compare", els.compare.checked);
      refreshHours().then(() => setStatus(`Compare ${els.compare.checked ? "enabled" : "disabled"}. Click Apply or Add to load this view.`)).catch(err => setStatus(err.message));
    });
    els.apply.addEventListener("click", () => applyLayer().catch(err => setStatus(err.message)));
    els.addLayer.addEventListener("click", () => addLayerFromControls().catch(err => setStatus(err.message)));
    els.clearLayers.addEventListener("click", () => {
      ++applyGeneration;
      clearActiveLayers();
      setStatus("Cleared overlay layers.");
    });
    els.usageStart.addEventListener("click", startUsageMonitor);
    els.usageStop.addEventListener("click", stopUsageMonitor);
    els.usageReset.addEventListener("click", () => {
      const wasActive = usage.active;
      stopUsageMonitor();
      resetUsageCounters();
      if (wasActive) startUsageMonitor();
    });
    els.loadMeteogram.addEventListener("click", loadMeteogramFromMap);
    els.loadSounding.addEventListener("click", loadSoundingFromMap);
    els.setCrossStart.addEventListener("click", () => setCrossPoint("start"));
    els.setCrossEnd.addEventListener("click", () => setCrossPoint("end"));
    els.renderCrossSection.addEventListener("click", renderCrossSectionFromMap);
    for (const map of [mapA, mapB]) {
      map.on("moveend zoomend", updateUsageDisplay);
      map.on("mousemove", event => samplePicker(map, event.latlng));
      map.on("click", event => setSelectedPoint(event.latlng));
      map.on("mouseout", () => {
        els.pickerValue.textContent = "move over map";
        els.pickerMeta.textContent = "Samples the selected layer/hour from the WxStore grid.";
      });
    }
    populateBasemaps();
    setBasemap(els.basemap.value);
    loadCrossProducts();
    loadModelList().then(refreshLayerList).then(applyLayer).catch(err => setStatus(err.message));
  </script>
</body>
</html>
"#;

const RADAR_HTML: &str = r####"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>Radar QC</title>
  <link rel="preconnect" href="https://unpkg.com" />
  <link rel="preconnect" href="https://tile.openstreetmap.org" />
  <link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css" />
  <script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js"></script>
  <script src="https://unpkg.com/lucide@0.468.0/dist/umd/lucide.min.js"></script>
  <style>
    :root {
      color-scheme: dark;
      --bg: #101312;
      --panel: #171b1d;
      --panel-2: #202528;
      --line: #31383b;
      --line-soft: #252b2d;
      --text: #e7ede8;
      --muted: #9fa9a3;
      --accent: #77c857;
      --warn: #d99b39;
      --bad: #d85d54;
      --focus: #8fc7ff;
    }
    * {
      box-sizing: border-box;
    }
    html,
    body {
      height: 100%;
      margin: 0;
      background: var(--bg);
      color: var(--text);
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      font-size: 14px;
    }
    button,
    input,
    select {
      font: inherit;
    }
    a {
      color: var(--focus);
      text-decoration: none;
    }
    a:hover {
      text-decoration: underline;
    }
    .app {
      min-height: 100%;
      display: grid;
      grid-template-rows: auto minmax(0, 1fr);
    }
    .topbar {
      display: grid;
      grid-template-columns: auto minmax(220px, 1.1fr) minmax(180px, 0.8fr) minmax(160px, 0.7fr) auto auto;
      gap: 10px;
      align-items: end;
      padding: 10px 12px;
      border-bottom: 1px solid var(--line);
      background: #151817;
    }
    .brand {
      display: flex;
      flex-direction: column;
      gap: 2px;
      min-width: 118px;
    }
    .brand strong {
      font-size: 16px;
      line-height: 1.1;
    }
    .brand span {
      color: var(--muted);
      font-size: 12px;
    }
    .field {
      display: flex;
      flex-direction: column;
      gap: 5px;
      min-width: 0;
    }
    .field label,
    .control-label {
      color: var(--muted);
      font-size: 12px;
      line-height: 1.1;
    }
    select,
    input[type="range"] {
      width: 100%;
    }
    select {
      height: 34px;
      color: var(--text);
      background: var(--panel-2);
      border: 1px solid var(--line);
      border-radius: 6px;
      padding: 0 30px 0 10px;
      overflow: hidden;
      text-overflow: ellipsis;
    }
    select:focus,
    button:focus-visible,
    input:focus-visible {
      outline: 2px solid var(--focus);
      outline-offset: 1px;
    }
    button {
      min-height: 34px;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: var(--panel-2);
      color: var(--text);
      padding: 0 10px;
      cursor: pointer;
    }
    button:hover {
      border-color: #4a5558;
    }
    button:disabled,
    select:disabled {
      opacity: 0.48;
      cursor: not-allowed;
    }
    .icon-button {
      width: 38px;
      padding: 0;
      display: inline-grid;
      place-items: center;
    }
    .icon-button svg {
      width: 17px;
      height: 17px;
    }
    .segmented {
      height: 34px;
      display: grid;
      grid-template-columns: 1fr 1fr;
      border: 1px solid var(--line);
      border-radius: 6px;
      overflow: hidden;
      background: var(--panel-2);
    }
    .segmented button {
      min-height: 32px;
      border: 0;
      border-radius: 0;
      background: transparent;
      padding: 0 9px;
      color: var(--muted);
    }
    .segmented button.active {
      background: #2e3d31;
      color: var(--text);
    }
    .toggle {
      height: 34px;
      display: flex;
      align-items: center;
      gap: 8px;
      padding: 0 10px;
      border: 1px solid var(--line);
      border-radius: 6px;
      background: var(--panel-2);
      color: var(--muted);
      white-space: nowrap;
    }
    .toggle input {
      width: 16px;
      height: 16px;
      accent-color: var(--accent);
    }
    .shell {
      min-height: 0;
      display: grid;
      grid-template-columns: minmax(0, 1fr) 340px;
    }
    .map-wrap {
      position: relative;
      min-height: 0;
      background: #0c0f0e;
    }
    #map {
      position: absolute;
      inset: 0;
    }
    .map-status {
      position: absolute;
      left: 12px;
      right: 12px;
      bottom: 12px;
      z-index: 500;
      display: flex;
      gap: 8px;
      align-items: center;
      pointer-events: none;
    }
    .status-pill,
    .coord-pill {
      min-height: 30px;
      max-width: 100%;
      padding: 6px 9px;
      border: 1px solid rgba(220, 232, 222, 0.18);
      border-radius: 6px;
      background: rgba(16, 19, 18, 0.86);
      color: var(--muted);
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }
    .coord-pill {
      margin-left: auto;
      color: var(--text);
    }
    .panel {
      min-width: 0;
      min-height: 0;
      overflow: auto;
      border-left: 1px solid var(--line);
      background: var(--panel);
    }
    .panel-section {
      padding: 13px 14px;
      border-bottom: 1px solid var(--line-soft);
    }
    .panel-section h2 {
      margin: 0 0 10px;
      font-size: 14px;
      line-height: 1.2;
    }
    .sample-value {
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto;
      gap: 8px;
      align-items: baseline;
      margin-bottom: 10px;
    }
    .sample-value strong {
      font-size: 30px;
      line-height: 1;
      overflow-wrap: anywhere;
    }
    .sample-value span {
      color: var(--muted);
    }
    .kv {
      display: grid;
      grid-template-columns: minmax(108px, 0.46fr) minmax(0, 1fr);
      gap: 6px 10px;
      align-items: start;
    }
    .kv div:nth-child(odd) {
      color: var(--muted);
    }
    .kv div:nth-child(even) {
      min-width: 0;
      overflow-wrap: anywhere;
    }
    .badges {
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
      margin-top: 10px;
    }
    .badge {
      border: 1px solid var(--line);
      border-radius: 999px;
      padding: 3px 8px;
      color: var(--muted);
      background: #151817;
      font-size: 12px;
      line-height: 1.4;
    }
    .badge.good {
      color: #bdf1ad;
      border-color: rgba(119, 200, 87, 0.48);
      background: rgba(119, 200, 87, 0.12);
    }
    .badge.warn {
      color: #f2c57d;
      border-color: rgba(217, 155, 57, 0.52);
      background: rgba(217, 155, 57, 0.13);
    }
    .badge.bad {
      color: #f5a19b;
      border-color: rgba(216, 93, 84, 0.52);
      background: rgba(216, 93, 84, 0.12);
    }
    .qc-grid {
      display: grid;
      gap: 8px;
    }
    .qc-card {
      border: 1px solid var(--line);
      border-radius: 6px;
      padding: 10px;
      background: #141817;
    }
    .qc-card strong {
      display: block;
      margin-bottom: 6px;
      font-size: 13px;
    }
    .qc-card .kv {
      grid-template-columns: minmax(100px, 0.48fr) minmax(0, 1fr);
      font-size: 13px;
    }
    .raw-json {
      margin: 0;
      max-height: 210px;
      overflow: auto;
      white-space: pre-wrap;
      color: #c9d1cc;
      font-family: ui-monospace, SFMono-Regular, Consolas, "Liberation Mono", monospace;
      font-size: 12px;
      line-height: 1.45;
    }
    .leaflet-container {
      background: #0c0f0e;
      font: inherit;
    }
    .leaflet-control-attribution {
      background: rgba(16, 19, 18, 0.72);
      color: var(--muted);
    }
    .leaflet-control-attribution a {
      color: var(--focus);
    }
    @media (max-width: 1000px) {
      .topbar {
        grid-template-columns: 1fr 1fr;
      }
      .brand {
        grid-column: 1 / -1;
      }
      .shell {
        grid-template-columns: 1fr;
        grid-template-rows: minmax(420px, 56vh) minmax(320px, 1fr);
      }
      .panel {
        border-left: 0;
        border-top: 1px solid var(--line);
      }
    }
    @media (max-width: 620px) {
      .topbar {
        grid-template-columns: 1fr;
      }
      .map-status {
        flex-direction: column;
        align-items: stretch;
      }
      .coord-pill {
        margin-left: 0;
      }
    }
  </style>
</head>
<body>
  <div class="app">
    <header class="topbar">
      <div class="brand">
        <strong>Radar QC</strong>
        <span>Level-II PNG + sidecar</span>
      </div>
      <div class="field">
        <label for="layerSelect">Layer</label>
        <select id="layerSelect"></select>
      </div>
      <div class="field">
        <label for="frameSelect">Frame</label>
        <select id="frameSelect"></select>
      </div>
      <div class="field">
        <label for="tiltSelect">Tilt</label>
        <select id="tiltSelect"></select>
      </div>
      <div class="field">
        <span class="control-label">Sample</span>
        <div class="segmented" role="group" aria-label="Sample method">
          <button type="button" class="active" data-method="nearest">Nearest</button>
          <button type="button" data-method="interpolated">Interp</button>
        </div>
      </div>
      <div class="field">
        <span class="control-label">Tiles</span>
        <div style="display:flex;gap:8px;align-items:center;">
          <input id="opacity" title="Radar tile opacity" type="range" min="0" max="1" step="0.05" value="0.88" />
          <button id="reload" type="button" class="icon-button" title="Reload radar layers" aria-label="Reload radar layers"><i data-lucide="refresh-cw"></i></button>
        </div>
      </div>
    </header>
    <main class="shell">
      <section class="map-wrap">
        <div id="map"></div>
        <div class="map-status">
          <div id="status" class="status-pill">loading</div>
          <div id="coords" class="coord-pill">--</div>
        </div>
      </section>
      <aside class="panel">
        <section class="panel-section">
          <h2>Sample</h2>
          <div id="samplePanel">
            <div class="sample-value"><strong>--</strong><span></span></div>
            <div class="kv"><div>Status</div><div>No sample</div></div>
          </div>
        </section>
        <section class="panel-section">
          <h2>Frame</h2>
          <div id="frameMeta" class="kv"></div>
        </section>
        <section class="panel-section">
          <h2>QC</h2>
          <div id="qcPanel" class="qc-grid"></div>
        </section>
        <section class="panel-section">
          <h2>Provenance</h2>
          <div id="provenancePanel" class="kv"></div>
        </section>
        <section class="panel-section">
          <h2>Raw Sample</h2>
          <pre id="rawSample" class="raw-json">{}</pre>
        </section>
      </aside>
    </main>
  </div>
  <script>
    const els = {};
    const state = {
      layers: [],
      frames: [],
      layer: null,
      frame: null,
      method: "nearest",
      map: null,
      baseLayer: null,
      radarLayer: null,
      sampleMarker: null,
      sampleSeq: 0
    };

    function el(id) {
      return document.getElementById(id);
    }

    function esc(value) {
      return String(value ?? "").replace(/[&<>"']/g, ch => ({
        "&": "&amp;",
        "<": "&lt;",
        ">": "&gt;",
        '"': "&quot;",
        "'": "&#39;"
      })[ch]);
    }

    function fmt(value, digits = 2) {
      const number = Number(value);
      if (!Number.isFinite(number)) return "--";
      return number.toLocaleString(undefined, { maximumFractionDigits: digits });
    }

    function fmtTime(value) {
      if (!value) return "--";
      const date = new Date(value);
      if (Number.isNaN(date.getTime())) return String(value);
      return date.toISOString().replace(".000Z", "Z");
    }

    function setStatus(message) {
      els.status.textContent = message || "";
    }

    function setCoords(latlng) {
      els.coords.textContent = `${fmt(latlng.lat, 5)}, ${fmt(latlng.lng, 5)}`;
    }

    async function fetchJson(url) {
      const response = await fetch(url, { cache: "no-store" });
      const text = await response.text();
      let data = {};
      if (text) {
        try {
          data = JSON.parse(text);
        } catch (err) {
          throw new Error(`Invalid JSON from ${url}`);
        }
      }
      if (!response.ok) {
        throw new Error(data.message || data.reason || data.error || `${response.status} ${response.statusText}`);
      }
      return data;
    }

    function layerLabel(layer) {
      const latest = layer.latest || {};
      const bits = [latest.site, latest.product, layer.id].filter(Boolean);
      return bits.length ? bits.join(" / ") : layer.id;
    }

    function frameLabel(frame) {
      return frame.label || [frame.site, (frame.product || "").toUpperCase(), fmtTime(frame.scan_time_utc)].filter(Boolean).join(" ");
    }

    function tiltLabel(tilt) {
      const elevation = tilt.elevation_deg == null ? "" : `${fmt(tilt.elevation_deg, 2)} deg`;
      return [tilt.id, elevation].filter(Boolean).join(" / ");
    }

    function hasSidecar(item) {
      return Boolean(item && (item.numeric_sidecar || item.numeric_sidecar_url));
    }

    function currentTilt() {
      const tiltId = els.tiltSelect.value;
      if (!tiltId || !state.frame) return null;
      return (state.frame.tilts || []).find(tilt => tilt.id === tiltId) || null;
    }

    function currentAsset() {
      return currentTilt() || state.frame || {};
    }

    function templateFrom(item) {
      if (!item) return "";
      const template = item.tile_url_template || item.url_template || "";
      if (!template) return "";
      if (template.startsWith("/v1/radar/tiles/")) return template;
      return `/v1/radar/tiles/${template.replace(/^\/+/, "")}`;
    }

    function populateSelect(select, items, selectedValue, labeler) {
      select.innerHTML = "";
      for (const item of items) {
        const option = document.createElement("option");
        option.value = item.id;
        option.textContent = labeler(item);
        select.appendChild(option);
      }
      if (items.some(item => item.id === selectedValue)) {
        select.value = selectedValue;
      } else {
        select.value = "";
      }
    }

    async function loadLayers(preserve = true) {
      const previous = preserve ? els.layerSelect.value : "";
      setStatus("loading layers");
      const data = await fetchJson("/v1/radar/layers");
      state.layers = data.layers || [];
      populateSelect(els.layerSelect, state.layers, previous, layerLabel);
      if (!state.layers.length) {
        state.layer = null;
        state.frame = null;
        setStatus("no radar layers");
        renderEmpty();
        return;
      }
      if (!els.layerSelect.value) {
        const preferred = state.layers.find(layer => /ktlx.*vel|vel.*ktlx/i.test(layer.id) && hasSidecar(layer.latest))
          || state.layers.find(layer => /ktlx/i.test(layer.id) && hasSidecar(layer.latest))
          || state.layers.find(layer => /vel/i.test(layer.id) && hasSidecar(layer.latest))
          || state.layers.find(layer => hasSidecar(layer.latest))
          || state.layers[state.layers.length - 1];
        els.layerSelect.value = preferred.id;
      }
      await selectLayer(true);
    }

    async function selectLayer(preserveFrame = false) {
      state.layer = state.layers.find(layer => layer.id === els.layerSelect.value) || null;
      state.frames = [];
      state.frame = null;
      if (!state.layer) return;
      setStatus("loading frames");
      const framesUrl = state.layer.frames_url || `/v1/radar/layers/${encodeURIComponent(state.layer.id)}/frames.json`;
      const data = await fetchJson(framesUrl);
      state.frames = data.frames || [];
      const previous = preserveFrame ? els.frameSelect.value : "";
      populateSelect(els.frameSelect, state.frames, previous, frameLabel);
      if (!els.frameSelect.value && state.frames.length) {
        els.frameSelect.value = state.frames[state.frames.length - 1].id;
      }
      selectFrame();
    }

    function selectFrame() {
      state.frame = state.frames.find(frame => frame.id === els.frameSelect.value) || null;
      const tilts = (state.frame && state.frame.tilts) || [];
      els.tiltSelect.innerHTML = "";
      const baseOption = document.createElement("option");
      baseOption.value = "";
      baseOption.textContent = "Frame";
      els.tiltSelect.appendChild(baseOption);
      for (const tilt of tilts) {
        const option = document.createElement("option");
        option.value = tilt.id;
        option.textContent = tiltLabel(tilt);
        els.tiltSelect.appendChild(option);
      }
      els.tiltSelect.disabled = tilts.length === 0;
      if (tilts.length) {
        const sidecarTilt = tilts.find(hasSidecar) || tilts[0];
        els.tiltSelect.value = sidecarTilt.id;
      }
      updateRadarLayer();
      renderAll();
    }

    function updateRadarLayer() {
      if (!state.map) return;
      if (state.radarLayer) {
        state.map.removeLayer(state.radarLayer);
        state.radarLayer = null;
      }
      const asset = currentAsset();
      const template = templateFrom(asset);
      if (!template) {
        setStatus("no tile template");
        return;
      }
      const opacity = Number(els.opacity.value || 0.88);
      const nativeMinZoom = Number(asset.minzoom ?? state.frame?.minzoom ?? 0);
      const nativeMaxZoom = Number(asset.maxzoom ?? state.frame?.maxzoom ?? 12);
      state.radarLayer = L.tileLayer(template, {
        minZoom: 0,
        maxZoom: 19,
        minNativeZoom: nativeMinZoom,
        maxNativeZoom: nativeMaxZoom,
        tileSize: 256,
        opacity,
        pane: "radarPane"
      }).addTo(state.map);
      const bounds = asset.bounds || state.frame?.bounds;
      if (Array.isArray(bounds) && bounds.length === 4) {
        const leafletBounds = [[bounds[1], bounds[0]], [bounds[3], bounds[2]]];
        state.map.fitBounds(leafletBounds, {
          padding: [18, 18],
          maxZoom: nativeMaxZoom
        });
      }
      setStatus("ready");
    }

    function renderAll() {
      renderMeta();
      renderQc();
      renderProvenance();
      try {
        if (window.lucide) window.lucide.createIcons();
      } catch (err) {}
    }

    function renderEmpty() {
      els.frameMeta.innerHTML = "<div>Status</div><div>No configured radar tile root</div>";
      els.qcPanel.innerHTML = "";
      els.provenancePanel.innerHTML = "";
    }

    function row(label, value) {
      return `<div>${esc(label)}</div><div>${esc(value ?? "--")}</div>`;
    }

    function renderMeta() {
      if (!state.frame) {
        renderEmpty();
        return;
      }
      const asset = currentAsset();
      const sidecar = asset.numeric_sidecar || state.frame.numeric_sidecar || null;
      const sidecarState = sidecar ? `${sidecar.schema || "sidecar"} / ${sidecar.processing_state || "--"}` : "none";
      const clipToBounds = asset.clip_to_bounds ?? state.frame.clip_to_bounds ?? false;
      els.frameMeta.innerHTML = [
        row("Layer", state.layer?.id),
        row("Site", state.frame.site),
        row("Product", state.frame.product),
        row("Scan", fmtTime(state.frame.scan_time_utc)),
        row("Sweep", asset.sweep_index ?? state.frame.sweep_index),
        row("Elevation", asset.elevation_deg == null ? "--" : `${fmt(asset.elevation_deg, 2)} deg`),
        row("Tiles", asset.tile_count ?? state.frame.tile_count),
        row("Zoom", `${asset.minzoom ?? state.frame.minzoom ?? "--"}-${asset.maxzoom ?? state.frame.maxzoom ?? "--"}`),
        row("Native gate", state.frame.native_gate_size_m == null ? "--" : `${fmt(state.frame.native_gate_size_m, 0)} m`),
        row("Az spacing", state.frame.native_azimuth_spacing_deg == null ? "--" : `${fmt(state.frame.native_azimuth_spacing_deg, 4)} deg`),
        row("Bounds clip", clipToBounds ? "on" : "off"),
        row("Color table", asset.color_table || state.frame.color_table),
        row("Sidecar", sidecarState)
      ].join("");
    }

    function qcCard(title, rows, tone = "") {
      if (!rows.length) return "";
      const className = tone ? `qc-card ${tone}` : "qc-card";
      return `<div class="${className}"><strong>${esc(title)}</strong><div class="kv">${rows.join("")}</div></div>`;
    }

    function renderQc() {
      if (!state.frame) {
        els.qcPanel.innerHTML = "";
        return;
      }
      const asset = currentAsset();
      const cards = [];
      const productQc = asset.product_qc || state.frame.product_qc;
      if (productQc) {
        cards.push(qcCard("Product", [
          row("finite gates", productQc.finite_gate_count),
          row("min", fmt(productQc.min_value, 2)),
          row("max", fmt(productQc.max_value, 2)),
          row("mean", fmt(productQc.mean_value, 2))
        ]));
      }
      const dealias = asset.dealias_qc || state.frame.dealias_qc;
      if (dealias) {
        cards.push(qcCard("Dealias", [
          row("decision", dealias.decision),
          row("nyquist", dealias.nyquist_ms == null ? "--" : `${fmt(dealias.nyquist_ms, 2)} m/s`),
          row("changed gates", dealias.changed_gate_count),
          row("original severe", dealias.original_score?.severe_jumps ?? dealias.original_severe_jumps),
          row("candidate severe", dealias.candidate_score?.severe_jumps ?? dealias.candidate_severe_jumps)
        ], dealias.accepted === false ? "bad" : ""));
      }
      const velocityQc = asset.velocity_qc || state.frame.velocity_qc;
      if (velocityQc) {
        const severe = Number(velocityQc.severe_jump_count || 0);
        cards.push(qcCard("Velocity", [
          row("finite gates", velocityQc.finite_gate_count),
          row("fold jumps", velocityQc.fold_like_jump_count),
          row("fold fraction", fmt(velocityQc.fold_like_jump_fraction, 5)),
          row("severe jumps", velocityQc.severe_jump_count),
          row("max jump", velocityQc.max_abs_jump_ms == null ? "--" : `${fmt(velocityQc.max_abs_jump_ms, 2)} m/s`)
        ], severe > 200 ? "warn" : ""));
      }
      const velocityFilter = asset.velocity_quality_qc || state.frame.velocity_quality_qc;
      if (velocityFilter) {
        cards.push(qcCard("Velocity Filter", [
          row("finite gates", velocityFilter.finite_gate_count),
          row("masked gates", velocityFilter.masked_gate_count),
          row("masked fraction", fmt(velocityFilter.masked_gate_fraction, 5))
        ]));
      }
      const reflectivity = asset.reflectivity_qc || state.frame.reflectivity_qc;
      if (reflectivity) {
        cards.push(qcCard("Reflectivity", Object.entries(reflectivity).map(([key, value]) => row(key, typeof value === "object" ? JSON.stringify(value) : value))));
      }
      els.qcPanel.innerHTML = cards.join("") || "<div class=\"kv\"><div>Status</div><div>No QC block</div></div>";
    }

    function renderProvenance() {
      if (!state.frame) {
        els.provenancePanel.innerHTML = "";
        return;
      }
      const asset = currentAsset();
      const provenance = asset.product_provenance || state.frame.product_provenance || {};
      const source = asset.source_key_or_url || state.frame.source_key_or_url;
      const sidecarUrl = asset.numeric_sidecar_url || state.frame.numeric_sidecar_url;
      const sidecarLink = sidecarUrl ? `<a href="${esc(sidecarUrl)}" target="_blank" rel="noopener">manifest</a>` : "--";
      els.provenancePanel.innerHTML = [
        row("Source", source),
        row("Product source", provenance.source),
        row("Derived", provenance.derived == null ? "--" : provenance.derived),
        row("Inputs", Array.isArray(provenance.inputs) ? provenance.inputs.join(", ") : provenance.inputs),
        row("Method", provenance.method),
        `<div>Sidecar</div><div>${sidecarLink}</div>`
      ].join("");
    }

    function renderSample(sample) {
      const valueText = sample.value == null ? "missing" : fmt(sample.value, 2);
      const unitText = sample.units || "";
      const stateBadges = ["raw", "dealiased", "filtered", "derived"]
        .filter(key => sample[key])
        .map(key => `<span class="badge good">${esc(key)}</span>`);
      const flagBadges = (sample.gate_flags || []).map(flag => {
        const tone = flag === "valid" ? "good" : flag === "missing" ? "bad" : "warn";
        return `<span class="badge ${tone}">${esc(flag)}</span>`;
      });
      els.samplePanel.innerHTML = `
        <div class="sample-value"><strong>${esc(valueText)}</strong><span>${esc(unitText)}</span></div>
        <div class="kv">
          ${sample.value_label ? row("Class", sample.value_label) : ""}
          ${row("Product", `${sample.product || "--"} / ${sample.product_name || "--"}`)}
          ${row("Scan", fmtTime(sample.scan_time_utc))}
          ${row("Sweep", `${sample.sweep_index ?? "--"} / ${fmt(sample.elevation_deg, 2)} deg`)}
          ${row("Azimuth", `${fmt(sample.azimuth_deg, 2)} deg`)}
          ${row("Range", `${fmt(sample.range_m, 0)} m`)}
          ${row("Gate", `${sample.gate_index ?? "--"} (${fmt(sample.gate_fraction, 2)})`)}
          ${row("Radial", `${sample.radial_index ?? "--"} / ${fmt(sample.radial_azimuth_deg, 2)} deg`)}
          ${row("Spacing", `${fmt(sample.gate_spacing_m, 0)} m / ${fmt(sample.azimuth_spacing_deg, 3)} deg`)}
          ${row("Nyquist", sample.nyquist_velocity_ms == null ? "--" : `${fmt(sample.nyquist_velocity_ms, 2)} m/s`)}
          ${row("Method", sample.method)}
          ${row("Site", `${sample.site?.id || "--"} ${fmt(sample.site?.lat, 4)}, ${fmt(sample.site?.lon, 4)}`)}
        </div>
        <div class="badges">${stateBadges.concat(flagBadges).join("")}</div>
      `;
      els.rawSample.textContent = JSON.stringify(sample, null, 2);
    }

    function renderSampleError(message) {
      els.samplePanel.innerHTML = `
        <div class="sample-value"><strong>--</strong><span></span></div>
        <div class="kv">${row("Status", message)}</div>
      `;
    }

    async function sampleAt(latlng, source) {
      if (!state.layer || !state.frame) return;
      const seq = ++state.sampleSeq;
      setCoords(latlng);
      const params = new URLSearchParams({
        layer: state.layer.id,
        frame: state.frame.id,
        lat: String(latlng.lat),
        lon: String(latlng.lng),
        method: state.method
      });
      if (state.frame.product) params.set("product", state.frame.product);
      const tilt = currentTilt();
      if (tilt) params.set("tilt", tilt.id);
      try {
        const sample = await fetchJson(`/v1/radar/sample?${params.toString()}`);
        if (seq !== state.sampleSeq) return;
        renderSample(sample);
        if (!state.sampleMarker) {
          state.sampleMarker = L.circleMarker(latlng, {
            radius: 5,
            color: "#ffffff",
            weight: 2,
            fillColor: "#77c857",
            fillOpacity: 0.85,
            pane: "markerPane"
          }).addTo(state.map);
        } else {
          state.sampleMarker.setLatLng(latlng);
        }
        setStatus(`${source} sample`);
      } catch (err) {
        if (seq !== state.sampleSeq) return;
        setStatus(err.message || String(err));
        renderSampleError(err.message || String(err));
      }
    }

    function initMap() {
      if (!window.L) {
        setStatus("Leaflet failed to load");
        return;
      }
      state.map = L.map("map", {
        preferCanvas: true,
        zoomControl: false
      }).setView([35.33, -97.28], 7);
      state.map.createPane("radarPane");
      state.map.getPane("radarPane").style.zIndex = 420;
      L.control.zoom({ position: "bottomright" }).addTo(state.map);
      state.baseLayer = L.tileLayer("https://tile.openstreetmap.org/{z}/{x}/{y}.png", {
        maxZoom: 19,
        attribution: "&copy; OpenStreetMap"
      }).addTo(state.map);
      state.map.on("mousemove", event => {
        setCoords(event.latlng);
      });
      state.map.on("contextmenu", event => {
        if (event.originalEvent) event.originalEvent.preventDefault();
        sampleAt(event.latlng, "right-click");
      });
    }

    function bindEvents() {
      els.layerSelect.addEventListener("change", () => selectLayer(false).catch(handleFatal));
      els.frameSelect.addEventListener("change", selectFrame);
      els.tiltSelect.addEventListener("change", () => {
        updateRadarLayer();
        renderAll();
      });
      els.opacity.addEventListener("input", () => {
        if (state.radarLayer) state.radarLayer.setOpacity(Number(els.opacity.value || 0.88));
      });
      els.reload.addEventListener("click", () => loadLayers(true).catch(handleFatal));
      document.querySelectorAll("[data-method]").forEach(button => {
        button.addEventListener("click", () => {
          state.method = button.dataset.method;
          document.querySelectorAll("[data-method]").forEach(other => other.classList.toggle("active", other === button));
        });
      });
    }

    function handleFatal(err) {
      setStatus(err.message || String(err));
      renderSampleError(err.message || String(err));
    }

    async function init() {
      Object.assign(els, {
        layerSelect: el("layerSelect"),
        frameSelect: el("frameSelect"),
        tiltSelect: el("tiltSelect"),
        opacity: el("opacity"),
        reload: el("reload"),
        status: el("status"),
        coords: el("coords"),
        samplePanel: el("samplePanel"),
        frameMeta: el("frameMeta"),
        qcPanel: el("qcPanel"),
        provenancePanel: el("provenancePanel"),
        rawSample: el("rawSample")
      });
      bindEvents();
      initMap();
      try {
        if (window.lucide) window.lucide.createIcons();
      } catch (err) {}
      await loadLayers(false);
    }

    init().catch(handleFatal);
  </script>
</body>
</html>
"####;

const SATELLITE_HTML: &str = r####"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>WxStore Satellite</title>
  <link rel="stylesheet" href="https://unpkg.com/leaflet@1.9.4/dist/leaflet.css" />
  <style>
    * { box-sizing: border-box; }
    html, body { height: 100%; margin: 0; }
    body {
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #0f172a;
      color: #111827;
    }
    #map { width: 100%; height: 100%; background: #111827; }
    .panel {
      position: absolute;
      z-index: 1000;
      left: 12px;
      top: 12px;
      width: min(430px, calc(100vw - 24px));
      display: grid;
      gap: 8px;
      padding: 10px;
      border-radius: 8px;
      background: rgba(255,255,255,.95);
      box-shadow: 0 10px 28px rgba(0,0,0,.24);
    }
    .topline {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
    }
    h1 { margin: 0; font-size: 17px; line-height: 1.2; }
    .navs { display: flex; gap: 6px; }
    a, button {
      display: inline-grid;
      place-items: center;
      min-height: 32px;
      border: 1px solid #111827;
      border-radius: 6px;
      background: #111827;
      color: #fff;
      padding: 0 9px;
      font: inherit;
      font-size: 12px;
      font-weight: 800;
      text-decoration: none;
      cursor: pointer;
    }
    a.secondary, button.secondary { background: #fff; color: #111827; border-color: #cbd5e1; }
    button:disabled { opacity: .55; cursor: default; }
    label {
      display: grid;
      gap: 4px;
      color: #475467;
      font-size: 10px;
      font-weight: 900;
      text-transform: uppercase;
    }
    select, input {
      width: 100%;
      min-height: 34px;
      border: 1px solid #cbd5e1;
      border-radius: 6px;
      background: #fff;
      color: #111827;
      padding: 0 8px;
      font: inherit;
      font-size: 13px;
      text-transform: none;
    }
    input[type="range"] { padding: 0; }
    .row { display: grid; grid-template-columns: 1fr 1fr; gap: 8px; }
    .buttons { display: grid; grid-template-columns: repeat(4, minmax(0,1fr)); gap: 6px; }
    .status {
      min-height: 34px;
      padding: 8px;
      border: 1px solid #e2e8f0;
      border-radius: 7px;
      background: #f8fafc;
      color: #334155;
      font-size: 12px;
      line-height: 1.35;
      overflow-wrap: anywhere;
    }
    .legend {
      position: absolute;
      z-index: 900;
      right: 12px;
      bottom: 12px;
      max-width: min(460px, calc(100vw - 24px));
      padding: 8px 10px;
      border-radius: 7px;
      background: rgba(15,23,42,.86);
      color: #e5e7eb;
      font-size: 12px;
      line-height: 1.35;
      overflow-wrap: anywhere;
    }
    @media (max-width: 720px) {
      .panel { right: 10px; left: 10px; top: 10px; width: auto; max-height: 58dvh; overflow: auto; }
      .topline { align-items: flex-start; flex-direction: column; }
      .row, .buttons { grid-template-columns: 1fr 1fr; }
      .legend { left: 10px; right: 10px; bottom: 10px; max-width: none; }
    }
  </style>
</head>
<body>
  <div id="map"></div>
  <div class="panel">
    <div class="topline">
      <h1>Satellite Tiles</h1>
      <div class="navs">
        <a class="secondary" href="/">Map</a>
        <a class="secondary" href="/plots">Plots</a>
        <a class="secondary" href="/ops">Ops</a>
      </div>
    </div>
    <div class="row">
      <label>Layer<select id="layer"></select></label>
      <label>Frame<select id="frame"></select></label>
    </div>
    <div class="row" id="tiltRow" style="display:none">
      <label>Tilt<select id="tilt"></select></label>
    </div>
    <div class="row">
      <label>Basemap<select id="basemap">
        <option value="osm">OpenStreetMap</option>
        <option value="carto">Light</option>
        <option value="dark">Dark</option>
      </select></label>
      <label>Opacity<input id="opacity" type="range" min="0" max="100" value="90" /></label>
    </div>
    <div class="buttons">
      <button id="prev" class="secondary" type="button">Prev</button>
      <button id="play" type="button">Play</button>
      <button id="next" class="secondary" type="button">Next</button>
      <button id="fit" class="secondary" type="button">Fit</button>
    </div>
    <div class="buttons">
      <button id="refresh" class="secondary" type="button">Refresh</button>
      <button id="latest" class="secondary" type="button">Latest</button>
      <button id="hide" class="secondary" type="button">Hide</button>
      <button id="show" class="secondary" type="button">Show</button>
    </div>
    <div id="status" class="status">Loading satellite layers...</div>
  </div>
  <div id="legend" class="legend">No satellite frame selected.</div>
  <script src="https://unpkg.com/leaflet@1.9.4/dist/leaflet.js"></script>
  <script>
    const els = {
      layer: document.getElementById("layer"),
      frame: document.getElementById("frame"),
      tiltRow: document.getElementById("tiltRow"),
      tilt: document.getElementById("tilt"),
      basemap: document.getElementById("basemap"),
      opacity: document.getElementById("opacity"),
      prev: document.getElementById("prev"),
      play: document.getElementById("play"),
      next: document.getElementById("next"),
      fit: document.getElementById("fit"),
      refresh: document.getElementById("refresh"),
      latest: document.getElementById("latest"),
      hide: document.getElementById("hide"),
      show: document.getElementById("show"),
      status: document.getElementById("status"),
      legend: document.getElementById("legend"),
    };
    const baseDefs = {
      osm: ["https://tile.openstreetmap.org/{z}/{x}/{y}.png", { maxZoom: 19, attribution: "&copy; OpenStreetMap" }],
      carto: ["https://{s}.basemaps.cartocdn.com/light_all/{z}/{x}/{y}{r}.png", { maxZoom: 20, attribution: "&copy; OpenStreetMap &copy; CARTO" }],
      dark: ["https://{s}.basemaps.cartocdn.com/dark_all/{z}/{x}/{y}{r}.png", { maxZoom: 20, attribution: "&copy; OpenStreetMap &copy; CARTO" }],
    };
    let map = L.map("map", { preferCanvas: true, zoomControl: true }).setView([38, -97], 4);
    let baseLayer = null;
    let satLayer = null;
    let layers = [];
    let frames = [];
    let playing = null;

    function setStatus(text) { els.status.textContent = text; }
    function esc(value) {
      return String(value ?? "").replace(/[&<>"']/g, ch => ({ "&":"&amp;", "<":"&lt;", ">":"&gt;", '"':"&quot;", "'":"&#39;" }[ch]));
    }
    async function fetchJson(url) {
      const res = await fetch(url, { cache: "no-store" });
      if (!res.ok) throw new Error(`${res.status} ${await res.text()}`);
      return res.json();
    }
    function setBase(name) {
      if (baseLayer) map.removeLayer(baseLayer);
      const [url, options] = baseDefs[name] || baseDefs.osm;
      baseLayer = L.tileLayer(url, options).addTo(map);
      if (satLayer) satLayer.bringToFront();
    }
    function frameBounds(frame) {
      const b = frame && frame.bounds;
      if (!Array.isArray(b) || b.length !== 4) return null;
      return [[b[1], b[0]], [b[3], b[2]]];
    }
    function selectedFrameIndex() {
      return Math.max(0, frames.findIndex(frame => frame.id === els.frame.value));
    }
    function frameTilts(frame) {
      return Array.isArray(frame && frame.tilts) ? frame.tilts : [];
    }
    function velocityQualityText(source, frame) {
      const enabled = source.velocity_quality_filter ?? frame.velocity_quality_filter;
      if (!enabled) return "";
      const qc = source.velocity_quality_qc || frame.velocity_quality_qc;
      const fraction = qc && Number(qc.masked_gate_fraction);
      if (Number.isFinite(fraction)) return ` | velocity QC ${(fraction * 100).toFixed(1)}% masked`;
      return " | velocity QC";
    }
    function syncTiltOptions(frame) {
      const tilts = frameTilts(frame);
      if (!tilts.length) {
        els.tiltRow.style.display = "none";
        els.tilt.innerHTML = "";
        return null;
      }
      const previous = els.tilt.value;
      els.tiltRow.style.display = "";
      els.tilt.innerHTML = tilts.map(tilt => {
        const label = tilt.name || `sweep ${tilt.sweep_index ?? ""}`;
        const elevation = Number.isFinite(Number(tilt.elevation_deg)) ? ` ${Number(tilt.elevation_deg).toFixed(2)} deg` : "";
        return `<option value="${esc(tilt.id)}">${esc(label)}${esc(elevation)}</option>`;
      }).join("");
      if (tilts.some(tilt => tilt.id === previous)) els.tilt.value = previous;
      else els.tilt.value = tilts[0].id;
      return tilts.find(tilt => tilt.id === els.tilt.value) || tilts[0];
    }
    function renderFrame(index = selectedFrameIndex(), fit = false) {
      if (!frames.length) {
        if (satLayer) map.removeLayer(satLayer);
        satLayer = null;
        els.tiltRow.style.display = "none";
        els.legend.textContent = "No satellite frames are available.";
        return;
      }
      const frame = frames[Math.max(0, Math.min(index, frames.length - 1))];
      els.frame.value = frame.id;
      const tilt = syncTiltOptions(frame);
      const source = tilt || frame;
      if (satLayer) map.removeLayer(satLayer);
      const opacity = Number(els.opacity.value || 90) / 100;
      satLayer = L.tileLayer(source.tile_url_template || frame.tile_url_template, {
        opacity,
        minZoom: source.minzoom || frame.minzoom || 0,
        maxNativeZoom: source.maxzoom || frame.maxzoom || 9,
        maxZoom: Math.max(12, source.maxzoom || frame.maxzoom || 9),
        pane: "tilePane",
      }).addTo(map);
      satLayer.bringToFront();
      const bounds = frameBounds(source) || frameBounds(frame);
      if (fit && bounds) map.fitBounds(bounds, { padding: [24, 24] });
      const nativeParts = [];
      if (source.native_gate_size_m) nativeParts.push(`${Number(source.native_gate_size_m).toFixed(0)} m gates`);
      if (source.native_azimuth_spacing_deg) nativeParts.push(`${Number(source.native_azimuth_spacing_deg).toFixed(2)} deg az`);
      if (source.maxzoom_site_meters_per_pixel) nativeParts.push(`${Number(source.maxzoom_site_meters_per_pixel).toFixed(0)} m/px @ z${source.maxzoom || frame.maxzoom || "?"}`);
      const nativeText = nativeParts.length ? ` | native ${nativeParts.join(", ")}` : "";
      const velocityText = velocityQualityText(source, frame);
      const tiltText = tilt ? ` | ${tilt.name || tilt.id}` : "";
      els.legend.textContent = `${els.layer.value} | ${frame.scan_time_utc || frame.id}${tiltText} | z${source.minzoom || frame.minzoom}-${source.maxzoom || frame.maxzoom} | ${source.tile_count || frame.tile_count || 0} tiles | ${frame.size_mb || 0} MB${nativeText}${velocityText}`;
      setStatus(`Loaded ${els.layer.value} frame ${frame.label || frame.id}.`);
    }
    async function loadLayers(preserve = true) {
      const previous = preserve ? els.layer.value : "";
      const data = await fetchJson("/v1/satellite/layers");
      layers = data.layers || [];
      els.layer.innerHTML = layers.map(layer => {
        const latest = layer.latest && layer.latest.scan_time_utc ? ` ${layer.latest.scan_time_utc}` : "";
        return `<option value="${esc(layer.id)}">${esc(layer.id)} (${layer.frame_count || 0})${esc(latest)}</option>`;
      }).join("");
      if (layers.some(layer => layer.id === previous)) els.layer.value = previous;
      else if (layers.length) els.layer.value = layers[layers.length - 1].id;
      if (!layers.length) {
        setStatus("No satellite tile layers are published yet.");
        els.legend.textContent = "Run rustwx-runner satellite-run-once or satellite-loop first.";
        return;
      }
      await loadFrames(true);
    }
    async function loadFrames(fit = false) {
      if (!els.layer.value) return;
      const data = await fetchJson(`/v1/satellite/layers/${encodeURIComponent(els.layer.value)}/frames.json`);
      frames = (data.frames || []).slice().sort((a, b) => String(a.scan_time_utc || a.id).localeCompare(String(b.scan_time_utc || b.id)));
      els.frame.innerHTML = frames.map(frame => `<option value="${esc(frame.id)}">${esc(frame.label || frame.scan_time_utc || frame.id)}</option>`).join("");
      if (frames.length) {
        els.frame.value = frames[frames.length - 1].id;
        renderFrame(frames.length - 1, fit);
      } else {
        renderFrame(0, false);
      }
    }
    function step(delta) {
      if (!frames.length) return;
      const next = (selectedFrameIndex() + delta + frames.length) % frames.length;
      renderFrame(next, false);
    }
    function stop() {
      if (playing) clearInterval(playing);
      playing = null;
      els.play.textContent = "Play";
    }
    function togglePlay() {
      if (playing) {
        stop();
        return;
      }
      els.play.textContent = "Pause";
      playing = setInterval(() => step(1), 700);
    }
    setBase("osm");
    els.basemap.addEventListener("change", () => setBase(els.basemap.value));
    els.layer.addEventListener("change", () => { stop(); loadFrames(true).catch(err => setStatus(err.message)); });
    els.frame.addEventListener("change", () => { stop(); renderFrame(selectedFrameIndex(), false); });
    els.tilt.addEventListener("change", () => { stop(); renderFrame(selectedFrameIndex(), false); });
    els.opacity.addEventListener("input", () => { if (satLayer) satLayer.setOpacity(Number(els.opacity.value || 90) / 100); });
    els.prev.addEventListener("click", () => step(-1));
    els.next.addEventListener("click", () => step(1));
    els.play.addEventListener("click", togglePlay);
    els.fit.addEventListener("click", () => renderFrame(selectedFrameIndex(), true));
    els.latest.addEventListener("click", () => renderFrame(frames.length - 1, true));
    els.hide.addEventListener("click", () => { if (satLayer) map.removeLayer(satLayer); satLayer = null; });
    els.show.addEventListener("click", () => renderFrame(selectedFrameIndex(), false));
    els.refresh.addEventListener("click", () => loadLayers(true).catch(err => setStatus(err.message)));
    loadLayers(false).catch(err => {
      setStatus(err.message);
      els.legend.textContent = "Satellite tile root is not available to WxStore.";
    });
  </script>
</body>
</html>"####;

const PLOTS_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>WxStore Plot Loop</title>
  <style>
    :root { color-scheme: light; }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #f6f8fb;
      color: #101828;
    }
    header {
      position: sticky;
      top: 0;
      z-index: 20;
      display: grid;
      grid-template-columns: minmax(180px, 1fr) auto;
      gap: 12px;
      align-items: center;
      padding: 10px 14px;
      border-bottom: 1px solid #d0d5dd;
      background: rgba(246,248,251,0.96);
    }
    h1 { margin: 0; font-size: 18px; line-height: 1.2; }
    .navs { display: flex; gap: 8px; }
    .nav, button {
      display: inline-grid;
      place-items: center;
      min-height: 34px;
      border: 1px solid #111827;
      border-radius: 6px;
      background: #111827;
      color: #fff;
      padding: 0 10px;
      font: inherit;
      font-size: 13px;
      font-weight: 800;
      text-decoration: none;
      cursor: pointer;
    }
    button.secondary, .nav.secondary {
      background: #fff;
      color: #111827;
      border-color: #cbd5e1;
    }
    button:disabled { opacity: 0.55; cursor: default; }
    main {
      display: grid;
      grid-template-columns: 320px minmax(0, 1fr);
      min-height: calc(100vh - 56px);
    }
    aside {
      display: grid;
      align-content: start;
      gap: 10px;
      padding: 12px;
      border-right: 1px solid #d0d5dd;
      background: #fff;
    }
    label {
      display: grid;
      gap: 5px;
      min-width: 0;
      font-size: 11px;
      font-weight: 800;
      color: #475467;
      text-transform: uppercase;
    }
    select, input {
      width: 100%;
      min-height: 34px;
      border: 1px solid #cbd5e1;
      border-radius: 6px;
      background: #fff;
      color: #111827;
      padding: 0 8px;
      font: inherit;
      font-size: 13px;
      text-transform: none;
    }
    .controls {
      display: grid;
      grid-template-columns: 1fr 1fr 1fr;
      gap: 8px;
    }
    .export-row {
      display: grid;
      gap: 6px;
    }
    .load-row {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 8px;
    }
    .quick-panel {
      display: grid;
      gap: 8px;
      padding: 9px;
      border: 1px solid #e2e8f0;
      border-radius: 8px;
      background: #f8fafc;
    }
    .quick-title {
      color: #475467;
      font-size: 10px;
      font-weight: 900;
      text-transform: uppercase;
    }
    .chip-row {
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
    }
    .chip {
      min-height: 28px;
      padding: 0 8px;
      border-color: #cbd5e1;
      background: #fff;
      color: #111827;
      font-size: 12px;
    }
    .chip.active {
      border-color: #111827;
      background: #111827;
      color: #fff;
    }
    .meta {
      display: grid;
      grid-template-columns: 1fr 1fr;
      gap: 8px;
    }
    .metric {
      min-width: 0;
      padding: 9px;
      border: 1px solid #e2e8f0;
      border-radius: 8px;
      background: #f8fafc;
    }
    .metric span {
      display: block;
      color: #667085;
      font-size: 10px;
      font-weight: 800;
      text-transform: uppercase;
    }
    .metric strong {
      display: block;
      margin-top: 3px;
      font-size: 17px;
      line-height: 1.1;
      overflow-wrap: anywhere;
    }
    .viewer {
      display: grid;
      grid-template-rows: minmax(0, 1fr) auto;
      min-width: 0;
      min-height: 0;
    }
    .stage {
      position: relative;
      display: grid;
      place-items: center;
      min-width: 0;
      min-height: 0;
      padding: 14px;
      background: #111827;
    }
    .stage img,
    .stage video {
      display: block;
      max-width: 100%;
      max-height: calc(100vh - 176px);
      width: auto;
      height: auto;
      object-fit: contain;
      background: #0b1220;
      box-shadow: 0 12px 30px rgba(0,0,0,0.24);
    }
    .empty {
      width: min(560px, 92vw);
      padding: 18px;
      border: 1px dashed #475467;
      border-radius: 8px;
      color: #e5e7eb;
      background: rgba(15,23,42,0.72);
      text-align: center;
    }
    .badge {
      position: absolute;
      left: 18px;
      top: 18px;
      max-width: calc(100% - 36px);
      padding: 6px 9px;
      border-radius: 6px;
      background: rgba(255,255,255,0.92);
      color: #111827;
      font-size: 12px;
      font-weight: 800;
      overflow-wrap: anywhere;
    }
    .timeline {
      display: grid;
      gap: 8px;
      padding: 10px 12px 12px;
      border-top: 1px solid #d0d5dd;
      background: #fff;
    }
    .hour-row {
      display: flex;
      gap: 6px;
      overflow-x: auto;
      padding-bottom: 2px;
    }
    .hour {
      min-width: 48px;
      height: 32px;
      border: 1px solid #d0d5dd;
      border-radius: 6px;
      background: #fff;
      color: #111827;
      font-size: 12px;
      font-weight: 800;
    }
    .hour.complete { background: #dcfce7; border-color: #86efac; color: #166534; }
    .hour.pending { background: #fff7ed; border-color: #fdba74; color: #9a3412; }
    .hour.blocked, .hour.failed { background: #fee2e2; border-color: #fca5a5; color: #991b1b; }
    .hour.active { outline: 3px solid #2563eb; outline-offset: 1px; }
    .status {
      min-height: 18px;
      color: #475467;
      font-size: 12px;
      overflow-wrap: anywhere;
    }
    .stage-actions {
      position: absolute;
      top: 14px;
      right: 14px;
      z-index: 2;
      display: flex;
      gap: 8px;
    }
    .stage-actions[hidden] { display: none; }
    .stage-action {
      display: inline-flex;
      align-items: center;
      justify-content: center;
      min-height: 34px;
      padding: 0 12px;
      border: 1px solid rgba(255,255,255,.35);
      border-radius: 6px;
      background: rgba(15, 23, 42, .82);
      color: #fff;
      font-size: 12px;
      font-weight: 800;
      text-decoration: none;
    }
    @media (max-width: 980px) {
      main { grid-template-columns: 1fr; }
      aside { border-right: 0; border-bottom: 1px solid #d0d5dd; }
      .stage img, .stage video { max-height: 62vh; }
    }
    @media (max-width: 560px) {
      header { grid-template-columns: 1fr; }
      .meta { grid-template-columns: 1fr; }
    }
  </style>
</head>
<body>
  <header>
    <h1>WxStore Plot Loop</h1>
    <div class="navs">
      <a class="nav secondary" href="/">Map</a>
      <a class="nav secondary" href="/satellite">Satellite</a>
      <a class="nav secondary" href="/ops">Ops</a>
      <button id="refresh" type="button">Refresh</button>
    </div>
  </header>
  <main>
    <aside>
      <label>Model<select id="model"></select></label>
      <label>Run<select id="run"></select></label>
      <label>Domain<select id="domain"></select></label>
      <label id="variantWrap" hidden>Projection<select id="variant"></select></label>
      <label id="ensembleWrap" hidden>GEFS<select id="ensemble"></select></label>
      <label>Product<input id="productSearch" placeholder="filter products" /><select id="product"></select></label>
      <div class="load-row">
        <button id="loadSelection" type="button">Load Now</button>
        <button id="loadLatest" type="button" class="secondary">Latest</button>
      </div>
      <div class="quick-panel">
        <div class="quick-title">Fast Paths</div>
        <div class="chip-row" id="presetButtons"></div>
        <div class="quick-title">Domains</div>
        <div class="chip-row" id="domainButtons"></div>
        <div class="quick-title" id="ensembleButtonsTitle">GEFS</div>
        <div class="chip-row" id="ensembleButtons"></div>
        <div class="quick-title" id="variantButtonsTitle">Projection</div>
        <div class="chip-row" id="variantButtons"></div>
        <div class="quick-title" id="productCategoryButtonsTitle">Product Groups</div>
        <div class="chip-row" id="productCategoryButtons"></div>
        <div class="quick-title" id="productButtonsTitle">Products</div>
        <div class="chip-row" id="productButtons"></div>
      </div>
      <div class="controls">
        <button id="prev" type="button" class="secondary">Prev</button>
        <button id="play" type="button">Play</button>
        <button id="next" type="button" class="secondary">Next</button>
      </div>
      <div class="export-row">
        <button id="exportMp4" type="button" class="secondary">Export MP4</button>
        <div class="status" id="exportStatus"></div>
      </div>
      <div class="meta">
        <div class="metric"><span>Frames</span><strong id="frameCount">0</strong></div>
        <div class="metric"><span>Available</span><strong id="availableCount">0</strong></div>
        <div class="metric"><span>Current</span><strong id="currentHour">-</strong></div>
        <div class="metric"><span>Artifacts</span><strong id="artifactCount">0</strong></div>
      </div>
      <div class="status" id="status">Loading summaries...</div>
    </aside>
    <section class="viewer">
      <div class="stage">
        <div class="badge" id="badge">No frame selected</div>
        <div class="stage-actions" id="stageActions" hidden>
          <a class="stage-action" id="openArtifact" href="" target="_blank" rel="noopener">Open</a>
          <a class="stage-action" id="exportArtifact" href="" download>Export</a>
        </div>
        <img id="plot" alt="" hidden />
        <video id="plotVideo" controls loop muted playsinline hidden></video>
        <div id="empty" class="empty">Loading static plot inventory...</div>
      </div>
      <div class="timeline">
        <div class="hour-row" id="hours"></div>
        <div class="status" id="frameMeta"></div>
      </div>
    </section>
  </main>
  <script>
    const els = {
      model: document.getElementById("model"),
      run: document.getElementById("run"),
      domain: document.getElementById("domain"),
      variant: document.getElementById("variant"),
      variantWrap: document.getElementById("variantWrap"),
      ensemble: document.getElementById("ensemble"),
      ensembleWrap: document.getElementById("ensembleWrap"),
      product: document.getElementById("product"),
      productSearch: document.getElementById("productSearch"),
      loadSelection: document.getElementById("loadSelection"),
      loadLatest: document.getElementById("loadLatest"),
      presetButtons: document.getElementById("presetButtons"),
      domainButtons: document.getElementById("domainButtons"),
      ensembleButtonsTitle: document.getElementById("ensembleButtonsTitle"),
      ensembleButtons: document.getElementById("ensembleButtons"),
      variantButtonsTitle: document.getElementById("variantButtonsTitle"),
      variantButtons: document.getElementById("variantButtons"),
      productCategoryButtonsTitle: document.getElementById("productCategoryButtonsTitle"),
      productCategoryButtons: document.getElementById("productCategoryButtons"),
      productButtonsTitle: document.getElementById("productButtonsTitle"),
      productButtons: document.getElementById("productButtons"),
      refresh: document.getElementById("refresh"),
      prev: document.getElementById("prev"),
      play: document.getElementById("play"),
      next: document.getElementById("next"),
      exportMp4: document.getElementById("exportMp4"),
      exportStatus: document.getElementById("exportStatus"),
      frameCount: document.getElementById("frameCount"),
      availableCount: document.getElementById("availableCount"),
      currentHour: document.getElementById("currentHour"),
      artifactCount: document.getElementById("artifactCount"),
      status: document.getElementById("status"),
      badge: document.getElementById("badge"),
      stageActions: document.getElementById("stageActions"),
      openArtifact: document.getElementById("openArtifact"),
      exportArtifact: document.getElementById("exportArtifact"),
      plot: document.getElementById("plot"),
      video: document.getElementById("plotVideo"),
      empty: document.getElementById("empty"),
      hours: document.getElementById("hours"),
      frameMeta: document.getElementById("frameMeta"),
    };
    let catalog = { manifests: [], summary: {} };
    let series = { manifests: [], products: [], frames: [] };
    let selectedIndex = 0;
    let playTimer = null;
    let refreshTimer = null;
    let loadingSeries = false;
    let followLatestRun = true;
    let stagedDirty = false;
    let loadedAxesKey = "";
    let loadedSelectionKey = "";
    let stagedProductKey = "";
    let loadedProductKey = "";
    let activeProductCategory = "popular";
    const VARIANT_ORDER = ["auto", "geo", "lambert", "albers", "mercator", "robinson"];
    const PRODUCT_CATEGORIES = [
      ["popular", "Popular"],
      ["surface", "Surface"],
      ["upper_air", "Upper Air"],
      ["severe", "Severe"],
      ["precip", "Precip"],
      ["clouds", "Clouds"],
      ["all", "All"],
    ];

    function valueText(value) {
      return value === null || value === undefined ? "" : String(value);
    }

    function modelOf(item) {
      return item.model || (item.run_kind || "unknown").replace(/_non_ecape_hour$/, "");
    }

    function runKey(item) {
      return `${modelOf(item)}|${item.date || ""}|${item.cycle_utc ?? ""}|${item.source || ""}`;
    }

    function ensembleKey(item) {
      return item.ensemble_key || item.member || item.ensemble_stat || "control";
    }

    function ensembleLabel(key) {
      if (key === "control") return "control";
      if (key === "all_members") return "all members";
      if (key === "all_members_gif") return "all members GIF";
      if (key === "all_members_webp") return "all members WebP";
      if (key === "all_members_mp4") return "all members MP4";
      if (/^gep\d\d$|^gec00$/.test(key)) return key;
      return `stat: ${key}`;
    }

    function variantKey(item) {
      return valueText(item.projection_variant || item.plot_variant || item.variant || "auto").trim() || "auto";
    }

    function variantLabel(key) {
      return key === "auto" ? "auto" : key.replaceAll("_", " ");
    }

    function compareVariantKeys(a, b) {
      const ai = VARIANT_ORDER.indexOf(a);
      const bi = VARIANT_ORDER.indexOf(b);
      if (ai !== -1 || bi !== -1) return (ai === -1 ? 999 : ai) - (bi === -1 ? 999 : bi);
      return a.localeCompare(b);
    }

    function runLabelFromKey(key) {
      const [model, date, cycle, source] = key.split("|");
      return `${model} ${date || "unknown"} ${cycle !== "" ? cycle + "z" : ""}${source ? " " + source : ""}`.trim();
    }

    function parseRunKey(key) {
      const [model, date, cycle, source] = key.split("|");
      return {
        model,
        date: Number(date || 0),
        cycle: Number(cycle || -1),
        source: source || "",
      };
    }

    function compareRunKeysDesc(a, b) {
      const left = parseRunKey(a);
      const right = parseRunKey(b);
      if (right.date !== left.date) return right.date - left.date;
      if (right.cycle !== left.cycle) return right.cycle - left.cycle;
      if (left.model !== right.model) return left.model.localeCompare(right.model);
      return left.source.localeCompare(right.source);
    }

    function normalizeProduct(key) {
      return valueText(key).replace(/^(direct|derived|windowed|ensemble|animation|animation_webp|video_mp4):/, "");
    }

    function productLabel(key) {
      return normalizeProduct(key).replaceAll("_", " ");
    }

    function artifactExtension(artifact) {
      const path = valueText(artifact && (artifact.path || artifact.relative_path || artifact.url));
      const match = path.toLowerCase().match(/\.([a-z0-9]+)(?:[?#].*)?$/);
      if (match) return match[1];
      const key = valueText(artifact && artifact.artifact_key);
      if (key.startsWith("animation_webp:")) return "webp";
      if (key.startsWith("animation:")) return "gif";
      if (key.startsWith("video_mp4:")) return "mp4";
      return "png";
    }

    function isVideoExtension(extension) {
      return extension === "mp4" || extension === "webm" || extension === "mov";
    }

    function htmlEscape(value) {
      return valueText(value)
        .replaceAll("&", "&amp;")
        .replaceAll("<", "&lt;")
        .replaceAll(">", "&gt;")
        .replaceAll('"', "&quot;");
    }

    function artifactDownloadName(frame) {
      const path = valueText(frame && frame.artifact && (frame.artifact.path || frame.artifact.relative_path));
      const name = path.split("/").pop();
      if (name) return name;
      const extension = artifactExtension(frame && frame.artifact);
      return `rustwx_${Date.now()}.${extension}`;
    }

    function stateOf(artifact) {
      return valueText(artifact && artifact.state).toLowerCase();
    }

    function isAvailable(artifact) {
      return !!artifact && artifact.exists && !!artifact.url;
    }

    function selectedRunParts() {
      const [model, date, cycle, source] = els.run.value.split("|");
      return { model, date, cycle, source };
    }

    function manifestsForSelection() {
      const key = els.run.value;
      const domain = els.domain.value;
      const ensemble = selectedEnsemble();
      const variant = selectedVariant();
      return catalog.manifests
        .filter(item => runKey(item) === key && item.domain === domain && ensembleKey(item) === ensemble && variantKey(item) === variant)
        .sort((a, b) => (a.forecast_hour ?? 0) - (b.forecast_hour ?? 0));
    }

    function selectedEnsemble() {
      if (els.ensembleWrap.hidden) return "control";
      return els.ensemble.value || "control";
    }

    function selectedVariant() {
      return els.variant.value || "auto";
    }

    function currentAxesKey() {
      return [
        els.run.value,
        els.domain.value,
        selectedEnsemble(),
        selectedVariant(),
      ].join("|");
    }

    function currentSelectionKey() {
      return `${currentAxesKey()}|${stagedProductKey || els.product.value || ""}`;
    }

    function axesAreDirty() {
      return currentAxesKey() !== loadedAxesKey;
    }

    function currentSelectionLabel() {
      const run = els.run.value ? runLabelFromKey(els.run.value) : "no run";
      const parts = [run, els.domain.value || "no domain"];
      const ensemble = selectedEnsemble();
      const variant = selectedVariant();
      if (ensemble !== "control") parts.push(ensembleLabel(ensemble));
      if (variant !== "auto") parts.push(variantLabel(variant));
      if (stagedProductKey || els.product.value) parts.push(productLabel(stagedProductKey || els.product.value));
      return parts.join(" / ");
    }

    function setStatus(text) {
      els.status.textContent = text;
    }

    function updateProductControlState() {
      const staleAxes = axesAreDirty();
      els.product.disabled = staleAxes || !series.products.length;
      els.productSearch.disabled = staleAxes || !series.products.length;
      if (staleAxes) {
        els.product.innerHTML = `<option value="">Load selection first</option>`;
        els.productCategoryButtonsTitle.hidden = true;
        els.productCategoryButtons.hidden = true;
        els.productCategoryButtons.innerHTML = "";
        els.productButtonsTitle.hidden = true;
        els.productButtons.hidden = true;
        els.productButtons.innerHTML = "";
      }
    }

    function stageProduct(productKey, reason = "Product staged") {
      if (!productKey) return;
      stagedProductKey = productKey;
      selectOption(els.product, productKey);
      selectedIndex = 0;
      markSelectionDirty(reason);
    }

    function markSelectionDirty(reason = "Selection changed") {
      stagedDirty = currentSelectionKey() !== loadedSelectionKey;
      els.loadSelection.disabled = !els.run.value || !els.domain.value;
      updateProductControlState();
      if (stagedDirty) {
        stopPlayback();
        els.exportMp4.disabled = true;
        setStatus(`${reason}. Press Load Now for ${currentSelectionLabel()}.`);
      }
      renderQuickButtons();
    }

    function selectOption(select, value) {
      const option = Array.from(select.options).find(item => item.value === value);
      if (!option) return false;
      select.value = value;
      return true;
    }

    function orderedSubset(values, preferred) {
      const seen = new Set();
      const out = [];
      for (const value of preferred) {
        if (values.includes(value) && !seen.has(value)) {
          out.push(value);
          seen.add(value);
        }
      }
      for (const value of values) {
        if (!seen.has(value)) out.push(value);
      }
      return out;
    }

    function productCategory(key) {
      const value = normalizeProduct(key).toLowerCase();
      if (/cape|cin|stp|srh|shear|helicity|lapse|sig_tor|supercell|updraft/.test(value)) return "severe";
      if (/qpf|precip|rain|snow|sleet|ice|reflectivity|refd|cref/.test(value)) return "precip";
      if (/cloud|ceil|visibility|fog|cig/.test(value)) return "clouds";
      if (/850mb|700mb|500mb|300mb|250mb|200mb|mb_|height_winds|temperature_height_winds|rh_height_winds|vorticity|jet/.test(value)) return "upper_air";
      if (/2m|10m|surface|mslp|pressure|dewpoint|relative_humidity|apparent|wind|gust|temperature/.test(value)) return "surface";
      return "popular";
    }

    function productCategoryLabel(key) {
      return PRODUCT_CATEGORIES.find(item => item[0] === key)?.[1] || key.replaceAll("_", " ");
    }

    function preferredProductsForCategory(category, productValues) {
      const preferred = {
        popular: [
          "2m_temperature",
          "temperature_2m",
          "500mb_height_winds",
          "mslp_10m_winds",
          "10m_winds",
          "qpf_total",
          "composite_reflectivity",
          "sbcape",
          "mucape",
          "cloud_cover",
        ],
        surface: [
          "2m_temperature",
          "temperature_2m",
          "2m_dewpoint",
          "2m_relative_humidity",
          "apparent_temperature_2m",
          "10m_winds",
          "10m_wind_gust",
          "mslp_10m_winds",
        ],
        upper_air: [
          "500mb_height_winds",
          "500mb_temperature_height_winds",
          "500mb_rh_height_winds",
          "700mb_height_winds",
          "850mb_height_winds",
          "300mb_height_winds",
          "250mb_height_winds",
          "200mb_height_winds",
        ],
        severe: [
          "sbcape",
          "mucape",
          "mlcape",
          "sbcin",
          "mucin",
          "mlcin",
          "stp",
          "srh_0_1km",
          "srh_0_3km",
          "bulk_shear_0_6km",
        ],
        precip: [
          "qpf_total",
          "qpf_1h",
          "precipitation",
          "snow",
          "composite_reflectivity",
        ],
        clouds: [
          "cloud_cover",
          "total_cloud_cover",
          "low_cloud_cover",
          "mid_cloud_cover",
          "high_cloud_cover",
          "ceiling",
          "visibility",
        ],
      }[category] || [];
      return preferred.filter(key => productValues.includes(key));
    }

    function preferredProductForActiveCategory(products) {
      const productValues = products.map(item => item.key);
      const byKey = key => products.find(item => item.key === key);
      const preferred = preferredProductsForCategory(activeProductCategory, productValues)
        .map(byKey)
        .find(Boolean);
      if (preferred) return preferred;
      if (activeProductCategory !== "all" && activeProductCategory !== "popular") {
        const inCategory = products.find(item => productCategory(item.key) === activeProductCategory);
        if (inCategory) return inCategory;
      }
      return null;
    }

    function setModel(model, preserveDomain = true) {
      const previousDomain = preserveDomain ? els.domain.value : "";
      const previousEnsemble = els.ensemble.value;
      const previousVariant = els.variant.value;
      if (!selectOption(els.model, model)) return false;
      followLatestRun = true;
      populateRuns(false);
      populateDomains(false);
      if (previousDomain) selectOption(els.domain, previousDomain);
      populateEnsembles(false);
      if (previousEnsemble) selectOption(els.ensemble, previousEnsemble);
      populateVariants(false);
      if (previousVariant) selectOption(els.variant, previousVariant);
      return true;
    }

    function renderChipRow(container, values, selected, labelFn, onClick) {
      container.innerHTML = values.map(value => (
        `<button type="button" class="chip ${value === selected ? "active" : ""}" data-value="${htmlEscape(value)}">${htmlEscape(labelFn(value))}</button>`
      )).join("");
      container.querySelectorAll("button[data-value]").forEach(button => {
        button.addEventListener("click", () => onClick(button.dataset.value));
      });
    }

    function applyPreset(kind) {
      if (kind === "gefs_all_members") {
        const wantedDomain = els.domain.value || "global";
        if (!setModel("gefs")) return;
        if (!selectOption(els.domain, wantedDomain)) selectOption(els.domain, "global") || selectOption(els.domain, "conus");
        populateEnsembles(false);
        selectOption(els.ensemble, "all_members");
        populateVariants(false);
        selectOption(els.variant, "auto");
        selectedIndex = 0;
        markSelectionDirty("GEFS all members staged");
        return;
      }
      if (kind === "gefs_control") {
        if (!setModel("gefs")) return;
        populateEnsembles(false);
        selectOption(els.ensemble, "control");
        populateVariants(false);
        selectedIndex = 0;
        markSelectionDirty("GEFS control staged");
        return;
      }
      if (kind === "hrrr_conus") {
        if (!setModel("hrrr", false)) return;
        selectOption(els.domain, "conus");
        populateEnsembles(false);
        populateVariants(false);
        selectedIndex = 0;
        markSelectionDirty("HRRR CONUS staged");
        return;
      }
      if (kind === "gfs_global") {
        if (!setModel("gfs", false)) return;
        selectOption(els.domain, "global");
        populateEnsembles(false);
        populateVariants(false);
        selectedIndex = 0;
        markSelectionDirty("GFS global staged");
        return;
      }
      if (kind === "gfs_conus") {
        if (!setModel("gfs", false)) return;
        selectOption(els.domain, "conus");
        populateEnsembles(false);
        populateVariants(false);
        selectedIndex = 0;
        markSelectionDirty("GFS CONUS staged");
      }
    }

    function renderQuickButtons() {
      if (!els.presetButtons) return;
      const models = Array.from(els.model.options).map(option => option.value);
      const presets = [
        ["gefs_all_members", "GEFS all members", models.includes("gefs")],
        ["gefs_control", "GEFS control", models.includes("gefs")],
        ["hrrr_conus", "HRRR CONUS", models.includes("hrrr")],
        ["gfs_global", "GFS global", models.includes("gfs")],
        ["gfs_conus", "GFS CONUS", models.includes("gfs")],
      ].filter(item => item[2]);
      renderChipRow(els.presetButtons, presets.map(item => item[0]), "", value => presets.find(item => item[0] === value)?.[1] || value, applyPreset);

      const domains = Array.from(els.domain.options).map(option => option.value);
      renderChipRow(
        els.domainButtons,
        orderedSubset(domains, ["conus", "global", "north_america", "europe", "africa", "asia", "australia", "south_america", "antarctica"]).slice(0, 12),
        els.domain.value,
        value => value.replaceAll("_", " "),
        value => {
          selectOption(els.domain, value);
          populateEnsembles(false);
          populateVariants(false);
          selectedIndex = 0;
          markSelectionDirty("Domain staged");
        }
      );

      const ensembles = Array.from(els.ensemble.options).map(option => option.value);
      const showEnsembles = !els.ensembleWrap.hidden && ensembles.length > 1;
      els.ensembleButtonsTitle.hidden = !showEnsembles;
      els.ensembleButtons.hidden = !showEnsembles;
      if (showEnsembles) {
        renderChipRow(
          els.ensembleButtons,
          orderedSubset(ensembles, ["all_members", "control", "gec00"]),
          selectedEnsemble(),
          ensembleLabel,
          value => {
            selectOption(els.ensemble, value);
            populateVariants(false);
            selectedIndex = 0;
            markSelectionDirty("GEFS member view staged");
          }
        );
      } else {
        els.ensembleButtons.innerHTML = "";
      }

      const variants = Array.from(els.variant.options).map(option => option.value);
      const showVariants = !els.variantWrap.hidden && variants.length > 1;
      els.variantButtonsTitle.hidden = !showVariants;
      els.variantButtons.hidden = !showVariants;
      if (showVariants) {
        renderChipRow(
          els.variantButtons,
          orderedSubset(variants, VARIANT_ORDER),
          selectedVariant(),
          variantLabel,
          value => {
            selectOption(els.variant, value);
            selectedIndex = 0;
            markSelectionDirty("Projection staged");
          }
        );
      } else {
        els.variantButtons.innerHTML = "";
      }

      const productValues = axesAreDirty() ? [] : series.products.map(item => item.key);
      const availableCategories = PRODUCT_CATEGORIES
        .map(item => item[0])
        .filter(category => category === "all" || productValues.some(product => productCategory(product) === category) || preferredProductsForCategory(category, productValues).length > 0);
      if (!availableCategories.includes(activeProductCategory)) {
        activeProductCategory = availableCategories.includes("popular") ? "popular" : (availableCategories[0] || "popular");
      }
      const showCategories = productValues.length > 0;
      els.productCategoryButtonsTitle.hidden = !showCategories;
      els.productCategoryButtons.hidden = !showCategories;
      if (showCategories) {
        renderChipRow(
          els.productCategoryButtons,
          availableCategories,
          activeProductCategory,
          productCategoryLabel,
          value => {
            activeProductCategory = value;
            renderQuickButtons();
          }
        );
      } else {
        els.productCategoryButtons.innerHTML = "";
      }
      const categoryProducts = activeProductCategory === "all"
        ? productValues
        : productValues.filter(product => productCategory(product) === activeProductCategory);
      const productButtons = orderedSubset(
        categoryProducts,
        preferredProductsForCategory(activeProductCategory, productValues)
      ).slice(0, activeProductCategory === "all" ? 24 : 18);
      const showProducts = productButtons.length > 0;
      els.productButtonsTitle.hidden = !showProducts;
      els.productButtons.hidden = !showProducts;
      if (showProducts) {
        renderChipRow(
          els.productButtons,
          productButtons,
          stagedProductKey || loadedProductKey || els.product.value,
          productLabel,
          value => stageProduct(value, "Product staged")
        );
      } else {
        els.productButtons.innerHTML = "";
      }
    }

    function populateModels(preserve = true) {
      const previous = preserve ? els.model.value : "";
      const models = Array.from(new Set(catalog.manifests.map(modelOf))).filter(Boolean).sort();
      els.model.innerHTML = models.map(model => `<option value="${model}">${model}</option>`).join("");
      if (models.includes(previous)) {
        els.model.value = previous;
      } else if (models.includes("hrrr")) {
        els.model.value = "hrrr";
      } else if (models.includes("gfs")) {
        els.model.value = "gfs";
      }
    }

    function populateRuns(preserve = true) {
      const previous = preserve ? els.run.value : "";
      const runs = Array.from(new Set(
        catalog.manifests.filter(item => modelOf(item) === els.model.value).map(runKey)
      )).sort(compareRunKeysDesc);
      const latest = runs[0] || "";
      els.run.innerHTML = runs.map(key => `<option value="${key}">${runLabelFromKey(key)}</option>`).join("");
      if (followLatestRun && latest) {
        els.run.value = latest;
      } else if (runs.includes(previous)) {
        els.run.value = previous;
      } else if (latest) {
        els.run.value = latest;
      }
    }

    function populateDomains(preserve = true) {
      const previous = preserve ? els.domain.value : "";
      const domains = Array.from(new Set(
        catalog.manifests.filter(item => runKey(item) === els.run.value).map(item => item.domain).filter(Boolean)
      )).sort();
      els.domain.innerHTML = domains.map(domain => `<option value="${domain}">${domain}</option>`).join("");
      if (domains.includes(previous)) {
        els.domain.value = previous;
      } else if (domains.includes("conus")) {
        els.domain.value = "conus";
      } else if (domains.includes("global")) {
        els.domain.value = "global";
      }
    }

    function populateEnsembles(preserve = true) {
      const previous = preserve ? els.ensemble.value : "";
      const model = els.model.value;
      const run = els.run.value;
      const domain = els.domain.value;
      const keys = Array.from(new Set(
        catalog.manifests
          .filter(item => modelOf(item) === model && runKey(item) === run && item.domain === domain)
          .map(ensembleKey)
      )).filter(Boolean).sort((a, b) => {
        if (a === "control") return -1;
        if (b === "control") return 1;
        const am = /^ge[cp]\d\d$/.test(a);
        const bm = /^ge[cp]\d\d$/.test(b);
        if (am !== bm) return am ? -1 : 1;
        return a.localeCompare(b);
      });
      const show = model === "gefs" && keys.length > 1;
      els.ensembleWrap.hidden = !show;
      els.ensemble.innerHTML = keys.map(key => `<option value="${key}">${ensembleLabel(key)}</option>`).join("");
      if (keys.includes(previous)) {
        els.ensemble.value = previous;
      } else if (keys.includes("control")) {
        els.ensemble.value = "control";
      } else if (keys.length) {
        els.ensemble.value = keys[0];
      }
    }

    function populateVariants(preserve = true) {
      const previous = preserve ? els.variant.value : "";
      const model = els.model.value;
      const run = els.run.value;
      const domain = els.domain.value;
      const ensemble = selectedEnsemble();
      const keys = Array.from(new Set(
        catalog.manifests
          .filter(item => modelOf(item) === model && runKey(item) === run && item.domain === domain && ensembleKey(item) === ensemble)
          .map(variantKey)
      )).filter(Boolean).sort(compareVariantKeys);
      const variants = keys.length ? keys : ["auto"];
      els.variantWrap.hidden = variants.length <= 1;
      els.variant.innerHTML = variants.map(key => `<option value="${key}">${variantLabel(key)}</option>`).join("");
      if (variants.includes(previous)) {
        els.variant.value = previous;
      } else if (variants.includes("auto")) {
        els.variant.value = "auto";
      } else {
        els.variant.value = variants[0];
      }
      renderQuickButtons();
    }

    function filteredProducts() {
      const query = els.productSearch.value.trim().toLowerCase();
      if (!query) return series.products;
      return series.products.filter(item => item.key.toLowerCase().includes(query) || productLabel(item.key).includes(query));
    }

    function populateProducts(preserve = true) {
      const previous = stagedProductKey || (preserve ? els.product.value : "");
      const products = filteredProducts();
      els.product.innerHTML = products.map(item => {
        const suffix = `${item.available}/${item.total}`;
        return `<option value="${item.key}">${productLabel(item.key)} (${suffix})</option>`;
      }).join("");
      if (products.some(item => item.key === previous)) {
        els.product.value = previous;
      } else {
        const preferred = preferredProductForActiveCategory(products)
          || products.find(item => /2m_temperature|temperature_2m/.test(item.key))
          || products.find(item => /composite_reflectivity/.test(item.key))
          || products.find(item => /sbcape|cape/.test(item.key))
          || products.find(item => item.available > 0)
          || products[0];
        if (preferred) els.product.value = preferred.key;
      }
      stagedProductKey = els.product.value || "";
      updateProductControlState();
    }

    function buildProducts(manifests) {
      const byKey = new Map();
      for (const manifest of manifests) {
        for (const artifact of manifest.artifacts || []) {
          const key = normalizeProduct(artifact.artifact_key);
          if (!key) continue;
          const entry = byKey.get(key) || { key, total: 0, available: 0 };
          entry.total += 1;
          if (isAvailable(artifact)) entry.available += 1;
          byKey.set(key, entry);
        }
      }
      return Array.from(byKey.values()).sort((a, b) => {
        if (b.available !== a.available) return b.available - a.available;
        return a.key.localeCompare(b.key);
      });
    }

    function buildFrames() {
      const product = loadedProductKey || els.product.value;
      series.frames = series.manifests
        .map(manifest => {
          const artifact = (manifest.artifacts || []).find(item => normalizeProduct(item.artifact_key) === product);
          return { manifest, artifact, hour: manifest.forecast_hour ?? 0 };
        })
        .filter(frame => frame.artifact)
        .sort((a, b) => a.hour - b.hour);
      if (selectedIndex >= series.frames.length) selectedIndex = Math.max(0, series.frames.length - 1);
    }

    function renderTimeline() {
      els.hours.innerHTML = series.frames.map((frame, index) => {
        const state = stateOf(frame.artifact) || "pending";
        const cls = [isAvailable(frame.artifact) ? "complete" : state, index === selectedIndex ? "active" : ""].join(" ");
        return `<button type="button" class="hour ${cls}" data-index="${index}">f${String(frame.hour).padStart(3, "0")}</button>`;
      }).join("");
    }

    function preloadNeighbor() {
      if (!series.frames.length) return;
      const next = series.frames[(selectedIndex + 1) % series.frames.length];
      if (next && isAvailable(next.artifact)) {
        if (isVideoExtension(artifactExtension(next.artifact))) return;
        const img = new Image();
        img.src = next.artifact.url;
      }
    }

    function renderFrame() {
      buildFrames();
      renderTimeline();
      const frame = series.frames[selectedIndex];
      const available = series.frames.filter(item => isAvailable(item.artifact)).length;
      els.frameCount.textContent = String(series.frames.length);
      els.availableCount.textContent = String(available);
      els.artifactCount.textContent = String((series.manifests || []).reduce((sum, item) => sum + ((item.artifacts || []).length), 0));
      els.prev.disabled = series.frames.length < 2;
      els.next.disabled = series.frames.length < 2;
      els.play.disabled = series.frames.length < 2;
      els.exportMp4.disabled = series.frames.filter(item => isAvailable(item.artifact) && !isVideoExtension(artifactExtension(item.artifact))).length < 2;
      if (!frame) {
        els.plot.hidden = true;
        els.video.hidden = true;
        els.video.pause();
        els.empty.hidden = false;
        els.empty.textContent = "No frames are available for this product yet.";
        els.badge.textContent = "No frame selected";
        els.currentHour.textContent = "-";
        els.frameMeta.textContent = "";
        els.stageActions.hidden = true;
        return;
      }
      const state = stateOf(frame.artifact);
      const label = `${modelOf(frame.manifest)} ${frame.manifest.date || ""} ${frame.manifest.cycle_utc ?? ""}z ${frame.manifest.domain || ""} ${productLabel(loadedProductKey || els.product.value)} f${String(frame.hour).padStart(3, "0")}`;
      const ensemble = ensembleKey(frame.manifest);
      const variant = variantKey(frame.manifest);
      const badges = [label];
      if (ensemble !== "control") badges.push(ensembleLabel(ensemble));
      if (variant !== "auto") badges.push(variantLabel(variant));
      els.badge.textContent = badges.join(" ");
      els.currentHour.textContent = `f${String(frame.hour).padStart(3, "0")}`;
      const variantMeta = variant === "auto" ? "" : ` | ${variantLabel(variant)}`;
      els.frameMeta.textContent = `${frame.manifest.run_label || frame.manifest.manifest_id} | ${state || "unknown"}${variantMeta} | ${frame.artifact.path || frame.artifact.relative_path || ""}`;
      if (isAvailable(frame.artifact)) {
        const extension = artifactExtension(frame.artifact);
        els.empty.hidden = true;
        els.stageActions.hidden = false;
        els.openArtifact.href = frame.artifact.url;
        els.exportArtifact.href = frame.artifact.url;
        els.exportArtifact.download = artifactDownloadName(frame);
        els.exportArtifact.textContent = `Export ${extension.toUpperCase()}`;
        if (isVideoExtension(extension)) {
          els.plot.hidden = true;
          if (els.plot.src) els.plot.removeAttribute("src");
          els.video.hidden = false;
          const absoluteUrl = new URL(frame.artifact.url, location.href).href;
          if (els.video.src !== absoluteUrl) {
            els.video.src = frame.artifact.url;
            els.video.load();
          }
        } else {
          els.video.hidden = true;
          els.video.pause();
          if (els.video.src) els.video.removeAttribute("src");
          els.plot.hidden = false;
          if (els.plot.src !== new URL(frame.artifact.url, location.href).href) {
            els.plot.src = frame.artifact.url;
          }
        }
      } else {
        els.plot.hidden = true;
        els.video.hidden = true;
        els.video.pause();
        els.empty.hidden = false;
        els.stageActions.hidden = true;
        els.empty.textContent = `${state || "pending"}: ${frame.artifact.detail || "the plot is not complete yet"}`;
      }
      preloadNeighbor();
    }

    async function exportCurrentMp4() {
      const imageFrames = series.frames.filter(item => isAvailable(item.artifact) && !isVideoExtension(artifactExtension(item.artifact)));
      if (imageFrames.length < 2) {
        els.exportStatus.textContent = "Need at least two completed image frames.";
        return;
      }
      const parts = selectedRunParts();
      const params = new URLSearchParams({
        model: parts.model,
        date: parts.date,
        cycle_utc: parts.cycle,
        domain: els.domain.value,
        ensemble: selectedEnsemble(),
        projection: selectedVariant(),
        product: loadedProductKey || els.product.value,
        fps: "2",
        crf: "18",
        preset: "faster",
      });
      if (parts.source) params.set("source", parts.source);
      els.exportMp4.disabled = true;
      els.exportStatus.textContent = `Building MP4 from ${imageFrames.length} frames...`;
      try {
        const res = await fetch(`/v1/static-plots/export-mp4?${params.toString()}`, {
          method: "POST",
          cache: "no-store",
        });
        const data = await res.json();
        if (!res.ok) throw new Error(data.message || data.reason || `MP4 export failed: ${res.status}`);
        els.exportStatus.innerHTML = `<a href="${htmlEscape(data.url)}" download>Download MP4</a> | ${data.frame_count} frames${data.rebuilt ? "" : " cached"}`;
      } catch (err) {
        els.exportStatus.textContent = err.message || String(err);
      } finally {
        els.exportMp4.disabled = false;
      }
    }

    function step(delta) {
      if (!series.frames.length) return;
      selectedIndex = (selectedIndex + delta + series.frames.length) % series.frames.length;
      renderFrame();
    }

    function stopPlayback() {
      if (playTimer) {
        clearInterval(playTimer);
        playTimer = null;
      }
      els.play.textContent = "Play";
    }

    function togglePlayback() {
      if (playTimer) {
        stopPlayback();
        return;
      }
      els.play.textContent = "Pause";
      playTimer = setInterval(() => step(1), 850);
    }

    async function fetchCatalog(preserve = true) {
      const catalogParams = new URLSearchParams({
        include_artifacts: "false",
        catalog_index: "true",
        state: "all",
        manifest_limit: "20000",
        _: String(Date.now()),
      });
      const res = await fetch(`/v1/static-plots?${catalogParams.toString()}`, { cache: "no-store" });
      if (!res.ok) throw new Error(`static plot catalog failed: ${res.status}`);
      const previousRun = els.run.value;
      catalog = await res.json();
      populateModels(preserve);
      populateRuns(preserve);
      populateDomains(preserve);
      populateEnsembles(preserve);
      populateVariants(preserve);
      if (els.run.value !== previousRun) selectedIndex = 0;
    }

    async function fetchSeries(preserveProduct = true) {
      if (!els.run.value || !els.domain.value || loadingSeries) return;
      loadingSeries = true;
      els.loadSelection.disabled = true;
      setStatus(`Loading ${currentSelectionLabel()}...`);
      try {
        const parts = selectedRunParts();
        const params = new URLSearchParams({
          include_artifacts: "true",
          model: parts.model,
          date: parts.date,
          cycle_utc: parts.cycle,
          domain: els.domain.value,
          ensemble: selectedEnsemble(),
          projection: selectedVariant(),
          state: "all",
          manifest_limit: "20000",
          artifact_limit: "1000",
          _: String(Date.now()),
        });
        if (parts.source) params.set("source", parts.source);
        const res = await fetch(`/v1/static-plots?${params.toString()}`, { cache: "no-store" });
        if (!res.ok) throw new Error(`static plot frames failed: ${res.status}`);
        const data = await res.json();
        series.manifests = (data.manifests || []).sort((a, b) => (a.forecast_hour ?? 0) - (b.forecast_hour ?? 0));
        series.products = buildProducts(series.manifests);
        loadedAxesKey = currentAxesKey();
        populateProducts(preserveProduct);
        loadedProductKey = els.product.value || stagedProductKey || loadedProductKey;
        stagedProductKey = loadedProductKey;
        selectedIndex = Math.min(selectedIndex, Math.max(0, series.manifests.length - 1));
        loadedSelectionKey = currentSelectionKey();
        stagedDirty = false;
        renderFrame();
        renderQuickButtons();
        const complete = series.frames.filter(frame => isAvailable(frame.artifact)).length;
        const variant = selectedVariant();
        const variantText = variant === "auto" ? "" : ` / ${variantLabel(variant)}`;
        setStatus(`${series.manifests.length} hours loaded for ${runLabelFromKey(els.run.value)} / ${els.domain.value}${variantText}; ${complete}/${series.frames.length} selected-product frames complete.`);
      } finally {
        loadingSeries = false;
        els.loadSelection.disabled = !els.run.value || !els.domain.value;
      }
    }

    async function loadSelectionNow(preserveProduct = true) {
      stopPlayback();
      selectedIndex = 0;
      await fetchSeries(preserveProduct);
    }

    async function reloadAll(preserve = true, loadCurrent = true) {
      stopPlayback();
      setStatus("Refreshing plot inventory...");
      await fetchCatalog(preserve);
      if (loadCurrent && !stagedDirty) {
        await fetchSeries(preserve);
      } else {
        markSelectionDirty("Inventory refreshed");
      }
    }

    els.model.addEventListener("change", () => {
      stopPlayback();
      followLatestRun = true;
      populateRuns(false);
      populateDomains(false);
      populateEnsembles(false);
      populateVariants(false);
      selectedIndex = 0;
      markSelectionDirty("Model staged");
    });
    els.run.addEventListener("change", () => {
      stopPlayback();
      followLatestRun = false;
      populateDomains(false);
      populateEnsembles(false);
      populateVariants(false);
      selectedIndex = 0;
      markSelectionDirty("Run staged");
    });
    els.domain.addEventListener("change", () => {
      stopPlayback();
      populateEnsembles(false);
      populateVariants(false);
      selectedIndex = 0;
      markSelectionDirty("Domain staged");
    });
    els.ensemble.addEventListener("change", () => {
      stopPlayback();
      populateVariants(false);
      selectedIndex = 0;
      markSelectionDirty("GEFS member view staged");
    });
    els.variant.addEventListener("change", () => {
      stopPlayback();
      selectedIndex = 0;
      markSelectionDirty("Projection staged");
    });
    els.product.addEventListener("change", () => {
      stageProduct(els.product.value, "Product staged");
    });
    els.productSearch.addEventListener("input", () => {
      if (axesAreDirty()) {
        markSelectionDirty("Selection staged");
        return;
      }
      const previous = els.product.value;
      populateProducts(true);
      selectedIndex = 0;
      if (els.product.value !== previous) {
        markSelectionDirty("Product staged");
      } else {
        renderQuickButtons();
      }
    });
    els.prev.addEventListener("click", () => step(-1));
    els.next.addEventListener("click", () => step(1));
    els.play.addEventListener("click", togglePlayback);
    els.exportMp4.addEventListener("click", exportCurrentMp4);
    els.loadSelection.addEventListener("click", () => {
      loadSelectionNow(true).catch(err => setStatus(err.message));
    });
    els.loadLatest.addEventListener("click", () => {
      followLatestRun = true;
      populateRuns(false);
      populateDomains(false);
      populateEnsembles(false);
      populateVariants(false);
      selectedIndex = 0;
      markSelectionDirty("Latest run staged");
    });
    els.refresh.addEventListener("click", () => {
      reloadAll(true, !stagedDirty).catch(err => setStatus(err.message));
    });
    els.hours.addEventListener("click", event => {
      const target = event.target.closest("[data-index]");
      if (!target) return;
      selectedIndex = Number(target.dataset.index);
      renderFrame();
    });
    window.addEventListener("keydown", event => {
      if (event.key === "ArrowLeft") step(-1);
      if (event.key === "ArrowRight") step(1);
      if (event.key === " ") {
        event.preventDefault();
        togglePlayback();
      }
    });

    reloadAll(false).catch(err => setStatus(err.message));
    refreshTimer = setInterval(() => {
      if (stagedDirty || loadedSelectionKey !== currentSelectionKey()) {
        return;
      }
      fetchCatalog(true)
        .then(() => fetchSeries(true))
        .catch(err => setStatus(err.message));
    }, 30000);
  </script>
</body>
</html>
"#;

const OPS_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>WxStore Ops</title>
  <style>
    :root { color-scheme: light; }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: #f8fafc;
      color: #0f172a;
    }
    header {
      position: sticky;
      top: 0;
      z-index: 10;
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 12px;
      padding: 12px 16px;
      border-bottom: 1px solid #cbd5e1;
      background: rgba(248,250,252,0.96);
    }
    h1 { margin: 0; font-size: 18px; line-height: 1.2; }
    nav { display: flex; gap: 8px; }
    nav a {
      display: inline-grid;
      place-items: center;
      height: 34px;
      padding: 0 10px;
      border-radius: 6px;
      background: #0f172a;
      color: #fff;
      text-decoration: none;
      font-weight: 800;
      font-size: 13px;
    }
    main {
      display: grid;
      gap: 12px;
      padding: 14px 16px 28px;
    }
    .metrics {
      display: grid;
      grid-template-columns: repeat(6, minmax(120px, 1fr));
      gap: 8px;
    }
    .metric, .panel {
      min-width: 0;
      padding: 10px;
      border: 1px solid #cbd5e1;
      border-radius: 8px;
      background: #fff;
    }
    .metric span { display: block; color: #64748b; font-size: 11px; font-weight: 800; text-transform: uppercase; }
    .metric strong { display: block; margin-top: 3px; font-size: 20px; line-height: 1.1; }
    .grid { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; }
    h2 { margin: 0 0 8px; font-size: 15px; }
    table { width: 100%; border-collapse: collapse; font-size: 12px; }
    th, td { padding: 6px 7px; border-bottom: 1px solid #e2e8f0; text-align: left; vertical-align: top; }
    th { color: #475569; font-size: 11px; text-transform: uppercase; }
    td { overflow-wrap: anywhere; }
    .ok { color: #166534; font-weight: 800; }
    .bad { color: #991b1b; font-weight: 800; }
    pre {
      margin: 0;
      max-height: 420px;
      overflow: auto;
      font-size: 12px;
      line-height: 1.35;
      white-space: pre-wrap;
    }
    @media (max-width: 1100px) {
      .metrics { grid-template-columns: 1fr 1fr 1fr; }
      .grid { grid-template-columns: 1fr; }
    }
    @media (max-width: 640px) {
      header { align-items: flex-start; flex-direction: column; }
      .metrics { grid-template-columns: 1fr 1fr; }
      main { padding: 10px; }
    }
  </style>
</head>
<body>
  <header>
    <h1>WxStore Ops</h1>
    <nav><a href="/">Map</a><a href="/satellite">Satellite</a><a href="/plots">Plots</a></nav>
  </header>
  <main>
    <section id="metrics" class="metrics"></section>
    <section class="grid">
      <div class="panel"><h2>Active Jobs</h2><div id="jobs"></div></div>
      <div class="panel"><h2>Profile Summary</h2><div id="profileSummary"></div></div>
    </section>
    <section class="panel"><h2>Hour Profile Summary</h2><div id="profileByHour"></div></section>
    <section class="grid">
      <div class="panel"><h2>Slowest Recent Hours</h2><div id="slowestProfiles"></div></div>
      <div class="panel"><h2>Recent Hour Profiles</h2><div id="profiles"></div></div>
    </section>
    <section class="grid">
      <div class="panel"><h2>Top Processes</h2><div id="processes"></div></div>
      <div class="panel"><h2>Storage</h2><div id="storage"></div></div>
    </section>
    <section class="panel"><h2>Raw Snapshot</h2><pre id="raw"></pre></section>
  </main>
  <script>
    const metrics = document.getElementById("metrics");
    const jobs = document.getElementById("jobs");
    const profileSummary = document.getElementById("profileSummary");
    const profileByHour = document.getElementById("profileByHour");
    const slowestProfiles = document.getElementById("slowestProfiles");
    const profiles = document.getElementById("profiles");
    const processes = document.getElementById("processes");
    const storage = document.getElementById("storage");
    const raw = document.getElementById("raw");

    function value(path, fallback = null) {
      return path.reduce((current, key) => current && current[key] !== undefined ? current[key] : fallback, window.snapshot);
    }

    function fmtBytes(bytes) {
      if (bytes === null || bytes === undefined || Number.isNaN(Number(bytes))) return "n/a";
      const units = ["B", "KB", "MB", "GB", "TB"];
      let value = Number(bytes);
      let unit = 0;
      while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit++; }
      return `${value.toFixed(unit ? 1 : 0)} ${units[unit]}`;
    }

    function metric(label, value) {
      return `<div class="metric"><span>${label}</span><strong>${value}</strong></div>`;
    }

    function table(rows, columns) {
      if (!rows || !rows.length) return `<div class="empty">No rows yet.</div>`;
      return `<table><thead><tr>${columns.map(col => `<th>${col.label}</th>`).join("")}</tr></thead><tbody>${rows.map(row => (
        `<tr>${columns.map(col => `<td>${col.render ? col.render(row) : (row[col.key] ?? "")}</td>`).join("")}</tr>`
      )).join("")}</tbody></table>`;
    }

    function render(snapshot) {
      window.snapshot = snapshot;
      const load = value(["loadavg"], {});
      const mem = value(["memory"], {});
      const disk = value(["disk"], {});
      const counts = value(["counts"], {});
      metrics.innerHTML = [
        metric("updated", snapshot.timestamp_utc || "missing"),
        metric("load 1m", load.one || "n/a"),
        metric("cpu cores", snapshot.cpu_count || "n/a"),
        metric("mem avail", fmtBytes(mem.available_bytes)),
        metric("disk free", fmtBytes(disk.available_bytes)),
        metric("plots", counts.static_artifacts || 0),
      ].join("");
      jobs.innerHTML = table(snapshot.jobs || [], [
        {label: "lane", render: r => `${r.model || ""} ${r.kind || ""}`},
        {label: "pid", key: "pid"},
        {label: "age", key: "etime"},
        {label: "cmd", key: "cmd"},
      ]);
      const summary = (snapshot.profile_summary && snapshot.profile_summary.groups) || [];
      profileSummary.innerHTML = table(summary, [
        {label: "lane", render: r => `${r.model || ""} ${r.kind || ""}`},
        {label: "n", key: "count"},
        {label: "avg", render: r => `${r.avg_s ?? ""}s`},
        {label: "p50", render: r => `${r.p50_s ?? ""}s`},
        {label: "p90", render: r => `${r.p90_s ?? ""}s`},
        {label: "max", render: r => `${r.max_s ?? ""}s`},
      ]);
      const hourSummary = (snapshot.profile_summary && snapshot.profile_summary.hour_groups) || [];
      profileByHour.innerHTML = table(hourSummary.slice(0, 160), [
        {label: "lane", render: r => `${r.model || ""} ${r.kind || ""}`},
        {label: "run", render: r => `${r.date || ""} ${r.cycle || ""}z`},
        {label: "hour", render: r => r.hour === undefined ? "" : `f${String(r.hour).padStart(3, "0")}`},
        {label: "domain", key: "domain"},
        {label: "n", key: "count"},
        {label: "latest", render: r => `${r.latest_elapsed_s ?? ""}s`},
        {label: "avg", render: r => `${r.avg_s ?? ""}s`},
        {label: "p90", render: r => `${r.p90_s ?? ""}s`},
        {label: "max", render: r => `${r.max_s ?? ""}s`},
      ]);
      const slowest = (snapshot.profile_summary && snapshot.profile_summary.slowest) || [];
      slowestProfiles.innerHTML = table(slowest.slice(0, 16), [
        {label: "lane", render: r => `${r.model || ""} ${r.kind || ""}`},
        {label: "run", render: r => `${r.date || ""} ${r.cycle || ""}z`},
        {label: "hour", render: r => r.hour === undefined ? "" : `f${String(r.hour).padStart(3, "0")}`},
        {label: "domain", key: "domain"},
        {label: "elapsed", render: r => `${r.elapsed_s ?? ""}s`},
        {label: "rss", render: r => fmtBytes((Number(r.rss_kb || 0) || 0) * 1024)},
      ]);
      profiles.innerHTML = table(snapshot.recent_profiles || [], [
        {label: "lane", render: r => `${r.kind || ""} ${r.model || ""}`},
        {label: "hour", key: "hour"},
        {label: "status", render: r => `<span class="${r.status === "ok" ? "ok" : "bad"}">${r.status || ""}</span>`},
        {label: "elapsed", render: r => r.total_elapsed_s || r.render_elapsed_s || r.export_elapsed_s || ""},
        {label: "rss", render: r => fmtBytes((Number(r.maxrss_kb || r.export_maxrss_kb || 0) || 0) * 1024)},
      ]);
      processes.innerHTML = table(snapshot.processes || [], [
        {label: "pid", key: "pid"},
        {label: "cpu", key: "cpu_pct"},
        {label: "rss", render: r => fmtBytes((Number(r.rss_kb || 0) || 0) * 1024)},
        {label: "age", key: "etime"},
        {label: "cmd", key: "cmd"},
      ]);
      const dirs = value(["dir_sizes"], {});
      storage.innerHTML = table(Object.entries(dirs).map(([name, bytes]) => ({name, bytes})), [
        {label: "dir", key: "name"},
        {label: "size", render: r => fmtBytes(r.bytes)},
      ]);
      raw.textContent = JSON.stringify(snapshot, null, 2);
    }

    async function tick() {
      try {
        const res = await fetch(`/v1/ops/live?t=${Date.now()}`);
        render(await res.json());
      } catch (err) {
        raw.textContent = err.message;
      } finally {
        setTimeout(tick, 2000);
      }
    }
    tick();
  </script>
</body>
</html>
"#;

impl AppState {
    fn cache_get(&self, key: &str) -> Option<Bytes> {
        let Ok(mut cache) = self.cache.write() else {
            return None;
        };
        let value = cache.entries.get(key).cloned();
        if value.is_some() {
            cache.hits = cache.hits.saturating_add(1);
        } else {
            cache.misses = cache.misses.saturating_add(1);
        }
        value
    }

    fn cache_insert(&self, key: String, value: Bytes) {
        let Ok(mut cache) = self.cache.write() else {
            return;
        };
        if let Some(old) = cache.entries.remove(&key) {
            cache.bytes = cache.bytes.saturating_sub(old.len());
        } else {
            cache.order.push_back(key.clone());
        }
        cache.bytes = cache.bytes.saturating_add(value.len());
        cache.entries.insert(key, value);
        while cache.entries.len() > CACHE_LIMIT || cache.bytes > CACHE_BYTES_LIMIT {
            if let Some(oldest) = cache.order.pop_front() {
                if let Some(old) = cache.entries.remove(&oldest) {
                    cache.bytes = cache.bytes.saturating_sub(old.len());
                    cache.evictions = cache.evictions.saturating_add(1);
                }
            } else {
                break;
            }
        }
    }

    fn cache_stats(&self) -> CacheStats {
        let Ok(cache) = self.cache.read() else {
            return CacheStats {
                entries: 0,
                entries_limit: CACHE_LIMIT,
                bytes: 0,
                bytes_limit: CACHE_BYTES_LIMIT,
                hits: 0,
                misses: 0,
                evictions: 0,
            };
        };
        CacheStats {
            entries: cache.entries.len(),
            entries_limit: CACHE_LIMIT,
            bytes: cache.bytes,
            bytes_limit: CACHE_BYTES_LIMIT,
            hits: cache.hits,
            misses: cache.misses,
            evictions: cache.evictions,
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
    variables: Option<String>,
    variable: Option<String>,
    forecast_hours: Option<String>,
    forecast_hour: Option<String>,
    hours: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CrossSectionRenderRequest {
    model: Option<String>,
    run: Option<String>,
    products: Option<String>,
    product: Option<String>,
    hour: Option<u8>,
    hours: Option<String>,
    start_lat: f64,
    start_lon: f64,
    end_lat: f64,
    end_lon: f64,
    route_name: Option<String>,
    spacing_km: Option<f32>,
    top_pressure_hpa: Option<f64>,
    width: Option<u32>,
    height: Option<u32>,
    force: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
struct SoundingRenderRequest {
    model: Option<String>,
    run: Option<String>,
    source: Option<String>,
    hour: Option<u16>,
    forecast_hour: Option<u16>,
    lat: f64,
    lon: f64,
    sample_method: Option<String>,
    crop_radius_deg: Option<f64>,
    box_radius_km: Option<f64>,
    station_id: Option<String>,
    force: Option<bool>,
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

async fn livez() -> Json<Value> {
    Json(json!({
        "schema": "wxstore.health.v1",
        "ok": true,
        "kind": "live",
        "service": "wxstore"
    }))
}

async fn readyz(State(state): State<Arc<AppState>>) -> (StatusCode, Json<Value>) {
    let status = readiness_status(
        state.profile.as_deref(),
        state.diagnostic.as_deref(),
        state.spatial.as_deref(),
        state.static_plots.as_deref(),
        state.evidence.as_deref(),
        state.observations.as_deref(),
        state.mesoanalysis_innovation.as_deref(),
        state.satellite_tiles.as_deref(),
        state.radar_tiles.as_deref(),
    );
    let ok = status.get("ok").and_then(Value::as_bool).unwrap_or(false);
    (
        if ok {
            StatusCode::OK
        } else {
            StatusCode::SERVICE_UNAVAILABLE
        },
        Json(status),
    )
}

async fn status(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(store_status(
        state.profile.as_deref(),
        state.diagnostic.as_deref(),
        state.spatial.as_deref(),
        state.static_plots.as_deref(),
        state.evidence.as_deref(),
        state.observations.as_deref(),
        state.mesoanalysis_innovation.as_deref(),
        state.satellite_tiles.as_deref(),
        state.radar_tiles.as_deref(),
        Some(state.cache_stats()),
        state.archive.as_deref(),
    ))
}

async fn archive_status(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let Some(archive) = state.archive.as_deref() else {
        return Err(not_found("archive root is not configured"));
    };
    Ok(Json(archive.status_json()))
}

async fn archive_events(
    State(state): State<Arc<AppState>>,
    Query(query): Query<ArchiveEventsQuery>,
) -> Result<Json<Value>, ApiError> {
    let Some(archive) = state.archive.as_deref() else {
        return Err(not_found("archive root is not configured"));
    };
    let limit = query.limit.map(|limit| limit.min(5000));
    let events = archive
        .event_summaries(query.rank.as_deref(), limit)
        .map_err(|err| internal_error(err.to_string()))?;
    Ok(Json(json!({
        "schema": "wxstore.archive.events.v1",
        "root": archive.root,
        "rank": query.rank,
        "event_count": events.len(),
        "events": events
    })))
}

async fn archive_event(
    State(state): State<Arc<AppState>>,
    AxumPath(event_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let Some(archive) = state.archive.as_deref() else {
        return Err(not_found("archive root is not configured"));
    };
    archive
        .event_json(&event_id)
        .map(Json)
        .map_err(|err| not_found(err.to_string()))
}

async fn archive_event_runs(
    State(state): State<Arc<AppState>>,
    AxumPath(event_id): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let Some(archive) = state.archive.as_deref() else {
        return Err(not_found("archive root is not configured"));
    };
    archive
        .runs_json(&event_id)
        .map(Json)
        .map_err(|err| not_found(err.to_string()))
}

async fn archive_event_polygons(
    State(state): State<Arc<AppState>>,
    AxumPath(event_id): AxumPath<String>,
) -> Result<Response, ApiError> {
    let Some(archive) = state.archive.as_deref() else {
        return Err(not_found("archive root is not configured"));
    };
    let value = archive
        .polygons_json(&event_id)
        .map_err(|err| not_found(err.to_string()))?;
    let bytes = serde_json::to_vec(&value).map_err(|err| internal_error(err.to_string()))?;
    Ok(bytes_response(
        Bytes::from(bytes),
        "application/geo+json",
        false,
        false,
    ))
}

async fn latest(
    State(state): State<Arc<AppState>>,
    AxumPath((model, domain)): AxumPath<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    if let Some(profile) = state.profile.as_deref() {
        let manifest = &profile.manifest;
        if model == manifest.model && domain == manifest.domain {
            return Ok(Json(json!({
                "schema": "wxstore.latest.v1",
                "model": manifest.model,
                "domain": manifest.domain,
                "run_id": manifest.run_id,
                "cycle": manifest.cycle,
                    "products": {
                        "profile_pressure_core": "ready",
                        "diag_scalar_basic": if state.diagnostic.is_some() { "ready_sparse_v0" } else { "unavailable" },
                        "surface_spatial": if state.spatial.is_some() { "ready" } else { "unavailable" },
                        "static_plots": if state.static_plots.is_some() { "ready" } else { "unavailable" }
                    },
                "canonical_run_url": format!("/v1/runs/{}/{}/{}", manifest.model, manifest.domain, manifest.run_id),
            })));
        }
    }

    if let Some(spatial) = state.spatial.as_deref() {
        if let Some(run) = spatial.latest_run_for_model(&model) {
            return Ok(Json(json!({
                "schema": "wxstore.latest.v1",
                "model": model,
                "domain": domain,
                "run_id": run,
                "products": {
                    "surface_spatial": "ready",
                    "static_plots": if state.static_plots.is_some() { "ready" } else { "unavailable" },
                    "profile_pressure_core": "unavailable",
                    "diag_scalar_basic": "unavailable"
                },
                "readiness": spatial.run_readiness(&model, &run),
                "canonical_run_url": format!("/v1/layers?model={}&run={}", model, run),
            })));
        }
    }

    Err(not_found("model/domain is not loaded on this node"))
}

async fn models(State(state): State<Arc<AppState>>) -> Json<Value> {
    let spatial = state.spatial.as_deref().map(SpatialLane::models_json);
    let static_plots = state
        .static_plots
        .as_deref()
        .map(StaticPlotLane::overview_json);
    let evidence = state
        .evidence
        .as_deref()
        .map(EvidenceBundleLane::summary_json);
    let observations = state
        .observations
        .as_deref()
        .map(ObservationLane::summary_json);
    let profile_loaded = state
        .profile
        .as_deref()
        .map(|profile| {
            json!({
                "model": profile.manifest.model,
                "domain": profile.manifest.domain,
                "run_id": profile.manifest.run_id,
                "products": ["temporal_sounding", "point_bin"]
            })
        })
        .unwrap_or_else(|| json!({"status": "unavailable"}));
    Json(json!({
        "schema": "wxstore.models.v1",
        "profile_loaded": profile_loaded,
        "spatial_loaded": spatial.unwrap_or_else(|| json!({"status": "unavailable"})),
        "static_plots_loaded": static_plots.unwrap_or_else(|| json!({"status": "unavailable"})),
        "evidence_bundles_loaded": evidence.unwrap_or_else(|| json!({"status": "unavailable"})),
        "direct_observations_loaded": observations.unwrap_or_else(|| json!({"status": "unavailable"})),
        "science_engine_scope": {
            "current_profile_lane": ["hrrr"],
            "current_spatial_surface_lanes": state.spatial.as_deref().map(SpatialLane::model_ids).unwrap_or_default(),
            "designed_for": ["hrrr", "gfs", "nam", "rap", "rrfs", "ecmwf_ifs", "ecmwf_ens", "ai_model_outputs"]
        }
    }))
}

async fn weather_objects(
    State(state): State<Arc<AppState>>,
    Query(query): Query<WeatherObjectQuery>,
) -> Result<impl IntoResponse, ApiError> {
    Ok((
        no_store_headers(),
        Json(weather_objects_index(
            &query,
            state.spatial.as_deref(),
            state.static_plots.as_deref(),
            state.evidence.as_deref(),
            state.observations.as_deref(),
            state.satellite_tiles.as_deref(),
            state.radar_tiles.as_deref(),
        )),
    ))
}

async fn static_plots(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StaticPlotCatalogQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(static_plots) = state.static_plots.as_deref() else {
        return Err(not_found("static plots root is not configured"));
    };
    Ok((no_store_headers(), Json(static_plots.catalog_json(&query))))
}

async fn evidence_bundles(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(evidence) = state.evidence.as_deref() else {
        return Err(not_found("evidence root is not configured"));
    };
    Ok((
        no_store_headers(),
        Json(evidence.bundles_json().map_err(bad_anyhow)?),
    ))
}

async fn evidence_bundle(
    State(state): State<Arc<AppState>>,
    AxumPath(bundle_id): AxumPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(evidence) = state.evidence.as_deref() else {
        return Err(not_found("evidence root is not configured"));
    };
    match evidence.bundle_json(&bundle_id) {
        Ok(bundle) => Ok((no_store_headers(), Json(bundle))),
        Err(err) if err.to_string().contains("is missing") => Err(not_found(err.to_string())),
        Err(err) => Err(bad_anyhow(err)),
    }
}

async fn observation_sources(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(observations) = state.observations.as_deref() else {
        return Err(not_found("observations root is not configured"));
    };
    Ok((
        no_store_headers(),
        Json(observations.sources_json().map_err(bad_anyhow)?),
    ))
}

async fn observation_source(
    State(state): State<Arc<AppState>>,
    AxumPath(source_id): AxumPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(observations) = state.observations.as_deref() else {
        return Err(not_found("observations root is not configured"));
    };
    match observations.source_json(&source_id) {
        Ok(source) => Ok((no_store_headers(), Json(source))),
        Err(err) if err.to_string().contains("is missing") => Err(not_found(err.to_string())),
        Err(err) => Err(bad_anyhow(err)),
    }
}

async fn mesoanalysis_innovation_status(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(lane) = state.mesoanalysis_innovation.as_deref() else {
        return Err(not_found(
            "mesoanalysis innovation index root is not configured",
        ));
    };
    Ok((no_store_headers(), Json(lane.summary_json())))
}

async fn mesoanalysis_innovation_query(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MesoanalysisInnovationQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(lane) = state.mesoanalysis_innovation.as_deref() else {
        return Err(not_found(
            "mesoanalysis innovation index root is not configured",
        ));
    };
    Ok((
        no_store_headers(),
        Json(lane.query_json(&query).map_err(bad_anyhow)?),
    ))
}

async fn mesoanalysis_innovation_watchlist(
    State(state): State<Arc<AppState>>,
    Query(query): Query<MesoanalysisInnovationQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(lane) = state.mesoanalysis_innovation.as_deref() else {
        return Err(not_found(
            "mesoanalysis innovation index root is not configured",
        ));
    };
    Ok((
        no_store_headers(),
        Json(lane.watchlist_json(&query).map_err(bad_anyhow)?),
    ))
}

async fn satellite_layers(
    State(state): State<Arc<AppState>>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(lane) = state.satellite_tiles.as_deref() else {
        return Err(not_found("satellite tiles root is not configured"));
    };
    Ok((
        no_store_headers(),
        Json(lane.layers_json().map_err(bad_anyhow)?),
    ))
}

async fn satellite_frames(
    State(state): State<Arc<AppState>>,
    AxumPath(layer_id): AxumPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(lane) = state.satellite_tiles.as_deref() else {
        return Err(not_found("satellite tiles root is not configured"));
    };
    Ok((
        no_store_headers(),
        Json(lane.frames_json(&layer_id).map_err(bad_anyhow)?),
    ))
}

async fn satellite_tile(
    State(state): State<Arc<AppState>>,
    AxumPath((layer_id, frame_id, z, x, tile_file)): AxumPath<(String, String, u8, u32, String)>,
) -> Result<Response, ApiError> {
    let Some(lane) = state.satellite_tiles.as_deref() else {
        return Err(not_found("satellite tiles root is not configured"));
    };
    let y = tile_file
        .strip_suffix(".png")
        .unwrap_or(tile_file.as_str())
        .parse::<u32>()
        .map_err(|_| bad_request("invalid satellite tile y"))?;
    let tile = lane
        .tile_path(&layer_id, &frame_id, z, x, y)
        .map_err(bad_anyhow)?;
    let bytes = fs::read(&tile)
        .map_err(|err| internal_error(format!("read satellite tile {}: {err}", tile.display())))?;
    Ok(bytes_response(Bytes::from(bytes), "image/png", false, true))
}

async fn radar_layers(State(state): State<Arc<AppState>>) -> Result<impl IntoResponse, ApiError> {
    let Some(lane) = state.radar_tiles.as_deref() else {
        return Err(not_found("radar tiles root is not configured"));
    };
    Ok((
        no_store_headers(),
        Json(lane.layers_json().map_err(bad_anyhow)?),
    ))
}

async fn radar_frames(
    State(state): State<Arc<AppState>>,
    AxumPath(layer_id): AxumPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(lane) = state.radar_tiles.as_deref() else {
        return Err(not_found("radar tiles root is not configured"));
    };
    Ok((
        no_store_headers(),
        Json(lane.frames_json(&layer_id).map_err(bad_anyhow)?),
    ))
}

async fn radar_sidecar(
    State(state): State<Arc<AppState>>,
    AxumPath((layer_id, frame_id, sidecar_file)): AxumPath<(String, String, String)>,
) -> Result<Response, ApiError> {
    let Some(lane) = state.radar_tiles.as_deref() else {
        return Err(not_found("radar tiles root is not configured"));
    };
    let sidecar = lane
        .sidecar_path(&layer_id, &frame_id, None, &sidecar_file)
        .map_err(bad_anyhow)?;
    let bytes = fs::read(&sidecar).map_err(|err| {
        internal_error(format!("read radar sidecar {}: {err}", sidecar.display()))
    })?;
    Ok(bytes_response(
        Bytes::from(bytes),
        radar_sidecar_content_type(&sidecar_file),
        false,
        true,
    ))
}

async fn radar_tilt_sidecar(
    State(state): State<Arc<AppState>>,
    AxumPath((layer_id, frame_id, tilt_id, sidecar_file)): AxumPath<(
        String,
        String,
        String,
        String,
    )>,
) -> Result<Response, ApiError> {
    let Some(lane) = state.radar_tiles.as_deref() else {
        return Err(not_found("radar tiles root is not configured"));
    };
    let sidecar = lane
        .sidecar_path(&layer_id, &frame_id, Some(&tilt_id), &sidecar_file)
        .map_err(bad_anyhow)?;
    let bytes = fs::read(&sidecar).map_err(|err| {
        internal_error(format!("read radar sidecar {}: {err}", sidecar.display()))
    })?;
    Ok(bytes_response(
        Bytes::from(bytes),
        radar_sidecar_content_type(&sidecar_file),
        false,
        true,
    ))
}

async fn radar_sample(
    State(state): State<Arc<AppState>>,
    Query(query): Query<RadarSampleQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(lane) = state.radar_tiles.as_deref() else {
        return Err(not_found("radar tiles root is not configured"));
    };
    Ok((
        no_store_headers(),
        Json(lane.sample_json(&query).map_err(bad_anyhow)?),
    ))
}

async fn radar_tile(
    State(state): State<Arc<AppState>>,
    AxumPath((layer_id, frame_id, z, x, tile_file)): AxumPath<(String, String, u8, u32, String)>,
) -> Result<Response, ApiError> {
    let Some(lane) = state.radar_tiles.as_deref() else {
        return Err(not_found("radar tiles root is not configured"));
    };
    let y = tile_file
        .strip_suffix(".png")
        .unwrap_or(tile_file.as_str())
        .parse::<u32>()
        .map_err(|_| bad_request("invalid radar tile y"))?;
    let tile = lane
        .tile_path(&layer_id, &frame_id, None, z, x, y)
        .map_err(bad_anyhow)?;
    let bytes = fs::read(&tile)
        .map_err(|err| internal_error(format!("read radar tile {}: {err}", tile.display())))?;
    Ok(bytes_response(Bytes::from(bytes), "image/png", false, true))
}

async fn radar_tilt_tile(
    State(state): State<Arc<AppState>>,
    AxumPath((layer_id, frame_id, tilt_id, z, x, tile_file)): AxumPath<(
        String,
        String,
        String,
        u8,
        u32,
        String,
    )>,
) -> Result<Response, ApiError> {
    let Some(lane) = state.radar_tiles.as_deref() else {
        return Err(not_found("radar tiles root is not configured"));
    };
    let y = tile_file
        .strip_suffix(".png")
        .unwrap_or(tile_file.as_str())
        .parse::<u32>()
        .map_err(|_| bad_request("invalid radar tile y"))?;
    let tile = lane
        .tile_path(&layer_id, &frame_id, Some(&tilt_id), z, x, y)
        .map_err(bad_anyhow)?;
    let bytes = fs::read(&tile)
        .map_err(|err| internal_error(format!("read radar tile {}: {err}", tile.display())))?;
    Ok(bytes_response(Bytes::from(bytes), "image/png", false, true))
}

async fn static_plots_export_mp4(
    State(state): State<Arc<AppState>>,
    Query(query): Query<StaticPlotMp4ExportQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let Some(static_plots) = state.static_plots.as_ref().cloned() else {
        return Err(not_found("static plots root is not configured"));
    };
    let export = tokio::task::spawn_blocking(move || static_plots.export_mp4(&query))
        .await
        .map_err(|err| internal_error(format!("static plot mp4 export task failed: {err}")))?
        .map_err(bad_anyhow)?;
    let encoded_path = url_encode_query_component(&export.relative_path);
    let mut url = format!("/v1/static-plots/artifacts/export/0?path={encoded_path}");
    if let Some(version) = export.version.as_deref() {
        url.push_str("&v=");
        url.push_str(&url_encode_query_component(version));
    }
    Ok((
        no_store_headers(),
        Json(json!({
            "schema": "wxstore.static_plots.export_mp4.v1",
            "status": "complete",
            "url": url,
            "path": export.relative_path,
            "absolute_path": export.path,
            "version": export.version,
            "frame_count": export.frame_count,
            "forecast_hours": export.forecast_hours,
            "rebuilt": export.rebuilt,
        })),
    ))
}

async fn plot_lab_config(State(state): State<Arc<AppState>>) -> (HeaderMap, Json<Value>) {
    (no_store_headers(), Json(state.plot_lab.config_json()))
}

async fn plot_lab_render(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PlotLabRenderRequest>,
) -> Result<impl IntoResponse, ApiError> {
    let plot_lab = Arc::clone(&state.plot_lab);
    let rendered = tokio::task::spawn_blocking(move || plot_lab.render(request))
        .await
        .map_err(|err| internal_error(format!("plot lab render task failed: {err}")))?
        .map_err(|err| internal_error(err.to_string()))?;
    Ok((no_store_headers(), Json(rendered)))
}

async fn ops_live(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let path = state.ops_root.join("ops").join("live.json");
    match fs::read(&path) {
        Ok(bytes) => {
            let value = serde_json::from_slice(&bytes).map_err(|err| {
                internal_error(format!("parse ops snapshot {}: {err}", path.display()))
            })?;
            Ok(Json(value))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Json(json!({
            "schema": "wxstore.ops.live.v1",
            "status": "missing",
            "path": display_path(&path),
            "message": "ops snapshot writer is not running"
        }))),
        Err(err) => Err(internal_error(format!(
            "read ops snapshot {}: {err}",
            path.display()
        ))),
    }
}

async fn plot_lab_artifact(
    State(state): State<Arc<AppState>>,
    AxumPath((render_id, file_name)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    let artifact = state
        .plot_lab
        .artifact_path(&render_id, &file_name)
        .map_err(bad_anyhow)?;
    let bytes = fs::read(&artifact).map_err(|err| {
        internal_error(format!(
            "read plot lab artifact {}: {err}",
            artifact.display()
        ))
    })?;
    Ok(bytes_response(
        Bytes::from(bytes),
        static_plot_artifact_content_type(&artifact),
        false,
        false,
    ))
}

async fn static_plot_artifact(
    State(state): State<Arc<AppState>>,
    AxumPath((manifest_id, artifact_index)): AxumPath<(String, usize)>,
    Query(query): Query<StaticPlotArtifactQuery>,
) -> Result<Response, ApiError> {
    let Some(static_plots) = state.static_plots.as_deref() else {
        return Err(not_found("static plots root is not configured"));
    };
    let artifact = if let Some(relative_path) = query.path.as_deref() {
        static_plots
            .artifact_relative_path(relative_path)
            .map_err(bad_anyhow)?
    } else {
        static_plots
            .artifact_path(&manifest_id, artifact_index)
            .map_err(bad_anyhow)?
    };
    let immutable = query
        .v
        .as_deref()
        .and_then(|version| {
            static_plot_artifact_version(&artifact).map(|current| current == version)
        })
        .unwrap_or(false);
    let bytes = fs::read(&artifact)
        .map_err(|err| internal_error(format!("read static plot {}: {err}", artifact.display())))?;
    Ok(bytes_response(
        Bytes::from(bytes),
        static_plot_artifact_content_type(&artifact),
        false,
        immutable,
    ))
}

fn static_plot_artifact_content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "jpg" | "jpeg" => "image/jpeg",
        _ => "image/png",
    }
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
    let temporal_sounding = state
        .profile
        .as_deref()
        .map(|profile| {
            json!({
                "model": profile.manifest.model,
                "run_id": profile.manifest.run_id,
                "variables": profile.variable_names(),
                "hours": profile.manifest.forecast_hours,
                "levels_hpa": profile.manifest.levels_hpa
            })
        })
        .unwrap_or_else(|| json!({"status": "unavailable"}));
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
            "temporal_sounding": temporal_sounding
        },
        "rustwx_hrrr_inventory": local_inventory.unwrap_or_else(|| json!({
            "status": "not_generated",
            "command": "cargo run --release -p rustwx-cli --bin hrrr_capability_inventory -- --date 20260429 --forecast-hour 0 --out-dir rustwx-inventory"
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

    if let Some(profile) = state.profile.as_deref() {
        let profile_run_requested = run
            .map(|value| {
                value == "latest"
                    || value == profile.manifest.run_id
                    || value == profile.manifest.cycle
            })
            .unwrap_or(true);
        if model == profile.manifest.model && profile_run_requested && member.is_none() {
            if let Some(grid) = profile.read_pressure_grid_product(variable, forecast_hour)? {
                return Ok(grid);
            }
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

    if format != "bin" && read.values.len() > MAX_GRID_JSON_CELLS {
        return Err(payload_too_large(format!(
            "grid JSON response would contain {} cells; use format=bin or a tile endpoint",
            read.values.len()
        )));
    }

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
    if let Some(profile) = state.profile.as_deref() {
        if model == profile.manifest.model && (run == "latest" || run == profile.manifest.run_id) {
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
                        "model": profile.manifest.model,
                        "run": profile.manifest.run_id,
                        "forecast_hours": profile.manifest.forecast_hours,
                        "pressure_hpa": level,
                        "tilejson": format!("/v1/tilejson/{}/{}/{}", profile.manifest.model, profile.manifest.run_id, variable)
                    }));
                }
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
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath((model, run, variable)): AxumPath<(String, String, String)>,
    Query(query): Query<TileJsonQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = query
        .base_url
        .unwrap_or_else(|| request_base_url(&headers))
        .trim_end_matches('/')
        .to_string();
    let resolved_run = resolve_tilejson_run(&state, &model, &run)?;
    let hour = query.forecast_hour.unwrap_or(0);
    let bounds = read_grid_from_state(
        &state,
        &model,
        Some(&resolved_run),
        query.member.as_deref(),
        &variable,
        hour,
    )
    .ok()
    .map(|grid| grid_bounds(&grid))
    .unwrap_or([-180.0, -85.05112878, 180.0, 85.05112878]);
    let model_path = url_path_segment(&model);
    let run_path = url_path_segment(&resolved_run);
    let variable_path = url_path_segment(&variable);
    let mut tile_url = format!(
        "{base}/v1/tiles/{model_path}/{run_path}/{variable_path}/{hour}/{{z}}/{{x}}/{{y}}.png"
    );
    let mut params = Vec::new();
    if let Some(member) = query.member {
        params.push(format!("member={}", url_query_value(&member)));
    }
    if let Some(palette) = query.palette {
        params.push(format!("palette={}", url_query_value(&palette)));
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
    Ok(Json(json!({
        "tilejson": "3.0.0",
        "name": format!("{model}/{resolved_run}/{variable}"),
        "scheme": "xyz",
        "tiles": [tile_url],
        "minzoom": 0,
        "maxzoom": 9,
        "bounds": bounds,
        "wxstore": {
            "schema": "wxstore.mapbox_layer.v1",
            "model": model,
            "run": resolved_run,
            "requested_run": run,
            "variable": variable,
            "forecast_hour": hour,
            "temporal_tile_template": format!("{base}/v1/tiles/{model_path}/{run_path}/{variable_path}/{{forecast_hour}}/{{z}}/{{x}}/{{y}}.png")
        }
    })))
}

fn resolve_tilejson_run(state: &AppState, model: &str, run: &str) -> Result<String, ApiError> {
    if let Some(spatial) = state.spatial.as_deref() {
        return spatial.resolve_run(model, Some(run));
    }
    if let Some(profile) = state.profile.as_deref() {
        if run == "latest" && model == profile.manifest.model {
            return Ok(profile.manifest.run_id.clone());
        }
    }
    Ok(run.to_string())
}

fn request_base_url(headers: &HeaderMap) -> String {
    let proto = headers
        .get("x-forwarded-proto")
        .and_then(|value| value.to_str().ok())
        .and_then(first_csv_value)
        .filter(|value| matches!(*value, "http" | "https"))
        .unwrap_or("http");
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get(header::HOST))
        .and_then(|value| value.to_str().ok())
        .and_then(first_csv_value)
        .filter(|value| !value.is_empty() && !value.contains('/') && !value.contains('\\'))
        .unwrap_or("127.0.0.1:8895");
    format!("{proto}://{host}")
}

fn first_csv_value(value: &str) -> Option<&str> {
    value
        .split(',')
        .next()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn url_path_segment(value: &str) -> String {
    percent_encode(value)
}

fn url_query_value(value: &str) -> String {
    percent_encode(value)
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
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
    if z > MAX_TILE_ZOOM {
        return Err(bad_request(format!(
            "tile zoom {z} exceeds max supported zoom {MAX_TILE_ZOOM}"
        )));
    }
    let y = parse_tile_y(&y).map_err(bad_anyhow)?;
    let transparent_below = query
        .transparent_below
        .or_else(|| default_transparent_below_for_variable(&variable));
    let transparent_above = query
        .transparent_above
        .or_else(|| default_transparent_above_for_variable(&variable));
    let alpha = query.alpha.unwrap_or(210);
    let requested_latest = run == "latest";
    let resolved_run = if requested_latest {
        if let Some(spatial) = state.spatial.as_deref() {
            spatial.resolve_run(&model, Some(&run))?
        } else {
            run.clone()
        }
    } else {
        run.clone()
    };
    let source_token = state
        .spatial
        .as_deref()
        .and_then(|spatial| {
            spatial
                .grid_source_cache_token(
                    &model,
                    &resolved_run,
                    query.member.as_deref(),
                    &variable,
                    forecast_hour,
                )
                .ok()
        })
        .unwrap_or_else(|| "source:profile".to_string());
    let cache_key = format!(
        "tile:v3:{source_token}:{model}:{resolved_run}:{member}:{variable}:f{forecast_hour:03}:{z}:{x}:{y}:{palette}:{min:?}:{max:?}:{transparent_below:?}:{transparent_above:?}:{alpha:?}",
        member = query.member.as_deref().unwrap_or(""),
        palette = query.palette.as_deref().unwrap_or(""),
        min = query.min,
        max = query.max,
    );
    let immutable_tile = !requested_latest;
    if let Some(png) = state.cache_get(&cache_key) {
        return Ok(png_tile_response(png, immutable_tile));
    }
    let member = query.member.clone();
    let model_for_read = model.clone();
    let run_for_read = resolved_run.clone();
    let state_for_read = state.clone();
    let grid = tokio::task::spawn_blocking(move || {
        read_grid_from_state(
            &state_for_read,
            &model_for_read,
            Some(&run_for_read),
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
    Ok(png_tile_response(png, immutable_tile))
}

fn png_tile_response(png: Bytes, immutable: bool) -> Response {
    let mut response = png.into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("image/png"));
    let cache_control = if immutable {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=30"
    };
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control),
    );
    response
}

async fn mapbox_layer(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath((model, run, variable)): AxumPath<(String, String, String)>,
    Query(query): Query<MapboxLayerQuery>,
) -> Result<Json<Value>, ApiError> {
    let base = query
        .base_url
        .unwrap_or_else(|| request_base_url(&headers))
        .trim_end_matches('/')
        .to_string();
    let resolved_run = resolve_tilejson_run(&state, &model, &run)?;
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
    let hours = available_hours_for_layer(
        &state,
        &model,
        &resolved_run,
        query.member.as_deref(),
        &variable,
    )
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
            read_grid_from_state(
                &state,
                &model,
                Some(&resolved_run),
                query.member.as_deref(),
                &variable,
                *hour,
            )
            .ok()
        })
        .map(|grid| grid_bounds(&grid))
        .unwrap_or([-180.0, -85.05112878, 180.0, 85.05112878]);
    let frames = hours
        .iter()
        .map(|hour| {
            let frame = format!("f{hour:03}");
            let model_path = url_path_segment(&model);
            let run_path = url_path_segment(&resolved_run);
            let variable_path = url_path_segment(&variable);
            let mut tile = format!(
                "{base}/v1/mapbox/tiles/{model_path}/{run_path}/{variable_path}/{frame}/{{z}}/{{x}}/{{y}}?palette={palette}&range={min},{max}"
            );
            let mut tilejson_url =
                format!("{base}/v1/mapbox/tilejson/{model_path}/{run_path}/{variable_path}/{frame}?palette={palette}&range={min},{max}");
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
        "run_id": resolved_run,
        "requested_run": run,
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
    headers: HeaderMap,
    AxumPath((model, run, variable, frame)): AxumPath<(String, String, String, String)>,
    Query(query): Query<MapboxLayerQuery>,
) -> Result<Json<Value>, ApiError> {
    let forecast_hour = parse_frame_hour(&frame).map_err(bad_anyhow)?;
    let (min, max) = query
        .range
        .as_deref()
        .and_then(parse_range_pair)
        .unwrap_or_else(|| default_range_for_variable(&variable, &[]));
    tilejson(
        State(state),
        headers,
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
    .await
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
    let variable_spec = query
        .hourly
        .as_deref()
        .or(query.variables.as_deref())
        .or(query.variable.as_deref())
        .unwrap_or("temperature_2m,dew_point_2m,wind_gusts_10m");
    let variables = split_csv(variable_spec);
    if variables.is_empty() {
        return Err(bad_request("hourly must list at least one variable"));
    }
    if variables.len() > MAX_FORECAST_VARIABLES {
        return Err(payload_too_large(format!(
            "too many hourly variables requested: {} > {}",
            variables.len(),
            MAX_FORECAST_VARIABLES
        )));
    }
    let hours = query
        .forecast_hours
        .or(query.forecast_hour)
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

async fn cross_section_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.cross_sections.status_json())
}

async fn cross_section_status_products(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(json!({
        "schema": "wxstore.cross_section.status_products.v1",
        "status": state.cross_sections.status_json(),
        "products": cross_section_products_value()
    }))
}

async fn cross_section_products() -> Json<Value> {
    Json(cross_section_products_value())
}

fn cross_section_products_value() -> Value {
    json!({
        "schema": "wxstore.cross_section.products.v1",
        "products": [
            {"product": "temperature", "label": "Temperature"},
            {"product": "wind_speed", "label": "Wind Speed"},
            {"product": "theta_e", "label": "Theta-e"},
            {"product": "rh", "label": "Relative Humidity"},
            {"product": "q", "label": "Specific Humidity"},
            {"product": "omega", "label": "Vertical Motion"},
            {"product": "vorticity", "label": "Absolute Vorticity"},
            {"product": "shear", "label": "Deep-Layer Shear"},
            {"product": "lapse_rate", "label": "Lapse Rate"},
            {"product": "cloud", "label": "Cloud Water/Ice"},
            {"product": "cloud_total", "label": "Total Hydrometeors"},
            {"product": "wetbulb", "label": "Wet Bulb"},
            {"product": "icing", "label": "Icing"},
            {"product": "frontogenesis", "label": "Frontogenesis"},
            {"product": "vpd", "label": "Vapor Pressure Deficit"},
            {"product": "dewpoint_dep", "label": "Dewpoint Depression"},
            {"product": "moisture_transport", "label": "Moisture Transport"},
            {"product": "pv", "label": "Potential Vorticity"},
            {"product": "fire_wx", "label": "Fire Weather"}
        ]
    })
}

async fn cross_section_render(
    State(state): State<Arc<AppState>>,
    Json(request): Json<CrossSectionRenderRequest>,
) -> Result<Json<Value>, ApiError> {
    let lane = state.cross_sections.clone();
    let response = tokio::task::spawn_blocking(move || run_cross_section_render(&lane, request))
        .await
        .map_err(|err| internal_error(format!("join error: {err}")))?
        .map_err(|err| bad_request(err.to_string()))?;
    Ok(Json(response))
}

async fn cross_section_artifact(
    State(state): State<Arc<AppState>>,
    AxumPath((render_id, file_name)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    let path = state
        .cross_sections
        .artifact_path(&render_id, &file_name)
        .map_err(|err| not_found(err.to_string()))?;
    let bytes = fs::read(&path).map_err(|err| not_found(err.to_string()))?;
    let mut response = Bytes::from(bytes).into_response();
    let content_type = match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "png" => "image/png",
        "webp" => "image/webp",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    Ok(response)
}

async fn sounding_status(State(state): State<Arc<AppState>>) -> Json<Value> {
    Json(state.soundings.status_json())
}

async fn sounding_render(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SoundingRenderRequest>,
) -> Result<Json<Value>, ApiError> {
    let lane = state.soundings.clone();
    let spatial = state.spatial.clone();
    let response = tokio::task::spawn_blocking(move || {
        run_sounding_render(&lane, spatial.as_deref(), request)
    })
    .await
    .map_err(|err| internal_error(format!("join error: {err}")))?
    .map_err(|err| bad_request(err.to_string()))?;
    Ok(Json(response))
}

async fn sounding_artifact(
    State(state): State<Arc<AppState>>,
    AxumPath((render_id, file_name)): AxumPath<(String, String)>,
) -> Result<Response, ApiError> {
    let path = state
        .soundings
        .artifact_path(&render_id, &file_name)
        .map_err(|err| not_found(err.to_string()))?;
    let bytes = fs::read(&path).map_err(|err| not_found(err.to_string()))?;
    let mut response = Bytes::from(bytes).into_response();
    let content_type = match path.extension().and_then(|ext| ext.to_str()).unwrap_or("") {
        "png" => "image/png",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=86400"),
    );
    Ok(response)
}

fn run_sounding_render(
    lane: &SoundingLane,
    spatial: Option<&SpatialLane>,
    request: SoundingRenderRequest,
) -> Result<Value> {
    if !request.lat.is_finite() || !request.lon.is_finite() {
        bail!("sounding coordinates must be finite");
    }
    if !(-90.0..=90.0).contains(&request.lat) {
        bail!("sounding latitude must be within [-90, 90]");
    }
    let model = normalize_cli_token(request.model.as_deref().unwrap_or("hrrr"), "model")?;
    let requested_run = request.run.unwrap_or_else(|| "latest".to_string());
    let volume_store = lane.resolve_volume_store(&model, &requested_run).ok();
    let resolved_run = volume_store
        .as_ref()
        .map(|(run, _)| run.clone())
        .map(Ok)
        .unwrap_or_else(|| {
            if is_archive_sounding_model(&model) {
                lane.resolve_volume_run(&model, &requested_run)
            } else {
                resolve_sounding_run(spatial, &model, &requested_run)
            }
        })?;
    let volume_store = volume_store.or_else(|| {
        lane.resolve_volume_store(&model, &resolved_run)
            .ok()
            .filter(|_| lane.volume_renderer.is_file())
    });
    let using_volume_store = volume_store.is_some();
    if using_volume_store && !lane.volume_renderer.is_file() {
        bail!(
            "volume-store sounding renderer is not present: {}",
            lane.volume_renderer.display()
        );
    }
    if !using_volume_store && !lane.renderer.is_file() {
        bail!(
            "sounding renderer is not present: {}",
            lane.renderer.display()
        );
    }
    let (date, cycle) = if using_volume_store {
        parse_run_date_cycle(&resolved_run).unwrap_or_default()
    } else {
        parse_run_date_cycle(&resolved_run)?
    };
    let render_model = sounding_cli_model(&model);
    let source = if using_volume_store {
        "pressure_volume".to_string()
    } else {
        normalize_cli_token(
            request
                .source
                .as_deref()
                .unwrap_or_else(|| default_sounding_source(&render_model)),
            "source",
        )?
    };
    let hour = request.forecast_hour.or(request.hour).unwrap_or(0);
    if hour > 840 {
        bail!("forecast hour is too large");
    }
    let sample_method = normalize_sounding_sample_method(
        request
            .sample_method
            .as_deref()
            .unwrap_or("inverse-distance4"),
    )?;
    let crop_radius_deg = request.crop_radius_deg.unwrap_or(1.25).clamp(0.25, 6.0);
    let lon = normalize_lon(request.lon);
    let render_id = stable_sounding_id(&json!({
        "model": &model,
        "render_model": &render_model,
        "run": &resolved_run,
        "requested_run": &requested_run,
        "source": &source,
        "hour": hour,
        "lat": request.lat,
        "lon": lon,
        "backend": if using_volume_store { "pressure_volume" } else { "grib" },
        "sample_method": &sample_method,
        "crop_radius_deg": crop_radius_deg,
        "box_radius_km": request.box_radius_km,
        "station_id": request.station_id
    }));
    let out_dir = lane.artifact_root.join(&render_id);
    let png_path = out_dir.join("sounding.png");
    let report_path = out_dir.join("sounding_manifest.json");
    if report_path.is_file() && png_path.is_file() && !request.force.unwrap_or(false) {
        let mut report: Value = serde_json::from_slice(&fs::read(&report_path)?)?;
        annotate_sounding_report(&render_id, &mut report);
        report["cache_hit"] = json!(true);
        report["requested_run"] = json!(requested_run);
        report["resolved_run"] = json!(resolved_run);
        report["model"] = json!(model);
        report["render_model"] = json!(render_model);
        report["backend"] = json!(if using_volume_store {
            "pressure_volume"
        } else {
            "grib"
        });
        return Ok(report);
    }
    fs::create_dir_all(&out_dir)?;

    let mut command = if let Some((_, store)) = volume_store.as_ref() {
        if hour > u16::from(u8::MAX) {
            bail!("pressure VolumeStore soundings only support f000-f255, got f{hour:03}");
        }
        let mut command = ProcessCommand::new(&lane.volume_renderer);
        command
            .arg("--store")
            .arg(store)
            .arg("--out-dir")
            .arg(&out_dir)
            .arg("--hour")
            .arg(hour.to_string())
            .arg(format!("--lat={}", request.lat))
            .arg(format!("--lon={}", lon))
            .arg("--sample-method")
            .arg("nearest")
            .arg("--output")
            .arg(&png_path)
            .arg("--manifest")
            .arg(&report_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(station_id) = request
            .station_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            command.arg("--station-id").arg(station_id);
        }
        command
    } else {
        let mut command = ProcessCommand::new(&lane.renderer);
        command
            .arg("--model")
            .arg(&render_model)
            .arg("--date")
            .arg(&date)
            .arg("--cycle")
            .arg(cycle.to_string())
            .arg("--forecast-hour")
            .arg(hour.to_string())
            .arg("--source")
            .arg(&source)
            .arg(format!("--lat={}", request.lat))
            .arg(format!("--lon={}", lon))
            .arg("--crop-radius-deg")
            .arg(crop_radius_deg.to_string())
            .arg("--sample-method")
            .arg(&sample_method)
            .arg("--out-dir")
            .arg(&out_dir)
            .arg("--cache-dir")
            .arg(&lane.cache_root)
            .arg("--output")
            .arg(&png_path)
            .arg("--manifest")
            .arg(&report_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(box_radius_km) = request.box_radius_km {
            command
                .arg("--box-radius-km")
                .arg(box_radius_km.to_string());
        }
        if let Some(station_id) = request
            .station_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            command.arg("--station-id").arg(station_id);
        }
        command
    };
    let started = Instant::now();
    let output = command.output()?;
    if !output.status.success() {
        bail!(
            "sounding renderer exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut report: Value = serde_json::from_slice(&fs::read(&report_path)?)?;
    annotate_sounding_report(&render_id, &mut report);
    report["cache_hit"] = json!(false);
    report["requested_run"] = json!(requested_run);
    report["resolved_run"] = json!(resolved_run);
    report["model"] = json!(model);
    report["render_model"] = json!(render_model);
    report["backend"] = json!(if using_volume_store {
        "pressure_volume"
    } else {
        "grib"
    });
    report["server_elapsed_ms"] = json!(started.elapsed().as_millis());
    Ok(report)
}

fn annotate_sounding_report(render_id: &str, report: &mut Value) {
    report["schema"] = json!("wxstore.sounding.render.v1");
    report["render_id"] = json!(render_id);
    report["artifact_base_url"] = json!(format!("/v1/sounding/artifacts/{render_id}"));
    report["png_url"] = json!(format!("/v1/sounding/artifacts/{render_id}/sounding.png"));
    report["manifest_url"] = json!(format!(
        "/v1/sounding/artifacts/{render_id}/sounding_manifest.json"
    ));
    if let Some(output) = report.get_mut("output").and_then(Value::as_object_mut) {
        output.insert(
            "png_url".to_string(),
            Value::String(format!("/v1/sounding/artifacts/{render_id}/sounding.png")),
        );
        output.insert(
            "manifest_url".to_string(),
            Value::String(format!(
                "/v1/sounding/artifacts/{render_id}/sounding_manifest.json"
            )),
        );
    }
}

fn stable_sounding_id(value: &Value) -> String {
    let body = serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"));
    let mut hash = 0xcbf29ce484222325u64;
    for byte in body.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("snd_{hash:016x}")
}

fn resolve_sounding_run(
    spatial: Option<&SpatialLane>,
    model: &str,
    requested_run: &str,
) -> Result<String> {
    if requested_run != "latest" {
        return Ok(requested_run.to_string());
    }
    spatial
        .and_then(|lane| lane.latest_run_for_model(model))
        .ok_or_else(|| anyhow!("no latest run is available for model '{model}'"))
}

fn sounding_cli_model(model: &str) -> String {
    match model {
        "hrrr_archive" | "hrrr-archive" => "hrrr".to_string(),
        "ecmwf" | "ecmwf_open_data" => "ecmwf-open-data".to_string(),
        value => value.to_string(),
    }
}

fn is_archive_sounding_model(model: &str) -> bool {
    matches!(model, "hrrr_archive" | "hrrr-archive")
}

fn default_sounding_source(model: &str) -> &'static str {
    match model {
        "hrrr" | "hrrr_archive" | "hrrr-archive" => "aws",
        _ => "nomads",
    }
}

fn normalize_sounding_sample_method(value: &str) -> Result<String> {
    let method = value.trim().to_ascii_lowercase().replace('_', "-");
    match method.as_str() {
        "nearest" | "inverse-distance4" | "box-mean" => Ok(method),
        _ => bail!("unsupported sounding sample method '{value}'"),
    }
}

fn parse_run_date_cycle(run: &str) -> Result<(String, u8)> {
    let chars = run.chars().collect::<Vec<_>>();
    let mut date = None;
    for start in 0..chars.len().saturating_sub(7) {
        let candidate = chars[start..start + 8].iter().collect::<String>();
        if candidate.chars().all(|ch| ch.is_ascii_digit()) {
            date = Some(candidate);
        }
    }
    let Some(date) = date else {
        bail!("could not find YYYYMMDD in run id '{run}'");
    };
    let mut cycle = None;
    for start in 0..chars.len().saturating_sub(2) {
        if chars[start].is_ascii_digit()
            && chars[start + 1].is_ascii_digit()
            && matches!(chars[start + 2], 'z' | 'Z')
        {
            let value = chars[start..start + 2].iter().collect::<String>();
            if let Ok(parsed) = value.parse::<u8>() {
                if parsed <= 23 {
                    cycle = Some(parsed);
                }
            }
        }
    }
    let cycle = cycle.ok_or_else(|| anyhow!("could not find cycle hour in run id '{run}'"))?;
    Ok((date, cycle))
}

fn run_cross_section_render(
    lane: &CrossSectionLane,
    request: CrossSectionRenderRequest,
) -> Result<Value> {
    if !lane.renderer.is_file() {
        bail!(
            "cross-section renderer is not present: {}",
            lane.renderer.display()
        );
    }
    if !request.start_lat.is_finite()
        || !request.start_lon.is_finite()
        || !request.end_lat.is_finite()
        || !request.end_lon.is_finite()
    {
        bail!("cross-section coordinates must be finite");
    }
    let start_lon = normalize_lon(request.start_lon);
    let end_lon = normalize_lon(request.end_lon);
    let model = request.model.unwrap_or_else(|| "hrrr".to_string());
    let run = request.run.unwrap_or_else(|| "latest".to_string());
    let resolved_run = lane.resolve_run(&model, &run)?;
    let store = lane.resolve_store(&model, &resolved_run)?;
    let product = request
        .products
        .or(request.product)
        .unwrap_or_else(|| "wind_speed".to_string());
    let hour = request.hour.unwrap_or(0);
    let route_name = request
        .route_name
        .unwrap_or_else(|| "Selected cross section".to_string());
    let route_id = stable_cross_section_id(&json!({
        "model": &model,
        "run": &resolved_run,
        "requested_run": &run,
        "product": &product,
        "hour": hour,
        "hours": request.hours.clone(),
        "start": [request.start_lat, start_lon],
        "end": [request.end_lat, end_lon],
        "spacing_km": request.spacing_km,
        "top_pressure_hpa": request.top_pressure_hpa,
        "width": request.width,
        "height": request.height,
    }));
    let out_dir = lane.artifact_root.join(&route_id);
    let report_path = out_dir.join("volume_cross_section_render_report.json");
    if report_path.is_file() && !request.force.unwrap_or(false) {
        let mut report: Value = serde_json::from_slice(&fs::read(&report_path)?)?;
        annotate_cross_section_report(lane, &route_id, &mut report);
        report["cache_hit"] = json!(true);
        report["requested_run"] = json!(run);
        report["resolved_run"] = json!(resolved_run);
        return Ok(report);
    }
    fs::create_dir_all(&out_dir)?;

    let mut command = ProcessCommand::new(&lane.renderer);
    command
        .arg("--store")
        .arg(&store)
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("--products")
        .arg(&product)
        .arg("--hour")
        .arg(hour.to_string())
        .arg("--route-id")
        .arg(&route_id)
        .arg("--route-name")
        .arg(&route_name)
        .arg("--start-lat")
        .arg(request.start_lat.to_string())
        .arg("--start-lon")
        .arg(start_lon.to_string())
        .arg("--end-lat")
        .arg(request.end_lat.to_string())
        .arg("--end-lon")
        .arg(end_lon.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(hours) = request.hours.as_deref() {
        command.arg("--hours").arg(hours);
    }
    if let Some(spacing) = request.spacing_km {
        command.arg("--spacing-km").arg(spacing.to_string());
    }
    if let Some(top) = request.top_pressure_hpa {
        command.arg("--top-pressure-hpa").arg(top.to_string());
    }
    if let Some(width) = request.width {
        command.arg("--width").arg(width.to_string());
    }
    if let Some(height) = request.height {
        command.arg("--height").arg(height.to_string());
    }
    let started = Instant::now();
    let output = command.output()?;
    if !output.status.success() {
        bail!(
            "cross-section renderer exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let mut report: Value = serde_json::from_slice(&fs::read(&report_path)?)?;
    annotate_cross_section_report(lane, &route_id, &mut report);
    report["cache_hit"] = json!(false);
    report["requested_run"] = json!(run);
    report["resolved_run"] = json!(resolved_run);
    report["server_elapsed_ms"] = json!(started.elapsed().as_millis());
    Ok(report)
}

fn annotate_cross_section_report(lane: &CrossSectionLane, render_id: &str, report: &mut Value) {
    report["schema"] = json!("wxstore.cross_section.render.v1");
    report["render_id"] = json!(render_id);
    report["artifact_base_url"] = json!(format!("/v1/cross-section/artifacts/{render_id}"));
    if let Some(outputs) = report.get_mut("outputs").and_then(Value::as_array_mut) {
        for output in outputs {
            for key in ["png_path", "webp_path", "summary_path"] {
                let Some(path) = output.get(key).and_then(Value::as_str) else {
                    continue;
                };
                let Some(file_name) = Path::new(path).file_name().and_then(|name| name.to_str())
                else {
                    continue;
                };
                let url_key = match key {
                    "png_path" => "png_url",
                    "webp_path" => "webp_url",
                    "summary_path" => "summary_url",
                    _ => continue,
                };
                output[url_key] = json!(format!(
                    "/v1/cross-section/artifacts/{render_id}/{file_name}"
                ));
            }
        }
    }
    let _ = lane;
}

fn stable_cross_section_id(value: &Value) -> String {
    let body = serde_json::to_string(value).unwrap_or_else(|_| format!("{value:?}"));
    let mut hash = 0xcbf29ce484222325u64;
    for byte in body.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("cs_{hash:016x}")
}

async fn resolve(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PointQuery>,
) -> Result<Json<Value>, ApiError> {
    let lat = query_lat(&query)?;
    let lon = query_lon(&query)?;
    let profile = state.profile_lane()?;
    let point = profile.locate_nearest(lat, lon).map_err(bad_anyhow)?;
    let manifest = &profile.manifest;
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
    let profile = state.profile_lane()?;
    let point = profile.locate_nearest(lat, lon).map_err(bad_anyhow)?;
    let req = RequestShape::from_point_query(profile, &query).map_err(bad_anyhow)?;
    json_response_for_point(state, point, lat, lon, req, headers, false).await
}

async fn point_bin(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PointQuery>,
) -> Result<Response, ApiError> {
    let lat = query_lat(&query)?;
    let lon = query_lon(&query)?;
    let profile = state.profile_lane()?;
    let point = profile.locate_nearest(lat, lon).map_err(bad_anyhow)?;
    let mut req = RequestShape::from_point_query(profile, &query).map_err(bad_anyhow)?;
    req.response_format = ResponseFormat::WxBin;
    binary_response_for_point(state, point, lat, lon, req, false).await
}

async fn canonical_temporal_sounding(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    AxumPath((model, domain, run, x, y)): AxumPath<(String, String, String, usize, usize)>,
    Query(query): Query<CanonicalQuery>,
) -> Result<Response, ApiError> {
    let profile = state.profile_lane()?;
    validate_canonical(profile, &model, &domain, &run)?;
    let point = profile.grid_point(x, y).map_err(bad_anyhow)?;
    let req = RequestShape::from_canonical_query(profile, &query).map_err(bad_anyhow)?;
    json_response_for_point(state, point, point.lat, point.lon, req, headers, true).await
}

async fn canonical_point_bin(
    State(state): State<Arc<AppState>>,
    AxumPath((model, domain, run, x, y)): AxumPath<(String, String, String, usize, usize)>,
    Query(query): Query<CanonicalQuery>,
) -> Result<Response, ApiError> {
    let profile = state.profile_lane()?;
    validate_canonical(profile, &model, &domain, &run)?;
    let point = profile.grid_point(x, y).map_err(bad_anyhow)?;
    let mut req = RequestShape::from_canonical_query(profile, &query).map_err(bad_anyhow)?;
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
    let profile = state.profile_lane_arc()?;
    let key = cache_key(&profile.manifest.run_id, &point, &req, "json");
    if immutable {
        if let Some(bytes) = state.cache_get(&key) {
            return Ok(bytes_response(bytes, "application/json", true, immutable));
        }
    }

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
    let profile = state.profile_lane_arc()?;
    let key = cache_key(&profile.manifest.run_id, &point, &req, "wxbin");
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

const WEATHER_OBJECT_DEFAULT_LIMIT: usize = 500;
const WEATHER_OBJECT_MAX_LIMIT: usize = 10_000;

struct WeatherObjectAccumulator<'a> {
    query: &'a WeatherObjectQuery,
    offset: usize,
    limit: usize,
    matched_count: usize,
    objects: Vec<Value>,
}

impl<'a> WeatherObjectAccumulator<'a> {
    fn new(query: &'a WeatherObjectQuery) -> Self {
        Self {
            query,
            offset: query.offset.unwrap_or(0),
            limit: query
                .limit
                .unwrap_or(WEATHER_OBJECT_DEFAULT_LIMIT)
                .min(WEATHER_OBJECT_MAX_LIMIT),
            matched_count: 0,
            objects: Vec::new(),
        }
    }

    fn consider(&mut self, object: Value) {
        if !weather_object_matches(&object, self.query) {
            return;
        }
        self.matched_count += 1;
        if self.matched_count <= self.offset || self.objects.len() >= self.limit {
            return;
        }
        self.objects.push(object);
    }
}

fn weather_objects_index(
    query: &WeatherObjectQuery,
    spatial: Option<&SpatialLane>,
    static_plots: Option<&StaticPlotLane>,
    evidence: Option<&EvidenceBundleLane>,
    observations: Option<&ObservationLane>,
    satellite_tiles: Option<&SatelliteTileLane>,
    radar_tiles: Option<&RadarTileLane>,
) -> Value {
    let mut acc = WeatherObjectAccumulator::new(query);
    let mut lanes = Vec::new();
    let mut errors = Vec::new();

    if let Some(evidence) = evidence {
        lanes.push("evidence_bundles");
        match evidence.bundle_records() {
            Ok(records) => {
                for record in records {
                    let id = record.get("id").and_then(Value::as_str).unwrap_or_default();
                    acc.consider(json!({
                        "schema": "wxstore.weather_object.v1",
                        "id": format!("evidence_bundle:{id}"),
                        "kind": "evidence_bundle",
                        "lane": "evidence_bundles",
                        "source": record.get("source").cloned().unwrap_or_else(|| json!(null)),
                        "label": record.get("claim").cloned().unwrap_or_else(|| json!(id)),
                        "updated_at": record.get("updated_at").cloned().unwrap_or_else(|| json!(null)),
                        "url": record.get("url").cloned().unwrap_or_else(|| json!(format!("/v1/evidence/bundles/{id}"))),
                        "metadata": record,
                    }));
                }
            }
            Err(err) => errors.push(json!({"lane": "evidence_bundles", "error": err.to_string()})),
        }
    }

    if let Some(observations) = observations {
        lanes.push("direct_observations");
        match observations.weather_objects() {
            Ok(snapshot) => {
                for object in snapshot.objects {
                    acc.consider(object);
                }
                errors.extend(snapshot.errors);
            }
            Err(err) => {
                errors.push(json!({"lane": "direct_observations", "error": err.to_string()}))
            }
        }
    }

    if let Some(static_plots) = static_plots {
        lanes.push("static_plots");
        match static_plots.manifests() {
            Ok(records) => {
                for record in records {
                    let identity = static_plot_record_identity(&record);
                    let plot_variant = static_plot_variant_key(&identity);
                    let valid_time = static_plot_valid_time(&identity);
                    let ensemble_statistic = identity.ensemble_stat.clone();
                    for (artifact_index, artifact) in record.manifest.artifacts.iter().enumerate() {
                        let path =
                            resolve_static_artifact_path(&record.manifest, &artifact.relative_path);
                        let relative_path = relative_path_string(&static_plots.root, &path);
                        let encoded_path = url_encode_query_component(&relative_path);
                        let threshold = static_plot_artifact_threshold(artifact);
                        let url = format!(
                            "/v1/static-plots/artifacts/{}/{}?path={}",
                            record.id, artifact_index, encoded_path
                        );
                        acc.consider(json!({
                            "schema": "wxstore.weather_object.v1",
                            "id": format!("static_plot_artifact:{}:{artifact_index}", record.id),
                            "kind": "static_plot_artifact",
                            "lane": "static_plots",
                            "model": identity.model,
                            "run": record.manifest.run_label,
                            "date": identity.date_yyyymmdd,
                            "cycle_utc": identity.cycle_utc,
                            "forecast_hour": identity.forecast_hour,
                            "valid_time": valid_time,
                            "source": identity.source,
                            "domain": identity.domain_slug,
                            "member": identity.member,
                            "ensemble_kind": identity.ensemble_kind,
                            "ensemble_stat": identity.ensemble_stat,
                            "ensemble_statistic": ensemble_statistic,
                            "projection_variant": plot_variant,
                            "product": artifact.artifact_key,
                            "threshold": threshold,
                            "state": artifact.state,
                            "label": artifact.artifact_key,
                            "url": url,
                            "path": relative_path,
                            "exists": path.is_file(),
                            "metadata": {
                                "manifest_id": record.id,
                                "manifest_path": relative_path_string(&static_plots.root, &record.path),
                                "detail": artifact.detail,
                                "content_identity": artifact.content_identity,
                                "input_fetch_keys": artifact.input_fetch_keys,
                            }
                        }));
                    }
                }
            }
            Err(err) => errors.push(json!({"lane": "static_plots", "error": err.to_string()})),
        }
    }

    if let Some(spatial) = spatial {
        lanes.push("surface_spatial");
        for model in spatial.model_ids() {
            for run in spatial.runs_for_model(&model) {
                let mut member_options = vec![None];
                member_options.extend(spatial.members_for(&model, &run).into_iter().map(Some));
                for member in member_options {
                    let member_ref = member.as_deref();
                    let variables = spatial
                        .variables_for(&model, &run, member_ref)
                        .unwrap_or_default();
                    for variable in variables {
                        let hours = spatial
                            .available_hours_for(&model, &run, member_ref, &variable)
                            .unwrap_or_default();
                        let valid_times = valid_times_from_run_id(&run, &hours);
                        let mut url = format!(
                            "/v1/layers?model={}&run={}&variable={}",
                            url_encode_query_component(&model),
                            url_encode_query_component(&run),
                            url_encode_query_component(&variable)
                        );
                        if let Some(member) = member_ref {
                            url.push_str("&member=");
                            url.push_str(&url_encode_query_component(member));
                        }
                        acc.consider(json!({
                            "schema": "wxstore.weather_object.v1",
                            "id": format!(
                                "model_grid_field:{}:{}:{}:{}",
                                model,
                                run,
                                member_ref.unwrap_or("deterministic"),
                                variable
                            ),
                            "kind": "model_grid_field",
                            "lane": "surface_spatial",
                            "model": model,
                            "run": run,
                            "member": member_ref,
                            "member_key": member_ref.unwrap_or("deterministic"),
                            "product": variable,
                            "forecast_hours": hours,
                            "valid_times": valid_times,
                            "url": url,
                            "metadata": {
                                "format": "wxa_or_zarr",
                                "variables_url": format!(
                                    "/v1/variables?model={}&run={}",
                                    url_encode_query_component(&model),
                                    url_encode_query_component(&run)
                                ),
                                "sample_url_template": "/v1/sample?model={model}&run={run}&variable={product}&forecast_hour={hour}&lat={lat}&lon={lon}"
                            }
                        }));
                        for (hour, valid_time) in hours.iter().copied().zip(valid_times.iter()) {
                            let mut field_url = format!(
                                "/v1/grid?model={}&run={}&variable={}&forecast_hour={hour}",
                                url_encode_query_component(&model),
                                url_encode_query_component(&run),
                                url_encode_query_component(&variable)
                            );
                            if let Some(member) = member_ref {
                                field_url.push_str("&member=");
                                field_url.push_str(&url_encode_query_component(member));
                            }
                            acc.consider(json!({
                                "schema": "wxstore.weather_object.v1",
                                "id": format!(
                                    "model_grid_field:{}:{}:{}:{}:f{hour:03}",
                                    model,
                                    run,
                                    member_ref.unwrap_or("deterministic"),
                                    variable
                                ),
                                "kind": "model_grid_field",
                                "lane": "surface_spatial",
                                "model": model,
                                "run": run,
                                "member": member_ref,
                                "member_key": member_ref.unwrap_or("deterministic"),
                                "product": variable,
                                "forecast_hour": hour,
                                "valid_time": valid_time,
                                "label": format!("{variable} f{hour:03}"),
                                "url": field_url,
                                "metadata": {
                                    "format": "wxa_or_zarr",
                                    "product_url": url,
                                    "sample_url_template": "/v1/sample?model={model}&run={run}&variable={product}&forecast_hour={hour}&lat={lat}&lon={lon}"
                                }
                            }));
                        }
                    }
                }
            }
        }
    }

    if let Some(satellite_tiles) = satellite_tiles {
        lanes.push("satellite_tiles");
        match satellite_tiles.layers_json() {
            Ok(layers) => {
                if let Some(items) = layers.get("layers").and_then(Value::as_array) {
                    for item in items {
                        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                        acc.consider(json!({
                            "schema": "wxstore.weather_object.v1",
                            "id": format!("satellite_layer:{id}"),
                            "kind": "satellite_layer",
                            "lane": "satellite_tiles",
                            "source": item.get("latest").and_then(|latest| latest.get("source")).cloned().unwrap_or_else(|| json!(null)),
                            "label": id,
                            "url": item.get("frames_url").cloned().unwrap_or_else(|| json!(format!("/v1/satellite/layers/{id}/frames.json"))),
                            "metadata": item,
                        }));
                        match satellite_tiles.frames_json(id) {
                            Ok(frames) => {
                                if let Some(frames) = frames.get("frames").and_then(Value::as_array)
                                {
                                    for frame in frames {
                                        acc.consider(satellite_frame_weather_object(
                                            id, item, frame,
                                        ));
                                    }
                                }
                            }
                            Err(err) => errors.push(json!({
                                "lane": "satellite_tiles",
                                "layer": id,
                                "error": err.to_string()
                            })),
                        }
                    }
                }
            }
            Err(err) => errors.push(json!({"lane": "satellite_tiles", "error": err.to_string()})),
        }
    }

    if let Some(radar_tiles) = radar_tiles {
        lanes.push("radar_tiles");
        match radar_tiles.layers_json() {
            Ok(layers) => {
                if let Some(items) = layers.get("layers").and_then(Value::as_array) {
                    for item in items {
                        let id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                        let latest = item.get("latest");
                        acc.consider(json!({
                            "schema": "wxstore.weather_object.v1",
                            "id": format!("radar_layer:{id}"),
                            "kind": "radar_layer",
                            "lane": "radar_tiles",
                            "source": latest.and_then(|latest| latest.get("source_key_or_url")).cloned().unwrap_or_else(|| json!("nexrad_level2")),
                            "label": id,
                            "product": latest.and_then(|latest| latest.get("product")).cloned().unwrap_or_else(|| json!(null)),
                            "valid_time": latest.and_then(|latest| latest.get("scan_time_utc")).cloned().unwrap_or_else(|| json!(null)),
                            "url": item.get("frames_url").cloned().unwrap_or_else(|| json!(format!("/v1/radar/layers/{id}/frames.json"))),
                            "metadata": item,
                        }));
                        match radar_tiles.frames_json(id) {
                            Ok(frames) => {
                                if let Some(frames) = frames.get("frames").and_then(Value::as_array)
                                {
                                    for frame in frames {
                                        acc.consider(radar_frame_weather_object(id, item, frame));
                                        if let Some(tilts) =
                                            frame.get("tilts").and_then(Value::as_array)
                                        {
                                            for tilt in tilts {
                                                acc.consider(radar_tilt_weather_object(
                                                    id, item, frame, tilt,
                                                ));
                                            }
                                        }
                                    }
                                }
                            }
                            Err(err) => errors.push(json!({
                                "lane": "radar_tiles",
                                "layer": id,
                                "error": err.to_string()
                            })),
                        }
                    }
                }
            }
            Err(err) => errors.push(json!({"lane": "radar_tiles", "error": err.to_string()})),
        }
    }

    json!({
        "schema": "wxstore.weather_objects.v1",
        "status": if errors.is_empty() { "ready" } else { "partial" },
        "lanes": lanes,
        "query": {
            "kind": query.kind,
            "lane": query.lane,
            "category": query.category,
            "model": query.model,
            "run": query.run,
            "product": query.product,
            "member": query.member,
            "forecast_hour": query.forecast_hour,
            "valid_time": query.valid_time,
            "frame": query.frame,
            "tilt": query.tilt,
            "threshold": query.threshold,
            "ensemble_stat": query.ensemble_stat,
            "ensemble_statistic": query.ensemble_stat,
            "source": query.source,
            "source_kind": query.source_kind,
            "network": query.network,
            "parameter": query.parameter,
            "quality_tier": query.quality_tier,
            "max_age_minutes": query.max_age_minutes,
            "q": query.q,
            "bbox": query.bbox,
            "lat": query.lat,
            "lon": query.lon,
            "radius_km": query.radius_km,
            "limit": acc.limit,
            "offset": acc.offset,
        },
        "matched_object_count": acc.matched_count,
        "returned_object_count": acc.objects.len(),
        "objects": acc.objects,
        "errors": errors,
    })
}

fn static_plot_valid_time(identity: &StaticPlotIdentity) -> Option<String> {
    let date = identity.date_yyyymmdd.as_deref()?;
    let cycle_utc = identity.cycle_utc?;
    let forecast_hour = identity.forecast_hour?;
    let date = chrono::NaiveDate::parse_from_str(date, "%Y%m%d").ok()?;
    let cycle = date.and_hms_opt(u32::from(cycle_utc), 0, 0)?;
    let valid = cycle + chrono::Duration::hours(i64::from(forecast_hour));
    Some(
        chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(valid, chrono::Utc)
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    )
}

fn static_plot_artifact_threshold(artifact: &StaticPlotArtifact) -> Option<Value> {
    artifact
        .content_identity
        .as_ref()
        .and_then(weather_object_first_threshold_value)
}

fn satellite_frame_weather_object(layer_id: &str, layer: &Value, frame: &Value) -> Value {
    let frame_id = frame.get("id").and_then(Value::as_str).unwrap_or_default();
    let product =
        weather_object_first_value(frame, &["product", "channel", "band"]).or_else(|| {
            layer.get("latest").and_then(|latest| {
                weather_object_first_value(latest, &["product", "channel", "band"])
            })
        });
    let source = weather_object_first_value(frame, &["source", "source_key", "source_key_or_url"])
        .or_else(|| {
            layer.get("latest").and_then(|latest| {
                weather_object_first_value(latest, &["source", "source_key", "source_key_or_url"])
            })
        });
    let valid_time =
        weather_object_first_value(frame, &["valid_time", "scan_time_utc", "timestamp", "time"]);
    let url = weather_object_first_value(frame, &["tile_url_template", "url_template", "url"])
        .or_else(|| layer.get("frames_url").cloned())
        .unwrap_or_else(|| json!(format!("/v1/satellite/layers/{layer_id}/frames.json")));
    json!({
        "schema": "wxstore.weather_object.v1",
        "id": format!("satellite_frame:{layer_id}:{frame_id}"),
        "kind": "satellite_frame",
        "lane": "satellite_tiles",
        "source": source.unwrap_or_else(|| json!(null)),
        "product": product.unwrap_or_else(|| json!(null)),
        "frame": frame_id,
        "valid_time": valid_time.unwrap_or_else(|| json!(null)),
        "label": frame.get("label").cloned().unwrap_or_else(|| json!(frame_id)),
        "url": url,
        "metadata": {
            "layer": layer,
            "frame": frame,
        },
    })
}

fn radar_frame_weather_object(layer_id: &str, layer: &Value, frame: &Value) -> Value {
    let frame_id = frame.get("id").and_then(Value::as_str).unwrap_or_default();
    let product = weather_object_first_value(frame, &["product"]);
    let source = weather_object_first_value(frame, &["source_key_or_url", "source", "source_key"])
        .unwrap_or_else(|| json!("nexrad_level2"));
    let valid_time =
        weather_object_first_value(frame, &["valid_time", "scan_time_utc", "timestamp"]);
    let url = weather_object_first_value(frame, &["tile_url_template", "url_template", "url"])
        .or_else(|| layer.get("frames_url").cloned())
        .unwrap_or_else(|| json!(format!("/v1/radar/layers/{layer_id}/frames.json")));
    json!({
        "schema": "wxstore.weather_object.v1",
        "id": format!("radar_frame:{layer_id}:{frame_id}"),
        "kind": "radar_frame",
        "lane": "radar_tiles",
        "source": source,
        "product": product.unwrap_or_else(|| json!(null)),
        "frame": frame_id,
        "valid_time": valid_time.unwrap_or_else(|| json!(null)),
        "label": frame.get("label").cloned().unwrap_or_else(|| json!(frame_id)),
        "url": url,
        "metadata": {
            "layer": layer,
            "frame": frame,
        },
    })
}

fn radar_tilt_weather_object(layer_id: &str, layer: &Value, frame: &Value, tilt: &Value) -> Value {
    let frame_id = frame.get("id").and_then(Value::as_str).unwrap_or_default();
    let tilt_id = tilt.get("id").and_then(Value::as_str).unwrap_or_default();
    let product = weather_object_first_value(tilt, &["product"])
        .or_else(|| weather_object_first_value(frame, &["product"]));
    let source = weather_object_first_value(tilt, &["source_key_or_url", "source", "source_key"])
        .or_else(|| {
            weather_object_first_value(frame, &["source_key_or_url", "source", "source_key"])
        })
        .unwrap_or_else(|| json!("nexrad_level2"));
    let valid_time =
        weather_object_first_value(tilt, &["valid_time", "scan_time_utc", "timestamp"]).or_else(
            || weather_object_first_value(frame, &["valid_time", "scan_time_utc", "timestamp"]),
        );
    let url = weather_object_first_value(
        tilt,
        &[
            "tile_url_template",
            "numeric_sidecar_url",
            "url_template",
            "url",
        ],
    )
    .or_else(|| {
        weather_object_first_value(
            frame,
            &[
                "tile_url_template",
                "numeric_sidecar_url",
                "url_template",
                "url",
            ],
        )
    })
    .or_else(|| layer.get("frames_url").cloned())
    .unwrap_or_else(|| json!(format!("/v1/radar/layers/{layer_id}/frames.json")));
    json!({
        "schema": "wxstore.weather_object.v1",
        "id": format!("radar_tilt:{layer_id}:{frame_id}:{tilt_id}"),
        "kind": "radar_tilt",
        "lane": "radar_tiles",
        "source": source,
        "product": product.unwrap_or_else(|| json!(null)),
        "frame": frame_id,
        "tilt": tilt_id,
        "valid_time": valid_time.unwrap_or_else(|| json!(null)),
        "label": tilt.get("label").cloned().unwrap_or_else(|| json!(format!("{frame_id} {tilt_id}"))),
        "url": url,
        "metadata": {
            "layer": layer,
            "frame": frame,
            "tilt": tilt,
        },
    })
}

fn weather_object_first_value(object: &Value, keys: &[&str]) -> Option<Value> {
    keys.iter()
        .filter_map(|key| object.get(*key))
        .find(|value| !value.is_null())
        .cloned()
}

fn weather_object_matches(object: &Value, query: &WeatherObjectQuery) -> bool {
    for (filter, key) in [
        (query.kind.as_deref(), "kind"),
        (query.lane.as_deref(), "lane"),
        (query.category.as_deref(), "category"),
        (query.model.as_deref(), "model"),
        (query.run.as_deref(), "run"),
        (query.valid_time.as_deref(), "valid_time"),
        (query.frame.as_deref(), "frame"),
        (query.tilt.as_deref(), "tilt"),
        (query.source.as_deref(), "source"),
        (query.source_kind.as_deref(), "source_kind"),
    ] {
        if let Some(filter) = normalized_optional_query(filter) {
            let Some(value) = weather_object_string_field(object, key) else {
                return false;
            };
            if value.to_ascii_lowercase() != filter {
                return false;
            }
        }
    }
    if let Some(product) = normalized_optional_query(query.product.as_deref()) {
        if !weather_object_product_matches(object, &product) {
            return false;
        }
    }
    if let Some(member) = normalized_optional_query(query.member.as_deref()) {
        if !weather_object_member_matches(object, &member) {
            return false;
        }
    }
    if let Some(ensemble_stat) = normalized_optional_query(query.ensemble_stat.as_deref()) {
        if !weather_object_ensemble_stat_matches(object, &ensemble_stat) {
            return false;
        }
    }
    if let Some(forecast_hour) = query.forecast_hour {
        if !weather_object_forecast_hour_matches(object, forecast_hour) {
            return false;
        }
    }
    if let Some(threshold) = normalized_optional_query(query.threshold.as_deref()) {
        if !weather_object_threshold_matches(object, &threshold) {
            return false;
        }
    }
    if let Some(network) = normalized_optional_query(query.network.as_deref()) {
        if !weather_object_network_matches(object, &network) {
            return false;
        }
    }
    if let Some(parameter) = normalized_optional_query(query.parameter.as_deref()) {
        if !weather_object_parameter_matches(object, &parameter) {
            return false;
        }
    }
    if let Some(quality_tier) = query.quality_tier {
        if !weather_object_quality_tier_matches(object, quality_tier) {
            return false;
        }
    }
    if query.max_age_minutes.is_some() && !weather_object_max_age_matches(object, query) {
        return false;
    }
    if !weather_object_spatial_matches(object, query) {
        return false;
    }
    if let Some(q) = normalized_optional_query(query.q.as_deref()) {
        object.to_string().to_ascii_lowercase().contains(&q)
    } else {
        true
    }
}

fn direct_observation_source_object(
    source_id: &str,
    source_kind: &str,
    record: &Value,
    items: Option<&Vec<Value>>,
) -> Value {
    json!({
        "schema": "wxstore.weather_object.v1",
        "id": format!("observation_source:{source_id}"),
        "kind": "observation_source",
        "category": direct_observation_object_category(source_kind),
        "lane": "direct_observations",
        "source": source_id,
        "source_kind": source_kind,
        "quality_tier": direct_observation_quality_tier(source_id, source_kind, None),
        "station_count": items.map(|items| items.len()).unwrap_or(0),
        "parameters": items
            .map(|items| weather_object_source_parameters(items))
            .unwrap_or_default(),
        "label": record.get("source_name").cloned().unwrap_or_else(|| json!(source_id)),
        "updated_at": record.get("fetched_at").cloned().unwrap_or_else(|| json!(null)),
        "url": format!("/v1/observations/sources/{source_id}"),
        "metadata": record,
    })
}

fn direct_observation_station_object(source_id: &str, source_kind: &str, item: &Value) -> Value {
    let station_id = item
        .get("station_id")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let object_kind = direct_observation_object_kind(source_kind);
    json!({
        "schema": "wxstore.weather_object.v1",
        "id": format!("{object_kind}:{source_id}:{station_id}"),
        "kind": object_kind,
        "category": direct_observation_object_category(source_kind),
        "lane": "direct_observations",
        "source": source_id,
        "source_kind": source_kind,
        "network": item.get("network").cloned().unwrap_or_else(|| json!(null)),
        "quality_tier": direct_observation_quality_tier(
            source_id,
            source_kind,
            item.get("network").and_then(Value::as_str),
        ),
        "station_id": station_id,
        "station_name": item.get("station_name").cloned().unwrap_or_else(|| json!(station_id)),
        "state": item.get("state").cloned().unwrap_or_else(|| json!(null)),
        "parameters": weather_object_available_parameters(item),
        "latitude": item.get("latitude").cloned().unwrap_or_else(|| json!(null)),
        "longitude": item.get("longitude").cloned().unwrap_or_else(|| json!(null)),
        "valid_time": item.get("timestamp").cloned().unwrap_or_else(|| json!(null)),
        "label": item.get("station_name").cloned().unwrap_or_else(|| json!(station_id)),
        "url": format!("/v1/observations/sources/{source_id}"),
        "metadata": item,
    })
}

fn weather_object_product_matches(object: &Value, product: &str) -> bool {
    let product = normalized_static_plot_product_key(product);
    weather_object_string_field_from_keys(object, &["product"])
        .map(|value| normalized_static_plot_product_key(&value) == product)
        .unwrap_or(false)
}

fn weather_object_member_matches(object: &Value, member: &str) -> bool {
    weather_object_string_field_from_keys(object, &["member", "member_key", "ensemble_key"])
        .map(|value| value == member)
        .unwrap_or(false)
}

fn weather_object_ensemble_stat_matches(object: &Value, ensemble_stat: &str) -> bool {
    weather_object_string_field_from_keys(object, &["ensemble_stat", "ensemble_statistic"])
        .map(|value| value == ensemble_stat)
        .unwrap_or(false)
}

fn weather_object_forecast_hour_matches(object: &Value, forecast_hour: u32) -> bool {
    weather_object_u32_field_from_keys(object, &["forecast_hour"])
        .map(|value| value == forecast_hour)
        .unwrap_or(false)
}

fn weather_object_threshold_matches(object: &Value, threshold: &str) -> bool {
    let mut candidates = Vec::new();
    weather_object_collect_threshold_values(object, &mut candidates);
    candidates
        .iter()
        .any(|value| weather_object_value_exact_matches(value, threshold))
}

fn weather_object_string_field_from_keys(object: &Value, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|key| weather_object_string_field(object, key))
        .or_else(|| {
            object.get("metadata").and_then(|metadata| {
                keys.iter()
                    .find_map(|key| weather_object_string_field(metadata, key))
            })
        })
}

fn weather_object_u32_field_from_keys(object: &Value, keys: &[&str]) -> Option<u32> {
    keys.iter()
        .find_map(|key| weather_object_u32_field(object, key))
        .or_else(|| {
            object.get("metadata").and_then(|metadata| {
                keys.iter()
                    .find_map(|key| weather_object_u32_field(metadata, key))
            })
        })
}

fn weather_object_u32_field(object: &Value, key: &str) -> Option<u32> {
    match object.get(key)? {
        Value::Number(value) => value.as_u64().and_then(|value| u32::try_from(value).ok()),
        Value::String(value) => {
            let value = value.trim();
            let value = value
                .strip_prefix('f')
                .or_else(|| value.strip_prefix('F'))
                .unwrap_or(value);
            value.parse::<u32>().ok()
        }
        _ => None,
    }
}

fn weather_object_first_threshold_value(object: &Value) -> Option<Value> {
    let mut candidates = Vec::new();
    weather_object_collect_threshold_values(object, &mut candidates);
    candidates.into_iter().next()
}

fn weather_object_collect_threshold_values(object: &Value, out: &mut Vec<Value>) {
    for key in [
        "threshold",
        "threshold_value",
        "value_threshold",
        "contour_threshold",
    ] {
        if let Some(value) = object.get(key).filter(|value| !value.is_null()) {
            out.push(value.clone());
        }
    }
    if let Some(values) = object.get("thresholds").and_then(Value::as_array) {
        out.extend(values.iter().filter(|value| !value.is_null()).cloned());
    }
    for key in ["metadata", "content_identity"] {
        if let Some(value) = object.get(key).filter(|value| value.is_object()) {
            weather_object_collect_threshold_values(value, out);
        }
    }
}

fn weather_object_value_exact_matches(value: &Value, filter: &str) -> bool {
    let filter_number = filter.parse::<f64>().ok();
    match value {
        Value::String(value) => {
            let normalized = normalized_optional_query(Some(value)).unwrap_or_default();
            normalized == filter
                || filter_number
                    .zip(value.trim().parse::<f64>().ok())
                    .is_some_and(|(a, b)| (a - b).abs() <= f64::EPSILON)
        }
        Value::Number(value) => value.as_f64().is_some_and(|value| {
            filter_number
                .map(|filter| (value - filter).abs() <= f64::EPSILON)
                .unwrap_or_else(|| value.to_string() == filter)
        }),
        Value::Bool(value) => value.to_string() == filter,
        _ => false,
    }
}

fn weather_object_network_matches(object: &Value, network: &str) -> bool {
    weather_object_string_field(object, "network")
        .or_else(|| {
            object
                .get("metadata")
                .and_then(|metadata| weather_object_string_field(metadata, "network"))
        })
        .or_else(|| {
            weather_object_string_field(object, "station_id").and_then(|station_id| {
                station_id
                    .split_once(':')
                    .map(|(prefix, _)| prefix.to_string())
            })
        })
        .is_some_and(|value| value == network)
}

fn weather_object_quality_tier_matches(object: &Value, quality_tier: u8) -> bool {
    if !(1..=5).contains(&quality_tier) {
        return false;
    }
    weather_object_number_field(object, "quality_tier")
        .map(|value| value as u8 == quality_tier && value.fract() == 0.0)
        .unwrap_or(false)
}

fn weather_object_parameter_matches(object: &Value, parameter: &str) -> bool {
    if weather_object_parameter_list_matches(object, parameter) {
        return true;
    }
    let Some(metadata) = object.get("metadata") else {
        return false;
    };
    let fields = weather_object_parameter_fields(parameter);
    if fields.is_empty() {
        return weather_object_meaningful_field(metadata, parameter);
    }
    fields
        .iter()
        .any(|field| weather_object_meaningful_field(metadata, field))
}

fn weather_object_parameter_list_matches(object: &Value, parameter: &str) -> bool {
    let Some(parameters) = object.get("parameters").and_then(Value::as_array) else {
        return false;
    };
    let query_fields = weather_object_parameter_fields(parameter);
    parameters.iter().filter_map(Value::as_str).any(|existing| {
        if existing.eq_ignore_ascii_case(parameter) {
            return true;
        }
        if query_fields.is_empty() {
            return false;
        }
        let existing_fields = weather_object_parameter_fields(&existing.to_ascii_lowercase());
        existing_fields
            .iter()
            .any(|field| query_fields.iter().any(|query_field| query_field == field))
    })
}

fn weather_object_available_parameters(metadata: &Value) -> Vec<&'static str> {
    weather_object_canonical_parameters()
        .iter()
        .copied()
        .filter(|parameter| {
            weather_object_parameter_fields(parameter)
                .iter()
                .any(|field| weather_object_meaningful_field(metadata, field))
        })
        .collect()
}

fn weather_object_source_parameters(items: &[Value]) -> Vec<&'static str> {
    let mut parameters = BTreeSet::new();
    for item in items {
        parameters.extend(weather_object_available_parameters(item));
    }
    parameters.into_iter().collect()
}

fn weather_object_canonical_parameters() -> &'static [&'static str] {
    &[
        "temperature",
        "air_temperature",
        "temperature_extremes",
        "water_temperature",
        "soil_temperature",
        "road_temperature",
        "dewpoint",
        "humidity",
        "wind",
        "pressure",
        "pressure_tendency",
        "visibility",
        "weather",
        "precipitation",
        "snow",
        "soil",
        "solar",
        "air_quality",
        "pm25",
        "pm10",
        "ozone",
        "wave_height",
        "streamflow",
        "stage",
        "stage_streamflow",
        "water_level",
        "tide",
        "reservoir",
        "road_surface",
    ]
}

fn weather_object_parameter_fields(parameter: &str) -> &'static [&'static str] {
    match parameter {
        "temperature" | "temp" => &[
            "temperature_f",
            "temperature_max_today_f",
            "temperature_min_today_f",
        ],
        "air_temperature" | "air_temp" | "t2m" => &["temperature_f"],
        "temperature_extremes" | "daily_temperature" => {
            &["temperature_max_today_f", "temperature_min_today_f"]
        }
        "water_temperature" | "water_temp" | "sst" => &["water_temperature_f"],
        "soil_temperature" | "soil_temp" => &["soil_temperature_4in_f"],
        "road_temperature" | "road_surface_temperature" | "pavement_temperature" => {
            &["road_surface_temperature_f"]
        }
        "dewpoint" | "dew_point" | "td" => &["dewpoint_f"],
        "humidity" | "rh" | "relative_humidity" => &["relative_humidity_pct"],
        "wind" | "wind_speed" | "wind_gust" | "gust" => {
            &["wind_speed_kts", "wind_gust_kts", "wind_direction_deg"]
        }
        "pressure" | "station_pressure" | "altimeter" | "mslp" => &[
            "station_pressure_mb",
            "sea_level_pressure_mb",
            "altimeter_inhg",
        ],
        "pressure_tendency" | "barometer_tendency" => &["pressure_tendency_mb"],
        "visibility" | "ceiling" | "flight_category" => {
            &["visibility_miles", "ceiling_ft", "cloud_cover"]
        }
        "weather" | "conditions" => &["weather_conditions"],
        "precip" | "precipitation" | "rain" | "rainfall" => {
            &["precipitation_1hr_in", "precipitation_accum_in"]
        }
        "snow" | "snowpack" | "swe" => &["snow_depth_in", "snow_water_equivalent_in"],
        "soil" | "soil_moisture" => &["soil_temperature_4in_f", "soil_moisture_pct"],
        "solar" | "radiation" | "solar_radiation" => &["solar_radiation_wm2"],
        "air_quality" | "aq" | "aqi" => &[
            "air_quality_index",
            "pm25_ugm3",
            "pm25_aqi",
            "pm10_ugm3",
            "pm10_aqi",
            "ozone_ppb",
            "ozone_aqi",
        ],
        "pm25" | "pm2_5" => &["pm25_ugm3", "pm25_aqi"],
        "pm10" => &["pm10_ugm3", "pm10_aqi"],
        "ozone" => &["ozone_ppb", "ozone_aqi"],
        "wave" | "waves" | "wave_height" => &[
            "wave_height_ft",
            "dominant_wave_period_s",
            "average_wave_period_s",
            "wave_direction_deg",
        ],
        "streamflow" | "flow" | "discharge" => &["streamflow_cfs"],
        "stage" | "gage_height" | "stream_stage" => &["gage_height_ft"],
        "stage_streamflow" | "river" => &["gage_height_ft", "streamflow_cfs"],
        "water_level" => &["water_level_ft"],
        "tide" => &["tide_ft"],
        "reservoir" => &["reservoir_elevation_ft"],
        "road" | "road_surface" | "pavement" => {
            &["road_surface_temperature_f", "road_surface_status"]
        }
        _ => &[],
    }
}

fn weather_object_meaningful_field(object: &Value, key: &str) -> bool {
    match object.get(key) {
        Some(Value::Number(value)) => value.as_f64().is_some_and(f64::is_finite),
        Some(Value::String(value)) => !value.trim().is_empty(),
        Some(Value::Bool(_)) => true,
        Some(Value::Array(values)) => !values.is_empty(),
        Some(Value::Object(values)) => !values.is_empty(),
        _ => false,
    }
}

fn weather_object_max_age_matches(object: &Value, query: &WeatherObjectQuery) -> bool {
    let Some(max_age_minutes) = query.max_age_minutes else {
        return true;
    };
    if !max_age_minutes.is_finite() || max_age_minutes < 0.0 {
        return false;
    }
    let Some(valid_time) = weather_object_valid_time(object) else {
        return false;
    };
    let age_seconds = chrono::Utc::now()
        .signed_duration_since(valid_time)
        .num_seconds()
        .max(0) as f64;
    age_seconds <= max_age_minutes * 60.0
}

fn weather_object_valid_time(object: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = object
        .get("valid_time")
        .and_then(Value::as_str)
        .or_else(|| object.get("updated_at").and_then(Value::as_str))
        .or_else(|| {
            object
                .get("metadata")
                .and_then(|metadata| metadata.get("timestamp"))
                .and_then(Value::as_str)
        })?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.with_timezone(&chrono::Utc))
}

fn direct_observation_object_kind(source_kind: &str) -> &'static str {
    match source_kind {
        "marine_current_observation" => "marine_observation",
        "hydro_current_observation" => "hydro_observation",
        "hydro_forecast_status" => "hydro_forecast_observation",
        "flash_flood_current_observation" => "flash_flood_observation",
        "coastal_meteorology_current" => "coastal_meteorology_observation",
        "coastal_water_current" => "coastal_water_observation",
        "air_quality_current_observation" => "air_quality_observation",
        "raws_fire_danger_daily" => "fire_danger_observation",
        "coop_daily_climate" => "climate_observation",
        _ => "surface_observation",
    }
}

fn direct_observation_object_category(source_kind: &str) -> &'static str {
    match source_kind {
        "marine_current_observation" | "coastal_meteorology_current" => "ocean",
        "hydro_current_observation"
        | "hydro_forecast_status"
        | "flash_flood_current_observation"
        | "coastal_water_current" => "water",
        "air_quality_current_observation" => "air_quality",
        "raws_current_weather" | "raws_fire_danger_daily" => "fire",
        "coop_daily_climate" => "climate",
        _ => "weather",
    }
}

fn direct_observation_quality_tier(
    source_id: &str,
    source_kind: &str,
    network: Option<&str>,
) -> u8 {
    let source_id = source_id.to_ascii_lowercase();
    let network = network.unwrap_or_default().to_ascii_lowercase();
    match source_kind {
        "asos_awos_metar"
        | "marine_current_observation"
        | "hydro_forecast_status"
        | "coastal_water_current"
        | "coastal_meteorology_current"
        | "air_quality_current_observation"
        | "snotel_hourly"
        | "scan_hourly" => 1,
        "hydro_current_observation" => {
            if source_id.starts_with("usgs_") || source_id.starts_with("noaa_") {
                1
            } else {
                3
            }
        }
        "mesonet_current_5min"
        | "mesonet_5min"
        | "mesonet_current_15min"
        | "mesonet_hourly_ag_weather"
        | "raws_current_weather" => 2,
        "rwis_current" | "flash_flood_current_observation" => 3,
        "raws_fire_danger_daily" | "coop_daily_climate" => 4,
        _ => {
            if matches!(
                network.as_str(),
                "asos" | "awos" | "metar" | "ndbc" | "co-ops" | "co-ops_met" | "airnow"
            ) {
                1
            } else if network.contains("mesonet") {
                2
            } else if network.is_empty() {
                5
            } else {
                3
            }
        }
    }
}

fn weather_object_spatial_matches(object: &Value, query: &WeatherObjectQuery) -> bool {
    if let Some(bbox) = normalized_optional_query(query.bbox.as_deref()) {
        let Some((min_lon, min_lat, max_lon, max_lat)) = parse_weather_object_bbox(&bbox) else {
            return false;
        };
        let Some((lat, lon)) = weather_object_coordinates(object) else {
            return false;
        };
        if lat < min_lat || lat > max_lat {
            return false;
        }
        let lon = normalize_lon(lon);
        let min_lon = normalize_lon(min_lon);
        let max_lon = normalize_lon(max_lon);
        if min_lon <= max_lon {
            if lon < min_lon || lon > max_lon {
                return false;
            }
        } else if lon < min_lon && lon > max_lon {
            return false;
        }
    }

    if query.lat.is_some() || query.lon.is_some() || query.radius_km.is_some() {
        let (Some(query_lat), Some(query_lon)) = (query.lat, query.lon) else {
            return false;
        };
        if !query_lat.is_finite() || !query_lon.is_finite() || !(-90.0..=90.0).contains(&query_lat)
        {
            return false;
        }
        let radius_km = query.radius_km.unwrap_or(50.0);
        if !radius_km.is_finite() || radius_km < 0.0 {
            return false;
        }
        let Some((lat, lon)) = weather_object_coordinates(object) else {
            return false;
        };
        if weather_object_distance_km(query_lat, query_lon, lat, lon) > radius_km {
            return false;
        }
    }

    true
}

fn parse_weather_object_bbox(value: &str) -> Option<(f64, f64, f64, f64)> {
    let parts = value
        .split(',')
        .map(str::trim)
        .map(str::parse::<f64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if parts.len() != 4 || parts.iter().any(|value| !value.is_finite()) {
        return None;
    }
    let min_lon = parts[0];
    let max_lon = parts[2];
    let min_lat = parts[1].min(parts[3]);
    let max_lat = parts[1].max(parts[3]);
    if !(-90.0..=90.0).contains(&min_lat)
        || !(-90.0..=90.0).contains(&max_lat)
        || !(-180.0..=180.0).contains(&min_lon)
        || !(-180.0..=180.0).contains(&max_lon)
    {
        return None;
    }
    Some((min_lon, min_lat, max_lon, max_lat))
}

fn weather_object_coordinates(object: &Value) -> Option<(f64, f64)> {
    let lat = weather_object_number_field(object, "latitude").or_else(|| {
        object
            .get("metadata")
            .and_then(|metadata| weather_object_number_field(metadata, "latitude"))
    })?;
    let lon = weather_object_number_field(object, "longitude").or_else(|| {
        object
            .get("metadata")
            .and_then(|metadata| weather_object_number_field(metadata, "longitude"))
    })?;
    (lat.is_finite() && lon.is_finite() && (-90.0..=90.0).contains(&lat)).then_some((lat, lon))
}

fn weather_object_distance_km(from_lat: f64, from_lon: f64, to_lat: f64, to_lon: f64) -> f64 {
    let from_lat = from_lat.to_radians();
    let to_lat = to_lat.to_radians();
    let dlat = to_lat - from_lat;
    let dlon = normalized_lon_delta(to_lon - from_lon).to_radians();
    let half_dlat = (dlat / 2.0).sin();
    let half_dlon = (dlon / 2.0).sin();
    let haversine = half_dlat * half_dlat + from_lat.cos() * to_lat.cos() * half_dlon * half_dlon;
    let central_angle = 2.0 * haversine.clamp(0.0, 1.0).sqrt().asin();
    RADAR_EARTH_AUTHALIC_RADIUS_M * central_angle / 1000.0
}

fn weather_object_number_field(object: &Value, key: &str) -> Option<f64> {
    match object.get(key)? {
        Value::Number(value) => value.as_f64(),
        Value::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn weather_object_string_field(object: &Value, key: &str) -> Option<String> {
    match object.get(key)? {
        Value::String(value) => Some(value.to_ascii_lowercase()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn store_status(
    profile: Option<&ProfileLane>,
    diagnostic: Option<&DiagnosticLane>,
    spatial: Option<&SpatialLane>,
    static_plots: Option<&StaticPlotLane>,
    evidence: Option<&EvidenceBundleLane>,
    observations: Option<&ObservationLane>,
    mesoanalysis_innovation: Option<&MesoanalysisInnovationLane>,
    satellite_tiles: Option<&SatelliteTileLane>,
    radar_tiles: Option<&RadarTileLane>,
    cache: Option<CacheStats>,
    archive: Option<&ArchiveLane>,
) -> Value {
    let spatial_models = spatial.map(SpatialLane::model_ids).unwrap_or_default();
    let profile_ready = profile
        .map(|profile| {
            !profile.manifest.variables.is_empty()
                && !profile.manifest.forecast_hours.is_empty()
                && !profile.manifest.levels_hpa.is_empty()
        })
        .unwrap_or(false);
    let spatial_ready = spatial.map(|_| !spatial_models.is_empty()).unwrap_or(true);
    let static_plots_ready = static_plots.is_some();
    let evidence_ready = evidence.is_some();
    let observations_ready = observations.is_some();
    let mesoanalysis_innovation_ready = mesoanalysis_innovation.is_some();
    let satellite_ready = satellite_tiles.is_some();
    let radar_ready = radar_tiles.is_some();
    let ok = if spatial.is_some() {
        spatial_ready
    } else if static_plots.is_some() {
        static_plots_ready
    } else if satellite_tiles.is_some() || radar_tiles.is_some() {
        satellite_ready || radar_ready
    } else if evidence.is_some() || observations.is_some() || mesoanalysis_innovation.is_some() {
        true
    } else {
        profile_ready
    };
    let cache_stats = cache.unwrap_or_else(default_cache_stats);
    json!({
        "schema": "wxstore.status.v1",
        "service": "wxstore",
        "ok": ok,
        "readiness": {
            "profile_pressure_core": if profile_ready { "ready" } else { "unavailable" },
            "diag_scalar_basic": if diagnostic.is_some() { "ready_sparse_v0" } else { "unavailable" },
            "surface_spatial": if spatial.is_some() {
                if spatial_ready { "ready" } else { "empty" }
            } else {
                "unavailable"
            },
            "static_plots": if static_plots_ready { "ready" } else { "unavailable" },
            "evidence_bundles": if evidence_ready { "ready" } else { "unavailable" },
            "direct_observations": if observations_ready { "ready" } else { "unavailable" },
            "mesoanalysis_innovation": if mesoanalysis_innovation_ready { "ready" } else { "unavailable" },
            "satellite_tiles": if satellite_ready { "ready" } else { "unavailable" },
            "radar_tiles": if radar_ready { "ready" } else { "unavailable" },
            "spatial_models": spatial_models
        },
        "loaded_run": profile
            .map(|profile| run_manifest_json(profile, diagnostic, spatial))
            .unwrap_or_else(|| json!({"status": "unavailable", "reason": "profile store is not configured"})),
        "lanes": {
            "profile_pressure_core": profile.map(ProfileLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "diag_scalar_basic": diagnostic.map(DiagnosticLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "surface_spatial": spatial.map(SpatialLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "static_plots": static_plots.map(StaticPlotLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "evidence_bundles": evidence.map(EvidenceBundleLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "direct_observations": observations.map(ObservationLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "mesoanalysis_innovation": mesoanalysis_innovation.map(MesoanalysisInnovationLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "satellite_tiles": satellite_tiles.map(SatelliteTileLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "radar_tiles": radar_tiles.map(RadarTileLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "archive": archive.map(ArchiveLane::status_json).unwrap_or_else(|| json!({"status": "unavailable"}))
        },
        "monitoring": monitoring_status(profile, diagnostic, spatial, static_plots, evidence, observations, mesoanalysis_innovation, &cache_stats),
        "cache": cache_stats
    })
}

fn readiness_status(
    profile: Option<&ProfileLane>,
    diagnostic: Option<&DiagnosticLane>,
    spatial: Option<&SpatialLane>,
    static_plots: Option<&StaticPlotLane>,
    evidence: Option<&EvidenceBundleLane>,
    observations: Option<&ObservationLane>,
    mesoanalysis_innovation: Option<&MesoanalysisInnovationLane>,
    satellite_tiles: Option<&SatelliteTileLane>,
    radar_tiles: Option<&RadarTileLane>,
) -> Value {
    let spatial_models = spatial.map(SpatialLane::model_ids).unwrap_or_default();
    let profile_ready = profile
        .map(|profile| {
            !profile.manifest.variables.is_empty()
                && !profile.manifest.forecast_hours.is_empty()
                && !profile.manifest.levels_hpa.is_empty()
        })
        .unwrap_or(false);
    let spatial_ready = spatial.map(|_| !spatial_models.is_empty()).unwrap_or(true);
    let satellite_ready = satellite_tiles.is_some();
    let radar_ready = radar_tiles.is_some();
    let ok = if spatial.is_some() {
        spatial_ready
    } else if static_plots.is_some() {
        true
    } else if satellite_tiles.is_some() || radar_tiles.is_some() {
        satellite_ready || radar_ready
    } else if evidence.is_some() || observations.is_some() || mesoanalysis_innovation.is_some() {
        true
    } else {
        profile_ready
    };
    json!({
        "schema": "wxstore.health.v1",
        "service": "wxstore",
        "kind": "ready",
        "ok": ok,
        "readiness": {
            "profile_pressure_core": if profile_ready { "ready" } else { "unavailable" },
            "diag_scalar_basic": if diagnostic.is_some() { "ready_sparse_v0" } else { "unavailable" },
            "surface_spatial": if spatial.is_some() {
                if spatial_ready { "ready" } else { "empty" }
            } else {
                "unavailable"
            },
            "static_plots": if static_plots.is_some() { "ready" } else { "unavailable" },
            "evidence_bundles": if evidence.is_some() { "ready" } else { "unavailable" },
            "direct_observations": if observations.is_some() { "ready" } else { "unavailable" },
            "mesoanalysis_innovation": if mesoanalysis_innovation.is_some() { "ready" } else { "unavailable" },
            "satellite_tiles": if satellite_ready { "ready" } else { "unavailable" },
            "radar_tiles": if radar_ready { "ready" } else { "unavailable" },
            "spatial_models": spatial_models
        }
    })
}

fn default_cache_stats() -> CacheStats {
    CacheStats {
        entries: 0,
        entries_limit: CACHE_LIMIT,
        bytes: 0,
        bytes_limit: CACHE_BYTES_LIMIT,
        hits: 0,
        misses: 0,
        evictions: 0,
    }
}

fn monitoring_status(
    profile: Option<&ProfileLane>,
    diagnostic: Option<&DiagnosticLane>,
    spatial: Option<&SpatialLane>,
    static_plots: Option<&StaticPlotLane>,
    evidence: Option<&EvidenceBundleLane>,
    observations: Option<&ObservationLane>,
    mesoanalysis_innovation: Option<&MesoanalysisInnovationLane>,
    cache: &CacheStats,
) -> Value {
    let profile_hours = profile
        .map(|profile| {
            profile
                .manifest
                .forecast_hours
                .iter()
                .map(|hour| u32::from(*hour))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let spatial_monitoring = spatial
        .map(spatial_monitoring_json)
        .unwrap_or_else(|| json!({"status": "unavailable"}));
    let spatial_latest_runs = spatial
        .map(|lane| {
            lane.model_ids()
                .into_iter()
                .map(|model| {
                    json!({
                        "model": model,
                        "latest_run": lane.latest_run_for_model(&model)
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let static_plot_summary = static_plots
        .map(StaticPlotLane::summary_json)
        .unwrap_or_else(|| json!({"status": "unavailable", "coverage": []}));
    let evidence_summary = evidence
        .map(EvidenceBundleLane::summary_json)
        .unwrap_or_else(|| json!({"status": "unavailable", "bundle_count": 0}));
    let observation_summary = observations
        .map(ObservationLane::summary_json)
        .unwrap_or_else(
            || json!({"status": "unavailable", "source_count": 0, "observation_count": 0}),
        );
    let innovation_summary = mesoanalysis_innovation
        .map(MesoanalysisInnovationLane::summary_json)
        .unwrap_or_else(|| {
            json!({
                "status": "unavailable",
                "history_case_count": 0,
                "station_series_count": 0,
                "source_series_count": 0
            })
        });
    json!({
        "schema": "wxstore.monitoring.v1",
        "generated_at": utc_now_string(),
        "latest_runs": {
            "profile_pressure_core": {
                "model": profile.map(|profile| profile.manifest.model.as_str()),
                "domain": profile.map(|profile| profile.manifest.domain.as_str()),
                "run_id": profile.map(|profile| profile.manifest.run_id.as_str()),
                "cycle": profile.map(|profile| profile.manifest.cycle.as_str())
            },
            "surface_spatial": spatial_latest_runs
        },
        "coverage": {
            "profile_forecast_hours": profile_hours,
            "surface_spatial": spatial_monitoring.get("models").cloned().unwrap_or_else(|| json!([]))
        },
        "product_completeness": {
            "profile_pressure_core": {
                "status": if profile.is_some() { "complete" } else { "unavailable" },
                "variable_count": profile.map(|profile| profile.manifest.variables.len()).unwrap_or(0),
                "forecast_hour_count": profile.map(|profile| profile.manifest.forecast_hours.len()).unwrap_or(0),
                "level_count": profile.map(|profile| profile.manifest.levels_hpa.len()).unwrap_or(0)
            },
            "diag_scalar_basic": {
                "status": if diagnostic.is_some() { "ready_sparse_v0" } else { "unavailable" }
            },
            "surface_spatial": spatial_monitoring.get("product_completeness").cloned().unwrap_or_else(|| json!([]))
        },
        "static_plots": {
            "status": static_plot_summary.get("status").cloned().unwrap_or_else(|| json!("unavailable")),
            "root": static_plot_summary.get("root").cloned().unwrap_or_else(|| json!(null)),
            "manifest_count": static_plot_summary.get("manifest_count").cloned().unwrap_or_else(|| json!(0)),
            "artifact_count": static_plot_summary.get("artifact_count").cloned().unwrap_or_else(|| json!(0)),
            "complete_count": static_plot_summary.get("complete_count").cloned().unwrap_or_else(|| json!(0)),
            "blocked_count": static_plot_summary.get("blocked_count").cloned().unwrap_or_else(|| json!(0)),
            "failed_count": static_plot_summary.get("failed_count").cloned().unwrap_or_else(|| json!(0)),
            "coverage": static_plot_summary.get("coverage").cloned().unwrap_or_else(|| json!([]))
        },
        "evidence_bundles": {
            "status": evidence_summary.get("status").cloned().unwrap_or_else(|| json!("unavailable")),
            "root": evidence_summary.get("root").cloned().unwrap_or_else(|| json!(null)),
            "bundle_root": evidence_summary.get("bundle_root").cloned().unwrap_or_else(|| json!(null)),
            "bundle_count": evidence_summary.get("bundle_count").cloned().unwrap_or_else(|| json!(0))
        },
        "direct_observations": {
            "status": observation_summary.get("status").cloned().unwrap_or_else(|| json!("unavailable")),
            "root": observation_summary.get("root").cloned().unwrap_or_else(|| json!(null)),
            "source_count": observation_summary.get("source_count").cloned().unwrap_or_else(|| json!(0)),
            "observation_count": observation_summary.get("observation_count").cloned().unwrap_or_else(|| json!(0)),
            "raw_record_count": observation_summary.get("raw_record_count").cloned().unwrap_or_else(|| json!(0))
        },
        "mesoanalysis_innovation": {
            "status": innovation_summary.get("status").cloned().unwrap_or_else(|| json!("unavailable")),
            "root": innovation_summary.get("root").cloned().unwrap_or_else(|| json!(null)),
            "history_case_count": innovation_summary.get("history_case_count").cloned().unwrap_or_else(|| json!(0)),
            "station_series_count": innovation_summary.get("station_series_count").cloned().unwrap_or_else(|| json!(0)),
            "source_series_count": innovation_summary.get("source_series_count").cloned().unwrap_or_else(|| json!(0))
        },
        "timing": {
            "surface_spatial_ingest": spatial_monitoring.get("ingest_timing").cloned().unwrap_or_else(|| json!([])),
            "surface_spatial_export": spatial_monitoring.get("export_timing").cloned().unwrap_or_else(|| json!([]))
        },
        "disk": {
            "surface_spatial": spatial_monitoring.get("disk").cloned().unwrap_or_else(|| json!({"status": "unavailable"})),
            "response_cache_bytes": cache.bytes,
            "response_cache_limit_bytes": cache.bytes_limit
        },
        "api_health": {
            "livez": "configured",
            "readyz": if profile.is_some() || spatial.is_some() || static_plots.is_some() || evidence.is_some() || observations.is_some() || mesoanalysis_innovation.is_some() { "configured" } else { "profile_unavailable" },
            "sample_api": if spatial.is_some() { "configured" } else { "unavailable" },
            "tile_api": if spatial.is_some() { "configured" } else { "unavailable" },
            "tilejson_api": if spatial.is_some() { "configured" } else { "unavailable" }
        }
    })
}

fn spatial_monitoring_json(spatial: &SpatialLane) -> Value {
    let mut product_completeness = Vec::new();
    let mut ingest_timing = Vec::new();
    let mut export_timing = Vec::new();
    let models = spatial
        .model_ids()
        .into_iter()
        .map(|model| {
            let runs = spatial.runs_for_model(&model);
            let latest_run = spatial.latest_run_for_model(&model);
            let run_reports = runs
                .iter()
                .map(|run| {
                    let report = spatial_run_monitoring_json(&spatial.root, &model, run);
                    if let Some(value) = report.get("product_completeness") {
                        product_completeness.push(value.clone());
                    }
                    if let Some(values) = report.get("ingest_timing").and_then(Value::as_array) {
                        ingest_timing.extend(values.iter().cloned());
                    }
                    if let Some(values) = report.get("export_timing").and_then(Value::as_array) {
                        export_timing.extend(values.iter().cloned());
                    }
                    report
                })
                .collect::<Vec<_>>();
            json!({
                "model": model,
                "latest_run": latest_run,
                "run_count": runs.len(),
                "runs": run_reports
            })
        })
        .collect::<Vec<_>>();
    json!({
        "status": if models.is_empty() { "empty" } else { "ready" },
        "root": display_path(&spatial.root),
        "models": models,
        "product_completeness": product_completeness,
        "ingest_timing": ingest_timing,
        "export_timing": export_timing,
        "disk": {
            "root": display_path(&spatial.root),
            "root_bytes": sum_dir_bytes(&spatial.root),
            "wxa_bytes": spatial_wxa_bytes(spatial)
        }
    })
}

fn spatial_run_monitoring_json(root: &Path, model: &str, run: &str) -> Value {
    let run_dir = root.join(model).join(run);
    let manifest_path = run_manifest_path(root, model, run);
    let manifest = fs::read(&manifest_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    let products = manifest
        .as_ref()
        .and_then(|value| value.get("products").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();
    let sources = manifest
        .as_ref()
        .and_then(|value| value.get("sources").and_then(Value::as_array))
        .cloned()
        .unwrap_or_default();
    let mut hours = BTreeSet::new();
    let product_coverage = products
        .iter()
        .map(|product| {
            let product_hours = product
                .get("forecast_hours")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for hour in &product_hours {
                if let Some(hour) = hour.as_u64() {
                    hours.insert(hour);
                }
            }
            json!({
                "product": product.get("product").cloned().unwrap_or_else(|| json!(null)),
                "member": product.get("member").cloned().unwrap_or_else(|| json!(null)),
                "forecast_hours": product_hours,
                "units": product.get("units").cloned().unwrap_or_else(|| json!(null)),
                "bytes": product.get("bytes").cloned().unwrap_or_else(|| json!(0)),
                "grid": product.get("grid").cloned().unwrap_or_else(|| json!(null))
            })
        })
        .collect::<Vec<_>>();
    let blockers = sources
        .iter()
        .flat_map(|source| {
            source
                .get("blockers")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        })
        .collect::<Vec<_>>();
    let ingest_timing = sources
        .iter()
        .map(|source| {
            json!({
                "model": model,
                "run": run,
                "kind": source.get("kind").cloned().unwrap_or_else(|| json!(null)),
                "imported_at": source.get("imported_at").cloned().unwrap_or_else(|| json!(null)),
                "elapsed_ms": source.get("elapsed_ms").cloned().unwrap_or_else(|| json!(null)),
                "blocker_count": source.get("blocker_count").cloned().unwrap_or_else(|| json!(0))
            })
        })
        .collect::<Vec<_>>();
    let export_timing = sources
        .iter()
        .filter_map(|source| source.get("source_manifest").and_then(Value::as_str))
        .map(|source_manifest| {
            let manifest_value = fs::read(source_manifest)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
            json!({
                "model": model,
                "run": run,
                "source_manifest": source_manifest,
                "timing": manifest_value
                    .as_ref()
                    .and_then(|value| value.get("timing").cloned())
                    .unwrap_or_else(|| json!(null))
            })
        })
        .collect::<Vec<_>>();
    let product_count = products.len();
    let status = if product_count == 0 {
        "unavailable"
    } else if blockers.is_empty() {
        "complete_for_manifest"
    } else {
        "partial_with_blockers"
    };
    json!({
        "run": run,
        "path": display_path(&run_dir),
        "run_manifest": if manifest_path.is_file() { json!(display_path(&manifest_path)) } else { json!(null) },
        "domains": domains_from_run_manifest(manifest.as_ref()),
        "forecast_hours": hours.into_iter().collect::<Vec<_>>(),
        "product_count": product_count,
        "products": product_coverage,
        "blocker_count": blockers.len(),
        "blockers": blockers,
        "product_completeness": {
            "model": model,
            "run": run,
            "status": status,
            "product_count": product_count,
            "blocker_count": blockers.len()
        },
        "ingest_timing": ingest_timing,
        "export_timing": export_timing,
        "bytes": sum_dir_bytes(&run_dir)
    })
}

fn domains_from_run_manifest(manifest: Option<&Value>) -> Vec<String> {
    let mut domains = BTreeSet::new();
    if let Some(sources) = manifest
        .and_then(|value| value.get("sources"))
        .and_then(Value::as_array)
    {
        for source in sources {
            let Some(source_manifest) = source.get("source_manifest").and_then(Value::as_str)
            else {
                continue;
            };
            if let Some(domain) = domain_from_source_manifest(source_manifest) {
                domains.insert(domain);
            }
        }
    }
    domains.into_iter().collect()
}

fn domain_from_source_manifest(source_manifest: &str) -> Option<String> {
    let value = fs::read(source_manifest)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())?;
    value
        .get("domain")
        .and_then(|domain| domain.get("slug"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn spatial_wxa_bytes(spatial: &SpatialLane) -> u64 {
    spatial
        .model_ids()
        .iter()
        .flat_map(|model| {
            spatial
                .runs_for_model(model)
                .into_iter()
                .map(|run| spatial.root.join(model).join(run))
                .collect::<Vec<_>>()
        })
        .map(|run_dir| sum_wxa_bytes(&run_dir))
        .sum()
}

fn sum_dir_bytes(dir: &Path) -> u64 {
    if !dir.is_dir() {
        return 0;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .map(|path| {
            fs::metadata(&path)
                .map(|meta| {
                    if meta.is_file() {
                        meta.len()
                    } else if meta.is_dir() {
                        sum_dir_bytes(&path)
                    } else {
                        0
                    }
                })
                .unwrap_or(0)
        })
        .sum()
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

fn wxa_grid_metadata_compatible(existing: &Value, incoming: &Value) -> bool {
    if existing == incoming {
        return true;
    }

    let existing_type = existing.get("type").and_then(Value::as_str);
    let incoming_type = incoming.get("type").and_then(Value::as_str);
    if existing_type != incoming_type {
        return false;
    }

    if existing_type == Some("curvilinear_latlon_sampled") {
        return [
            "type",
            "nx",
            "ny",
            "bounds",
            "corners",
            "monotonic",
            "sample_strategy",
        ]
        .iter()
        .all(|key| existing.get(*key) == incoming.get(*key));
    }

    false
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
                    "latest_run": self.latest_run_for_model(&model),
                    "latest_pointer": read_latest_pointer_value(&self.root, &model).unwrap_or_else(|| json!({"status": "missing"})),
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
                let latest = self.latest_run_for_model(&model);
                let domains = latest
                    .as_deref()
                    .and_then(|run| {
                        fs::read(run_manifest_path(&self.root, &model, run))
                            .ok()
                            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    })
                    .map(|manifest| domains_from_run_manifest(Some(&manifest)))
                    .unwrap_or_default();
                json!({
                    "id": model,
                    "runs": runs,
                    "latest_run": latest,
                    "latest_readiness": {
                        "status": if latest.is_some() { "available" } else { "unavailable" },
                        "domains": domains
                    }
                })
            })
            .collect::<Vec<_>>();
        json!({
            "status": "ready",
            "root": self.root,
            "models": models
        })
    }

    fn run_readiness(&self, model: &str, run: &str) -> Value {
        let run_dir = self.root.join(model).join(run);
        if !run_dir.is_dir() {
            return json!({
                "status": "unavailable",
                "reason": "run directory is missing"
            });
        }
        let manifest_path = run_manifest_path(&self.root, model, run);
        let manifest = fs::read(&manifest_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        let variables = self.variables_for(model, run, None).unwrap_or_default();
        let mut available_hours = Map::new();
        for variable in &variables {
            if let Ok(hours) = self.available_hours_for(model, run, None, variable) {
                available_hours.insert(variable.clone(), json!(hours));
            }
        }
        let has_data = !variables.is_empty();
        let status = if manifest.is_some() && has_data {
            "complete"
        } else if has_data {
            "partial_legacy"
        } else {
            "unavailable"
        };
        json!({
            "status": status,
            "run_manifest": if manifest_path.is_file() { json!(relative_path_string(&self.root, &manifest_path)) } else { json!(null) },
            "domains": domains_from_run_manifest(manifest.as_ref()),
            "variables": variables,
            "available_hours": available_hours,
            "members": self.members_for(model, run),
            "product_count": manifest
                .as_ref()
                .and_then(|value| value.get("product_count").and_then(Value::as_u64))
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
        self.latest_run_for_model(model)
            .ok_or_else(|| not_found(format!("no runs are available for model '{model}'")))
    }

    fn runs_for_model(&self, model: &str) -> Vec<String> {
        list_dirs(&self.root.join(model))
    }

    fn latest_run_for_model(&self, model: &str) -> Option<String> {
        read_latest_pointer_run(&self.root, model)
            .or_else(|| self.runs_for_model(model).last().cloned())
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
            let (meta, _) = read_wxa_dense2d_metadata(&wxa_path)?;
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
        let variable = storage_variable_alias(variable).unwrap_or(variable);
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
        let variable = storage_variable_alias(variable).unwrap_or(variable);
        let member_key = member.unwrap_or("-");
        let wxa_path = self.wxa_file_path(model, run, member, variable);
        let cache_key = if wxa_path.is_file() {
            format!(
                "wxa|{model}|{run}|{member_key}|{variable}|{forecast_hour}|{}|{}",
                wxa_path.display(),
                file_cache_token(&wxa_path)
            )
        } else {
            format!("zarr|{model}|{run}|{member_key}|{variable}|{forecast_hour}")
        };
        if let Some(grid) = self
            .grid_cache
            .read()
            .ok()
            .and_then(|cache| cache.get(&cache_key).cloned())
        {
            return Ok((*grid).clone());
        }

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

    fn grid_source_cache_token(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
        variable: &str,
        forecast_hour: u32,
    ) -> Result<String> {
        let variable = storage_variable_alias(variable).unwrap_or(variable);
        if self.array_exists(model, run, member, variable) {
            return Ok(self.raw_grid_source_cache_token(
                model,
                run,
                member,
                variable,
                forecast_hour,
            ));
        }
        if let Some(raw) = raw_variable_for_product(variable) {
            return Ok(self.raw_grid_source_cache_token(model, run, member, raw, forecast_hour));
        }
        if let Some(deps) = cheap_derived_dependencies(variable) {
            let tokens = deps
                .iter()
                .map(|dep| {
                    self.grid_source_cache_token(model, run, member, dep, forecast_hour)
                        .unwrap_or_else(|_| format!("missing:{dep}:f{forecast_hour:03}"))
                })
                .collect::<Vec<_>>();
            return Ok(format!("derived:{variable}:{}", tokens.join("+")));
        }
        if let Some(window) = parse_windowed_product(variable) {
            let hours = self.available_hours_for(model, run, member, window.raw_variable)?;
            let tokens = hours
                .into_iter()
                .filter(|hour| *hour >= window.start && *hour <= window.end)
                .map(|hour| {
                    self.raw_grid_source_cache_token(model, run, member, window.raw_variable, hour)
                })
                .collect::<Vec<_>>();
            return Ok(format!("window:{variable}:{}", tokens.join("+")));
        }
        Ok(format!("source:{variable}:f{forecast_hour:03}"))
    }

    fn raw_grid_source_cache_token(
        &self,
        model: &str,
        run: &str,
        member: Option<&str>,
        variable: &str,
        forecast_hour: u32,
    ) -> String {
        let variable = storage_variable_alias(variable).unwrap_or(variable);
        let wxa_path = self.wxa_file_path(model, run, member, variable);
        if wxa_path.is_file() {
            return format!(
                "wxa:{}:f{forecast_hour:03}:{}",
                wxa_path.display(),
                file_cache_token(&wxa_path)
            );
        }
        match self.array_dir(model, run, member, variable) {
            Ok(array_dir) => format!("zarr:{}:f{forecast_hour:03}", array_dir.display()),
            Err(_) => format!("missing:{variable}:f{forecast_hour:03}"),
        }
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
    let first_grid_meta = first.grid_meta();

    let cy = WXA_SPATIAL_CHUNK_Y.min(first.ny);
    let cx = WXA_SPATIAL_CHUNK_X.min(first.nx);
    let n_chunks_y = first.ny.div_ceil(cy);
    let n_chunks_x = first.nx.div_ceil(cx);

    let mut records = Vec::<WxaDense2dIndexRecord>::new();
    let mut payload = Vec::<u8>::new();

    let incoming_hours = grids
        .iter()
        .map(|grid| grid.forecast_hour)
        .collect::<BTreeSet<_>>();
    if path.is_file() {
        let (existing_bytes, existing_meta, existing_records) = read_wxa_dense2d(&path)
            .with_context(|| format!("read existing WXA metadata {}", path.display()))?;
        let mut incompatibilities = Vec::new();
        if existing_meta.model != model {
            incompatibilities.push(format!(
                "model existing={} incoming={}",
                existing_meta.model, model
            ));
        }
        if existing_meta.run != run {
            incompatibilities.push(format!(
                "run existing={} incoming={}",
                existing_meta.run, run
            ));
        }
        if existing_meta.member.as_deref() != member {
            incompatibilities.push(format!(
                "member existing={:?} incoming={:?}",
                existing_meta.member.as_deref(),
                member
            ));
        }
        if existing_meta.variable != product {
            incompatibilities.push(format!(
                "variable existing={} incoming={}",
                existing_meta.variable, product
            ));
        }
        if existing_meta.nx != first.nx || existing_meta.ny != first.ny {
            incompatibilities.push(format!(
                "shape existing={}x{} incoming={}x{}",
                existing_meta.nx, existing_meta.ny, first.nx, first.ny
            ));
        }
        if existing_meta.chunk_y != cy || existing_meta.chunk_x != cx {
            incompatibilities.push(format!(
                "chunk existing={}x{} incoming={}x{}",
                existing_meta.chunk_x, existing_meta.chunk_y, cx, cy
            ));
        }
        if existing_meta.dtype != "f32_le" {
            incompatibilities.push(format!("dtype existing={}", existing_meta.dtype));
        }
        if existing_meta.codec != "zstd_level_1" {
            incompatibilities.push(format!("codec existing={}", existing_meta.codec));
        }
        if existing_meta.units != first.units {
            incompatibilities.push(format!(
                "units existing={} incoming={}",
                existing_meta.units, first.units
            ));
        }
        if !wxa_grid_metadata_compatible(&existing_meta.grid, &first_grid_meta) {
            let existing_grid = serde_json::to_string(&existing_meta.grid).unwrap_or_default();
            let incoming_grid = serde_json::to_string(&first_grid_meta).unwrap_or_default();
            incompatibilities.push(format!(
                "grid metadata differs existing_type={} incoming_type={} existing_len={} incoming_len={} existing_bounds={} incoming_bounds={}",
                existing_meta
                    .grid
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                first_grid_meta
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown"),
                existing_grid.len(),
                incoming_grid.len(),
                existing_meta
                    .grid
                    .get("bounds")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "null".to_string()),
                first_grid_meta
                    .get("bounds")
                    .map(Value::to_string)
                    .unwrap_or_else(|| "null".to_string())
            ));
        }
        if !incompatibilities.is_empty() {
            bail!(
                "existing WXA product is incompatible with incoming grids: {} ({})",
                path.display(),
                incompatibilities.join("; ")
            );
        }
        for record in existing_records
            .into_iter()
            .filter(|record| !incoming_hours.contains(&record.forecast_hour))
        {
            let end = record.offset + record.len;
            if end > existing_bytes.len() {
                bail!("existing WXA chunk exceeds file length: {}", path.display());
            }
            let offset = payload.len();
            payload.extend_from_slice(&existing_bytes[record.offset..end]);
            records.push(WxaDense2dIndexRecord { offset, ..record });
        }
    }

    for grid in grids {
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
    records.sort_by_key(|record| (record.forecast_hour, record.chunk_y, record.chunk_x));

    let mut forecast_hours = records
        .iter()
        .map(|record| record.forecast_hour)
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
        grid: first_grid_meta,
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
    atomic_write_bytes(&path, &output)?;
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
    if let Some(profile) = state.profile.as_deref() {
        if model == profile.manifest.model
            && (run == "latest" || run == profile.manifest.run_id || run == profile.manifest.cycle)
            && parse_pressure_grid_product(variable, &profile.manifest.levels_hpa).is_some()
        {
            return Ok(profile
                .manifest
                .forecast_hours
                .iter()
                .map(|hour| u32::from(*hour))
                .collect());
        }
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
            let Some(value) = tile_value_for_latlon(grid, lat, lon) else {
                continue;
            };
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

fn tile_value_for_latlon(grid: &SpatialGrid, lat: f64, lon: f64) -> Option<f32> {
    if should_smooth_tile_variable(&grid.variable) {
        if let Some(value) = interpolated_geographic_value(grid, lat, lon) {
            return Some(value);
        }
    }
    let index = grid_index_for_latlon(grid, lat, lon)?;
    Some(grid.values.get(index).copied().unwrap_or(f32::NAN))
}

fn should_smooth_tile_variable(variable: &str) -> bool {
    let lower = variable.to_ascii_lowercase();
    !(lower.contains("categorical")
        || lower.contains("precipitation_type")
        || lower.ends_with("_type")
        || lower.contains("cloud_cover_levels"))
}

fn interpolated_geographic_value(grid: &SpatialGrid, lat: f64, lon: f64) -> Option<f32> {
    if !lat.is_finite() || !lon.is_finite() {
        return None;
    }
    match grid.grid_meta.get("type").and_then(Value::as_str)? {
        "regular_latlon" => interpolated_regular_latlon_value(grid, lat, lon),
        "rectilinear_latlon" => interpolated_rectilinear_latlon_value(grid, lat, lon),
        "curvilinear_latlon_sampled" => interpolated_curvilinear_sampled_value(grid, lat, lon),
        _ => None,
    }
}

fn interpolated_regular_latlon_value(grid: &SpatialGrid, lat: f64, lon: f64) -> Option<f32> {
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
    let yf = (lat - lat_start) / lat_step;
    if yf < 0.0 || yf > grid.ny.saturating_sub(1) as f64 {
        return None;
    }
    let mut xf = (unwrap_lon_near(lon, lon_start) - lon_start) / lon_step;
    let lon_wrap = grid
        .grid_meta
        .get("lon_wrap")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        || (lon_step.abs() * grid.nx as f64 - 360.0).abs() <= lon_step.abs().max(0.01) * 2.0;
    if lon_wrap {
        xf = xf.rem_euclid(grid.nx as f64);
    } else if xf < 0.0 || xf > grid.nx.saturating_sub(1) as f64 {
        return None;
    }
    bilinear_grid_value(grid, xf, yf, lon_wrap)
}

fn interpolated_rectilinear_latlon_value(grid: &SpatialGrid, lat: f64, lon: f64) -> Option<f32> {
    let lat_axis = meta_f64_array(&grid.grid_meta, "lat_axis")?;
    let lon_axis = meta_f64_array(&grid.grid_meta, "lon_axis")?;
    if lat_axis.len() != grid.ny || lon_axis.len() != grid.nx {
        return None;
    }
    let y = axis_fraction(&lat_axis, lat, false)?;
    let lon_wrap = grid
        .grid_meta
        .get("lon_wrap")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let x = axis_fraction(&lon_axis, lon, lon_wrap)?;
    bilinear_grid_value(grid, x, y, lon_wrap)
}

fn interpolated_curvilinear_sampled_value(grid: &SpatialGrid, lat: f64, lon: f64) -> Option<f32> {
    let bounds = grid_bounds(grid);
    if lat < bounds[1] - 0.25 || lat > bounds[3] + 0.25 {
        return None;
    }
    if bounds[0] <= bounds[2] && (lon < bounds[0] - 0.25 || lon > bounds[2] + 0.25) {
        return None;
    }

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

    let mut nearest = None::<(usize, f64)>;
    for (index, (&sample_lat, &sample_lon)) in lats.iter().zip(lons.iter()).enumerate() {
        if !sample_lat.is_finite() || !sample_lon.is_finite() {
            continue;
        }
        let dlat = sample_lat - lat;
        let dlon = normalized_lon_delta(sample_lon - lon) * lat.to_radians().cos().abs().max(0.25);
        let dist2 = dlat * dlat + dlon * dlon;
        if nearest.is_none_or(|(_, best_dist)| dist2 < best_dist) {
            nearest = Some((index, dist2));
        }
    }
    let (nearest_index, _) = nearest?;
    let nearest_sx = nearest_index % sample_nx;
    let nearest_sy = nearest_index / sample_nx;

    let sx_start = nearest_sx.saturating_sub(1);
    let sx_end = nearest_sx.min(sample_nx - 2);
    let sy_start = nearest_sy.saturating_sub(1);
    let sy_end = nearest_sy.min(sample_ny - 2);

    let mut best = None::<(f64, f64, f64)>;
    for sy in sy_start..=sy_end {
        for sx in sx_start..=sx_end {
            let Some((tx, ty, err2)) =
                inverse_curvilinear_sample_cell(lat, lon, sx, sy, sample_nx, &lats, &lons)
            else {
                continue;
            };
            if best.is_none_or(|(_, _, best_err2)| err2 < best_err2) {
                best = Some((sx as f64 + tx, sy as f64 + ty, err2));
            }
        }
    }

    let (sample_xf, sample_yf, _) = best?;
    let sample_x0 = sample_xf.floor().clamp(0.0, (sample_nx - 2) as f64) as usize;
    let sample_y0 = sample_yf.floor().clamp(0.0, (sample_ny - 2) as f64) as usize;
    let sample_x1 = sample_x0 + 1;
    let sample_y1 = sample_y0 + 1;
    let tx = (sample_xf - sample_x0 as f64).clamp(0.0, 1.0);
    let ty = (sample_yf - sample_y0 as f64).clamp(0.0, 1.0);
    let gx = bilerp(
        xs[sample_x0],
        xs[sample_x1],
        xs[sample_x0],
        xs[sample_x1],
        tx,
        ty,
    );
    let gy = bilerp(
        ys[sample_y0],
        ys[sample_y0],
        ys[sample_y1],
        ys[sample_y1],
        tx,
        ty,
    );
    bilinear_grid_value(grid, gx, gy, false)
}

fn inverse_curvilinear_sample_cell(
    target_lat: f64,
    target_lon: f64,
    sx: usize,
    sy: usize,
    sample_nx: usize,
    lats: &[f64],
    lons: &[f64],
) -> Option<(f64, f64, f64)> {
    let i00 = sy * sample_nx + sx;
    let i10 = i00 + 1;
    let i01 = i00 + sample_nx;
    let i11 = i01 + 1;
    let lat00 = lats[i00];
    let lat10 = lats[i10];
    let lat01 = lats[i01];
    let lat11 = lats[i11];
    let lon00 = lons[i00];
    let lon10 = lon00 + normalized_lon_delta(lons[i10] - lon00);
    let lon01 = lon00 + normalized_lon_delta(lons[i01] - lon00);
    let lon11 = lon00 + normalized_lon_delta(lons[i11] - lon00);
    let target_lon = unwrap_lon_near(target_lon, lon00);
    if ![
        lat00, lat10, lat01, lat11, lon00, lon10, lon01, lon11, target_lat, target_lon,
    ]
    .iter()
    .all(|value| value.is_finite())
    {
        return None;
    }

    let min_lat = lat00.min(lat10).min(lat01).min(lat11) - 0.5;
    let max_lat = lat00.max(lat10).max(lat01).max(lat11) + 0.5;
    let min_lon = lon00.min(lon10).min(lon01).min(lon11) - 0.5;
    let max_lon = lon00.max(lon10).max(lon01).max(lon11) + 0.5;
    if target_lat < min_lat || target_lat > max_lat || target_lon < min_lon || target_lon > max_lon
    {
        return None;
    }

    let mut tx = if (max_lon - min_lon).abs() > 1.0e-9 {
        ((target_lon - min_lon) / (max_lon - min_lon)).clamp(0.0, 1.0)
    } else {
        0.5
    };
    let mut ty = if (max_lat - min_lat).abs() > 1.0e-9 {
        ((target_lat - min_lat) / (max_lat - min_lat)).clamp(0.0, 1.0)
    } else {
        0.5
    };

    for _ in 0..8 {
        let lat_here = bilerp(lat00, lat10, lat01, lat11, tx, ty);
        let lon_here = bilerp(lon00, lon10, lon01, lon11, tx, ty);
        let f_lat = lat_here - target_lat;
        let f_lon = lon_here - target_lon;
        let dlat_dtx = (lat10 - lat00) * (1.0 - ty) + (lat11 - lat01) * ty;
        let dlat_dty = (lat01 - lat00) * (1.0 - tx) + (lat11 - lat10) * tx;
        let dlon_dtx = (lon10 - lon00) * (1.0 - ty) + (lon11 - lon01) * ty;
        let dlon_dty = (lon01 - lon00) * (1.0 - tx) + (lon11 - lon10) * tx;
        let det = dlat_dtx * dlon_dty - dlon_dtx * dlat_dty;
        if det.abs() < 1.0e-12 {
            break;
        }
        let step_tx = (f_lat * dlon_dty - f_lon * dlat_dty) / det;
        let step_ty = (dlat_dtx * f_lon - dlon_dtx * f_lat) / det;
        tx = (tx - step_tx).clamp(-0.25, 1.25);
        ty = (ty - step_ty).clamp(-0.25, 1.25);
        if step_tx.abs().max(step_ty.abs()) < 1.0e-5 {
            break;
        }
    }

    let lat_here = bilerp(lat00, lat10, lat01, lat11, tx, ty);
    let lon_here = bilerp(lon00, lon10, lon01, lon11, tx, ty);
    let err_lat = lat_here - target_lat;
    let err_lon =
        normalized_lon_delta(lon_here - target_lon) * target_lat.to_radians().cos().abs().max(0.25);
    let err2 = err_lat * err_lat + err_lon * err_lon;
    if tx >= -0.05 && tx <= 1.05 && ty >= -0.05 && ty <= 1.05 && err2 <= 0.25 {
        Some((tx.clamp(0.0, 1.0), ty.clamp(0.0, 1.0), err2))
    } else {
        None
    }
}

fn axis_fraction(axis: &[f64], value: f64, wraps: bool) -> Option<f64> {
    if axis.is_empty() || !value.is_finite() {
        return None;
    }
    let value = if wraps {
        unwrap_lon_near(value, axis[0])
    } else {
        value
    };
    if axis.len() == 1 {
        return ((value - axis[0]).abs() <= f64::EPSILON).then_some(0.0);
    }
    let ascending = axis[axis.len() - 1] >= axis[0];
    let first = axis[0];
    let last = axis[axis.len() - 1];
    let in_range = if ascending {
        value >= first && value <= last
    } else {
        value <= first && value >= last
    };
    if !in_range {
        return None;
    }
    for index in 0..axis.len() - 1 {
        let a = axis[index];
        let b = axis[index + 1];
        if a == b {
            continue;
        }
        let between = if ascending {
            value >= a && value <= b
        } else {
            value <= a && value >= b
        };
        if between {
            return Some(index as f64 + ((value - a) / (b - a)).clamp(0.0, 1.0));
        }
    }
    Some((axis.len() - 1) as f64)
}

fn bilinear_grid_value(grid: &SpatialGrid, xf: f64, yf: f64, wraps_x: bool) -> Option<f32> {
    if !xf.is_finite() || !yf.is_finite() || yf < 0.0 || yf > grid.ny.saturating_sub(1) as f64 {
        return None;
    }
    let x0f = xf.floor();
    let y0f = yf.floor();
    let tx = (xf - x0f).clamp(0.0, 1.0) as f32;
    let ty = (yf - y0f).clamp(0.0, 1.0) as f32;
    let x0 = x0f as isize;
    let y0 = y0f as isize;
    let x1 = if wraps_x {
        (x0 + 1).rem_euclid(grid.nx as isize)
    } else {
        (x0 + 1).min(grid.nx.saturating_sub(1) as isize)
    };
    let y1 = (y0 + 1).min(grid.ny.saturating_sub(1) as isize);
    if x0 < 0 || y0 < 0 || x0 >= grid.nx as isize || y0 >= grid.ny as isize {
        return None;
    }
    let x0 = x0 as usize;
    let y0 = y0 as usize;
    let x1 = x1 as usize;
    let y1 = y1 as usize;
    let v00 = grid.values[y0 * grid.nx + x0];
    let v10 = grid.values[y0 * grid.nx + x1];
    let v01 = grid.values[y1 * grid.nx + x0];
    let v11 = grid.values[y1 * grid.nx + x1];
    if v00.is_finite() && v10.is_finite() && v01.is_finite() && v11.is_finite() {
        let top = v00 * (1.0 - tx) + v10 * tx;
        let bottom = v01 * (1.0 - tx) + v11 * tx;
        return Some(top * (1.0 - ty) + bottom * ty);
    }
    let nearest_x = if tx < 0.5 { x0 } else { x1 };
    let nearest_y = if ty < 0.5 { y0 } else { y1 };
    Some(grid.values[nearest_y * grid.nx + nearest_x])
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

fn cached_f32_file(
    cache: &mut HashMap<PathBuf, Arc<Vec<f32>>>,
    path: &Path,
) -> Result<Arc<Vec<f32>>> {
    if let Some(values) = cache.get(path) {
        return Ok(Arc::clone(values));
    }
    let values = Arc::new(read_f32_file(path)?);
    cache.insert(path.to_path_buf(), Arc::clone(&values));
    Ok(values)
}

fn grid_meta_from_latlon(
    model: &str,
    nx: usize,
    ny: usize,
    lat: &[f32],
    lon: &[f32],
    record: &RustwxGridExportRecord,
) -> Value {
    if matches!(model, "hrrr" | "hrrr_archive")
        && !lat.is_empty()
        && lat.len() == nx * ny
        && lon.len() == nx * ny
    {
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

fn read_wxa_dense2d_metadata(path: &Path) -> Result<(WxaDense2dMeta, Vec<WxaDense2dIndexRecord>)> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let file_len = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len() as usize;
    let mut header_bytes = vec![0u8; WXA_DENSE2D_HEADER_LEN];
    file.read_exact(&mut header_bytes)
        .with_context(|| format!("read WXA header {}", path.display()))?;
    let header = parse_wxa_dense2d_header(&header_bytes)?;
    let meta_end = WXA_DENSE2D_HEADER_LEN + header.metadata_len;
    if meta_end > file_len {
        bail!("WXA metadata exceeds file length");
    }
    let mut meta_bytes = vec![0u8; header.metadata_len];
    file.read_exact(&mut meta_bytes)
        .with_context(|| format!("read WXA metadata {}", path.display()))?;
    let meta: WxaDense2dMeta = serde_json::from_slice(&meta_bytes)
        .with_context(|| format!("parse WXA metadata {}", path.display()))?;
    let index_end = header.index_offset + header.index_count * WXA_DENSE2D_INDEX_RECORD_LEN;
    if index_end > file_len || header.payload_offset > file_len {
        bail!("WXA index exceeds file length");
    }
    file.seek(SeekFrom::Start(header.index_offset as u64))
        .with_context(|| format!("seek WXA index {}", path.display()))?;
    let mut index_bytes = vec![0u8; header.index_count * WXA_DENSE2D_INDEX_RECORD_LEN];
    file.read_exact(&mut index_bytes)
        .with_context(|| format!("read WXA index {}", path.display()))?;
    let mut records = Vec::with_capacity(header.index_count);
    let mut offset = 0usize;
    for _ in 0..header.index_count {
        records.push(WxaDense2dIndexRecord {
            forecast_hour: u32_from(&index_bytes[offset..offset + 4])?,
            chunk_y: u32_from(&index_bytes[offset + 4..offset + 8])? as usize,
            chunk_x: u32_from(&index_bytes[offset + 8..offset + 12])? as usize,
            y_count: u32_from(&index_bytes[offset + 12..offset + 16])? as usize,
            x_count: u32_from(&index_bytes[offset + 16..offset + 20])? as usize,
            raw_len: u32_from(&index_bytes[offset + 20..offset + 24])? as usize,
            offset: u64_from(&index_bytes[offset + 24..offset + 32])? as usize,
            len: u64_from(&index_bytes[offset + 32..offset + 40])? as usize,
            min: f32_from(&index_bytes[offset + 40..offset + 44])?,
            max: f32_from(&index_bytes[offset + 44..offset + 48])?,
            valid_count: u32_from(&index_bytes[offset + 48..offset + 52])?,
        });
        offset += WXA_DENSE2D_INDEX_RECORD_LEN;
    }
    Ok((meta, records))
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

fn storage_variable_alias(variable: &str) -> Option<&'static str> {
    match variable {
        "temperature_2m" => Some("2m_temperature"),
        "dew_point_2m" => Some("2m_dewpoint"),
        "relative_humidity_2m" => Some("2m_relative_humidity"),
        "wind_gusts_10m" => Some("10m_wind_gusts"),
        "precipitation" => Some("total_qpf"),
        "cloud_cover_low" => Some("low_cloud_cover"),
        "cloud_cover_mid" => Some("middle_cloud_cover"),
        "cloud_cover_high" => Some("high_cloud_cover"),
        "pressure_msl" => Some("mslp_10m_winds"),
        "cape" => Some("sbcape"),
        "convective_inhibition" => Some("sbcin"),
        _ => None,
    }
}

fn cheap_derived_dependencies(variable: &str) -> Option<&'static [&'static str]> {
    match variable {
        "dewpoint_depression_2m" => Some(&["temperature_2m", "dew_point_2m"]),
        "vpd_2m" | "heat_index_2m" => Some(&["temperature_2m", "relative_humidity_2m"]),
        "wind_chill_2m" => Some(&["temperature_2m", "wind_speed_10m"]),
        "apparent_temperature_2m" => {
            Some(&["temperature_2m", "relative_humidity_2m", "wind_speed_10m"])
        }
        "wind_speed_10m" | "10m_wind_speed" | "wind_direction_10m" => {
            Some(&["u_component_of_wind_10m", "v_component_of_wind_10m"])
        }
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
    if let Some(cycle_time) = parse_run_id_cycle_utc(run) {
        return hours
            .iter()
            .map(|lead| {
                (cycle_time + chrono::Duration::hours(i64::from(*lead)))
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
            })
            .collect();
    }
    hours
        .iter()
        .map(|hour| format!("{run}+f{hour:03}"))
        .collect()
}

fn parse_run_id_cycle_utc(run: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    if let Ok(cycle_time) = chrono::DateTime::parse_from_rfc3339(run) {
        return Some(cycle_time.with_timezone(&chrono::Utc));
    }

    let bytes = run.as_bytes();
    for start in 0..bytes.len().saturating_sub(7) {
        if !bytes[start..start + 8].iter().all(u8::is_ascii_digit) {
            continue;
        }

        let search_end = bytes.len().min(start + 40);
        let Some((hour_start, hour_end)) = (start + 8..search_end).find_map(|candidate| {
            if !bytes[candidate].is_ascii_digit()
                || candidate
                    .checked_sub(1)
                    .is_some_and(|prev| bytes[prev].is_ascii_digit())
            {
                return None;
            }
            let mut end = candidate;
            while end < search_end && bytes[end].is_ascii_digit() && end - candidate < 2 {
                end += 1;
            }
            if end < bytes.len()
                && matches!(bytes[end], b'z' | b'Z')
                && (end + 1 == bytes.len() || !bytes[end + 1].is_ascii_digit())
            {
                Some((candidate, end))
            } else {
                None
            }
        }) else {
            continue;
        };

        let Some(year) = ascii_digits_to_u32(&bytes[start..start + 4]) else {
            continue;
        };
        let Some(month) = ascii_digits_to_u32(&bytes[start + 4..start + 6]) else {
            continue;
        };
        let Some(day) = ascii_digits_to_u32(&bytes[start + 6..start + 8]) else {
            continue;
        };
        let Some(hour) = ascii_digits_to_u32(&bytes[hour_start..hour_end]) else {
            continue;
        };
        if hour > 23 {
            continue;
        }
        let Some(date) = chrono::NaiveDate::from_ymd_opt(year as i32, month, day) else {
            continue;
        };
        let Some(time) = chrono::NaiveTime::from_hms_opt(hour, 0, 0) else {
            continue;
        };
        return Some(chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(
            date.and_time(time),
            chrono::Utc,
        ));
    }
    None
}

fn ascii_digits_to_u32(bytes: &[u8]) -> Option<u32> {
    bytes.iter().try_fold(0u32, |value, byte| {
        byte.is_ascii_digit()
            .then_some(value * 10 + u32::from(*byte - b'0'))
    })
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
    if hours.len() > MAX_QUERY_HOURS {
        bail!(
            "too many forecast hours requested: {} > {}",
            hours.len(),
            MAX_QUERY_HOURS
        );
    }
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
    if hours.len() > MAX_QUERY_HOURS {
        bail!(
            "too many forecast hours requested: {} > {}",
            hours.len(),
            MAX_QUERY_HOURS
        );
    }
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

fn file_cache_token(path: &Path) -> String {
    match fs::metadata(path) {
        Ok(meta) => {
            let modified_ns = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos())
                .unwrap_or_default();
            format!("len={}:mtime_ns={modified_ns}", meta.len())
        }
        Err(_) => "missing".to_string(),
    }
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
        Json(json!({
            "error": true,
            "code": "bad_request",
            "message": reason.as_ref(),
            "reason": reason.as_ref()
        })),
    )
}

fn bad_anyhow(err: anyhow::Error) -> ApiError {
    bad_request(err.to_string())
}

fn not_found(reason: impl AsRef<str>) -> ApiError {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": true,
            "code": "not_found",
            "message": reason.as_ref(),
            "reason": reason.as_ref()
        })),
    )
}

fn service_unavailable(reason: impl AsRef<str>) -> ApiError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "error": true,
            "code": "service_unavailable",
            "message": reason.as_ref(),
            "reason": reason.as_ref()
        })),
    )
}

fn payload_too_large(reason: impl AsRef<str>) -> ApiError {
    (
        StatusCode::PAYLOAD_TOO_LARGE,
        Json(json!({
            "error": true,
            "code": "response_too_large",
            "message": reason.as_ref(),
            "reason": reason.as_ref()
        })),
    )
}

fn internal_error(reason: impl AsRef<str>) -> ApiError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({
            "error": true,
            "code": "internal_error",
            "message": reason.as_ref(),
            "reason": reason.as_ref()
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_test_root(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time before epoch")
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("wxstore_{name}_{}_{}", std::process::id(), nonce));
        fs::create_dir_all(&path).expect("create temp test root");
        path
    }

    fn write_test_f32_le(path: &Path, values: &[f32]) {
        let mut bytes = Vec::with_capacity(values.len() * 4);
        for value in values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        fs::write(path, bytes).expect("write f32 values");
    }

    #[test]
    fn evidence_bundle_lane_lists_and_loads_bundle_files() {
        let root = temp_test_root("evidence_bundle_lane");
        let bundle_dir = root.join("bundles");
        fs::create_dir_all(&bundle_dir).expect("create bundle dir");
        fs::write(
            bundle_dir.join("case-001.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.evidence.bundle.v1",
                "id": "case-001",
                "claim": "Dryline initiation requires a boundary and sounding check",
                "conclusion": "unknown",
                "updated_at": "2026-05-11T00:00:00Z",
                "artifacts": [
                    {"role": "feature_record", "path": "dryline_feature_record.json"},
                    {"role": "sounding", "path": "sample_sounding_diagnostics.json"}
                ]
            }))
            .expect("serialize bundle"),
        )
        .expect("write bundle");

        let lane = EvidenceBundleLane::open(&root).expect("open evidence lane");
        let bundles = lane.bundles_json().expect("list bundles");
        assert_eq!(bundles["bundle_count"], 1);
        assert_eq!(bundles["bundles"][0]["id"], "case-001");
        assert_eq!(bundles["bundles"][0]["artifact_count"], 2);
        assert_eq!(
            bundles["bundles"][0]["url"],
            "/v1/evidence/bundles/case-001"
        );

        let bundle = lane.bundle_json("case-001").expect("load bundle");
        assert_eq!(bundle["bundle_id"], "case-001");
        assert_eq!(
            bundle["claim"],
            "Dryline initiation requires a boundary and sounding check"
        );
        assert!(bundle["wxstore_path"]
            .as_str()
            .is_some_and(|path| path.ends_with("case-001.json")));
        assert!(lane.bundle_json("../case-001").is_err());

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn evidence_only_status_is_ready() {
        let root = temp_test_root("evidence_only_status");
        fs::create_dir_all(root.join("bundles")).expect("create bundle dir");
        let evidence = EvidenceBundleLane::open(&root).expect("open evidence lane");

        let status = store_status(
            None,
            None,
            None,
            None,
            Some(&evidence),
            None,
            None,
            None,
            None,
            None,
            None,
        );
        let ready = readiness_status(
            None,
            None,
            None,
            None,
            Some(&evidence),
            None,
            None,
            None,
            None,
        );

        assert_eq!(status["ok"], true);
        assert_eq!(status["readiness"]["evidence_bundles"], "ready");
        assert_eq!(status["monitoring"]["evidence_bundles"]["bundle_count"], 0);
        assert_eq!(ready["ok"], true);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn observation_lane_serves_sources_and_latest_observations() {
        let root = temp_test_root("observation_lane");
        let source_dir = root.join("sources").join("aviation_weather_metar_conus");
        fs::create_dir_all(&source_dir).expect("create observation source dir");
        fs::write(
            root.join("index.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.index.v1",
                "updated_at": "2026-05-12T05:17:14Z",
                "records": [{
                    "schema": "rustwx-runner.observations.mirror.v1",
                    "id": "aviation_weather_metar_conus",
                    "kind": "asos_awos_metar",
                    "source_name": "Aviation Weather Center METAR Cache CONUS",
                    "fetched_at": "2026-05-12T05:17:13Z",
                    "status": "ok",
                    "raw_record_count": 2,
                    "observation_count": 1,
                    "skipped_count": 1
                }]
            }))
            .expect("serialize observation index"),
        )
        .expect("write observation index");
        fs::write(
            source_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "aviation_weather_metar_conus",
                "observation_count": 1,
                "observations": [{
                    "schema": "rustwx-runner.observation.v1",
                    "station_id": "KOUN",
                    "station_name": "Norman/Univ Oklahoma Arpt",
                    "state": "OK",
                    "timestamp": "2026-05-12T05:15:00Z",
                    "temperature_f": 77.0
                }]
            }))
            .expect("serialize observations"),
        )
        .expect("write latest observations");

        let observations = ObservationLane::open(&root).expect("open observation lane");
        let summary = observations.summary_json();
        assert_eq!(summary["status"], "ready");
        assert_eq!(summary["source_count"], 1);
        assert_eq!(summary["observation_count"], 1);
        let sources = observations
            .sources_json()
            .expect("list observation sources");
        assert_eq!(sources["sources"][0]["id"], "aviation_weather_metar_conus");
        let source = observations
            .source_json("aviation_weather_metar_conus")
            .expect("load observation source");
        assert_eq!(source["observations"][0]["station_id"], "KOUN");
        assert!(observations.source_json("../bad").is_err());

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn mesoanalysis_innovation_lane_queries_station_source_and_watchlists() {
        let root = temp_test_root("mesoanalysis_innovation_lane");
        fs::write(
            root.join("manifest.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx.surface_mesoanalysis.innovation_wxstore_index.v1",
                "generated_at": "2026-05-13T06:49:53Z",
                "history_case_count": 3,
                "station_series_count": 2,
                "source_series_count": 1,
                "query_policy": {
                    "station_keys": ["station_key", "station_id", "source"],
                    "source_keys": ["source"],
                    "variable_key": "variable"
                }
            }))
            .expect("serialize manifest"),
        )
        .expect("write manifest");
        fs::write(
            root.join("station_index.jsonl"),
            [
                serde_json::to_string(&json!({
                    "station_key": "aviation_weather_metar_conus::KP69",
                    "station_id": "KP69",
                    "source": "aviation_weather_metar_conus",
                    "variable": "temperature_c",
                    "case_count": 3,
                    "mean_abs_analysis_error": 5.45,
                    "watchlist": {
                        "severity_score": 7.36,
                        "reason": "persistent_station_bias"
                    }
                }))
                .expect("serialize station record"),
                serde_json::to_string(&json!({
                    "station_key": "aviation_weather_metar_conus::KOUN",
                    "station_id": "KOUN",
                    "source": "aviation_weather_metar_conus",
                    "variable": "temperature_c",
                    "case_count": 3,
                    "mean_abs_analysis_error": 0.25
                }))
                .expect("serialize station record"),
            ]
            .join("\n"),
        )
        .expect("write station index");
        fs::write(
            root.join("source_index.jsonl"),
            serde_json::to_string(&json!({
                "source": "aviation_weather_metar_conus",
                "variable": "wind_speed_ms",
                "case_count": 3,
                "mean_candidate_minus_background_mae": 0.013,
                "watchlist": {
                    "severity_score": 1.31,
                    "reason": "source_mean_worse_than_background"
                }
            }))
            .expect("serialize source record"),
        )
        .expect("write source index");
        fs::write(
            root.join("station_watchlist.json"),
            serde_json::to_vec_pretty(&json!([{
                "station_key": "aviation_weather_metar_conus::KP69",
                "station_id": "KP69",
                "source": "aviation_weather_metar_conus",
                "variable": "temperature_c",
                "case_count": 3,
                "severity_score": 7.36,
                "reason": "persistent_station_bias"
            }]))
            .expect("serialize station watchlist"),
        )
        .expect("write station watchlist");
        fs::write(
            root.join("source_watchlist.json"),
            serde_json::to_vec_pretty(&json!([{
                "source": "aviation_weather_metar_conus",
                "variable": "wind_speed_ms",
                "case_count": 3,
                "severity_score": 1.31,
                "reason": "source_mean_worse_than_background"
            }]))
            .expect("serialize source watchlist"),
        )
        .expect("write source watchlist");

        let lane = MesoanalysisInnovationLane::open(&root).expect("open innovation lane");
        let status = lane.summary_json();
        assert_eq!(status["status"], "ready");
        assert_eq!(status["history_case_count"], 3);

        let station_report = lane
            .query_json(&MesoanalysisInnovationQuery {
                station: Some("KP69".to_string()),
                variable: Some("temperature_c".to_string()),
                ..Default::default()
            })
            .expect("query station");
        assert_eq!(
            station_report["schema"],
            "wxstore.surface_mesoanalysis.innovation_query.v1"
        );
        assert_eq!(station_report["station_match_count"], 1);
        assert_eq!(station_report["source_match_count"], 0);
        assert_eq!(
            station_report["station_records"][0]["station_key"],
            "aviation_weather_metar_conus::KP69"
        );

        let source_report = lane
            .query_json(&MesoanalysisInnovationQuery {
                kind: Some("source".to_string()),
                source: Some("aviation_weather_metar_conus".to_string()),
                variable: Some("wind_speed_ms".to_string()),
                min_case_count: Some(2),
                ..Default::default()
            })
            .expect("query source");
        assert_eq!(source_report["station_match_count"], 0);
        assert_eq!(source_report["source_match_count"], 1);
        assert_eq!(
            source_report["source_records"][0]["watchlist"]["reason"],
            "source_mean_worse_than_background"
        );

        let watchlist = lane
            .watchlist_json(&MesoanalysisInnovationQuery {
                kind: Some("station".to_string()),
                top: Some(1),
                ..Default::default()
            })
            .expect("query watchlist");
        assert_eq!(watchlist["station_match_count"], 1);
        assert_eq!(watchlist["source_match_count"], 0);
        assert_eq!(watchlist["station_items"][0]["station_id"], "KP69");

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn weather_object_index_lists_direct_surface_observations() {
        let root = temp_test_root("weather_objects_observations");
        let source_dir = root.join("sources").join("aviation_weather_metar_conus");
        fs::create_dir_all(&source_dir).expect("create observation source dir");
        fs::write(
            root.join("index.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.index.v1",
                "updated_at": "2026-05-12T05:17:14Z",
                "records": [{
                    "id": "aviation_weather_metar_conus",
                    "kind": "asos_awos_metar",
                    "source_name": "Aviation Weather Center METAR Cache CONUS",
                    "fetched_at": "2026-05-12T05:17:13Z",
                    "status": "ok",
                    "observation_count": 1,
                    "raw_record_count": 2
                }]
            }))
            .expect("serialize observation index"),
        )
        .expect("write observation index");
        fs::write(
            source_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "aviation_weather_metar_conus",
                "observations": [{
                    "station_id": "KOUN",
                    "station_name": "Norman/Univ Oklahoma Arpt",
                    "state": "OK",
                    "latitude": 35.24359,
                    "longitude": -97.47133,
                    "timestamp": "2026-05-12T05:15:00Z",
                    "temperature_f": 77.0
                }]
            }))
            .expect("serialize observations"),
        )
        .expect("write latest observations");
        let observations = ObservationLane::open(&root).expect("open observation lane");

        let index = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("surface_observation".to_string()),
                q: Some("norman".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );

        assert_eq!(index["matched_object_count"], 1);
        assert_eq!(index["objects"][0]["station_id"], "KOUN");
        assert_eq!(index["objects"][0]["latitude"], 35.24359);
        assert_eq!(index["objects"][0]["longitude"], -97.47133);
        assert_eq!(
            index["objects"][0]["source"],
            "aviation_weather_metar_conus"
        );

        let source_index = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("observation_source".to_string()),
                source: Some("aviation_weather_metar_conus".to_string()),
                parameter: Some("temperature".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(source_index["matched_object_count"], 1);
        assert_eq!(source_index["objects"][0]["station_count"], 1);
        assert_eq!(source_index["objects"][0]["quality_tier"], 1);
        let source_parameters = source_index["objects"][0]["parameters"]
            .as_array()
            .expect("source parameters");
        assert!(source_parameters
            .iter()
            .any(|value| value.as_str() == Some("temperature")));
        assert!(!source_parameters
            .iter()
            .any(|value| value.as_str() == Some("wind")));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn weather_object_direct_observation_cache_tracks_latest_file_changes() {
        let root = temp_test_root("weather_objects_observation_cache");
        let source_dir = root.join("sources").join("aviation_weather_metar_conus");
        fs::create_dir_all(&source_dir).expect("create observation source dir");
        fs::write(
            root.join("index.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.index.v1",
                "updated_at": "2026-05-12T05:17:14Z",
                "records": [{
                    "id": "aviation_weather_metar_conus",
                    "kind": "asos_awos_metar",
                    "source_name": "Aviation Weather Center METAR Cache CONUS",
                    "fetched_at": "2026-05-12T05:17:13Z",
                    "status": "ok",
                    "observation_count": 1,
                    "raw_record_count": 1
                }]
            }))
            .expect("serialize observation index"),
        )
        .expect("write observation index");
        let latest_path = source_dir.join("latest_observations.json");
        fs::write(
            &latest_path,
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "aviation_weather_metar_conus",
                "observations": [{
                    "station_id": "KOUN",
                    "station_name": "Norman/Univ Oklahoma Arpt",
                    "state": "OK",
                    "latitude": 35.24359,
                    "longitude": -97.47133,
                    "timestamp": "2026-05-12T05:15:00Z",
                    "temperature_f": 77.0
                }]
            }))
            .expect("serialize first observations"),
        )
        .expect("write first observations");
        let observations = ObservationLane::open(&root).expect("open observation lane");

        let initial = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("observation_source".to_string()),
                parameter: Some("wind".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(initial["matched_object_count"], 0);

        fs::write(
            &latest_path,
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "aviation_weather_metar_conus",
                "observations": [{
                    "station_id": "KOUN",
                    "station_name": "Norman/Univ Oklahoma Arpt",
                    "state": "OK",
                    "latitude": 35.24359,
                    "longitude": -97.47133,
                    "timestamp": "2026-05-12T05:20:00Z",
                    "temperature_f": 77.0,
                    "wind_speed_kts": 18.0,
                    "raw_observation": "cache invalidation fixture with extra length"
                }]
            }))
            .expect("serialize updated observations"),
        )
        .expect("write updated observations");

        let updated = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("observation_source".to_string()),
                parameter: Some("wind".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(updated["matched_object_count"], 1);
        assert_eq!(updated["objects"][0]["station_count"], 1);
        assert!(updated["objects"][0]["parameters"]
            .as_array()
            .expect("updated source parameters")
            .iter()
            .any(|value| value.as_str() == Some("wind")));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn weather_object_index_types_hydro_and_marine_observations() {
        let root = temp_test_root("weather_objects_typed_observations");
        let usgs_dir = root.join("sources").join("usgs_nwis_ok");
        let nwps_dir = root.join("sources").join("noaa_nwps_river_forecasts");
        let flash_dir = root.join("sources").join("maricopa_fcd_alert");
        let ndbc_dir = root.join("sources").join("ndbc_marine");
        let coops_met_dir = root.join("sources").join("noaa_coops_meteorology");
        let coops_dir = root.join("sources").join("noaa_coops_water_level");
        let aq_dir = root.join("sources").join("epa_airnow_monitors");
        fs::create_dir_all(&usgs_dir).expect("create USGS source dir");
        fs::create_dir_all(&nwps_dir).expect("create NWPS source dir");
        fs::create_dir_all(&flash_dir).expect("create Maricopa FCD source dir");
        fs::create_dir_all(&ndbc_dir).expect("create NDBC source dir");
        fs::create_dir_all(&coops_met_dir).expect("create CO-OPS met source dir");
        fs::create_dir_all(&coops_dir).expect("create CO-OPS source dir");
        fs::create_dir_all(&aq_dir).expect("create AQ source dir");
        fs::write(
            root.join("index.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.index.v1",
                "updated_at": "2026-05-12T12:06:48Z",
                "records": [
                    {
                        "id": "usgs_nwis_ok",
                        "kind": "hydro_current_observation",
                        "source_name": "USGS NWIS Instantaneous Values OK",
                        "fetched_at": "2026-05-12T12:06:48Z",
                        "status": "ok",
                        "observation_count": 1,
                        "raw_record_count": 3
                    },
                    {
                        "id": "noaa_nwps_river_forecasts",
                        "kind": "hydro_forecast_status",
                        "source_name": "NOAA NWPS River Forecast Status",
                        "fetched_at": "2026-05-12T14:38:00Z",
                        "status": "ok",
                        "observation_count": 1,
                        "raw_record_count": 1
                    },
                    {
                        "id": "maricopa_fcd_alert",
                        "kind": "flash_flood_current_observation",
                        "source_name": "Maricopa County Flood Control ALERT Rain and Stream Gauges",
                        "fetched_at": "2026-05-12T14:45:00Z",
                        "status": "ok",
                        "observation_count": 1,
                        "raw_record_count": 2
                    },
                    {
                        "id": "ndbc_marine",
                        "kind": "marine_current_observation",
                        "source_name": "NOAA NDBC Latest Marine Observations",
                        "fetched_at": "2026-05-12T11:46:39Z",
                        "status": "ok",
                        "observation_count": 1,
                        "raw_record_count": 1
                    },
                    {
                        "id": "noaa_coops_meteorology",
                        "kind": "coastal_meteorology_current",
                        "source_name": "NOAA CO-OPS Latest Meteorology",
                        "fetched_at": "2026-05-12T15:30:00Z",
                        "status": "ok",
                        "observation_count": 1,
                        "raw_record_count": 3
                    },
                    {
                        "id": "noaa_coops_water_level",
                        "kind": "coastal_water_current",
                        "source_name": "NOAA CO-OPS Latest Water Levels",
                        "fetched_at": "2026-05-12T12:12:00Z",
                        "status": "ok",
                        "observation_count": 1,
                        "raw_record_count": 1
                    },
                    {
                        "id": "epa_airnow_monitors",
                        "kind": "air_quality_current_observation",
                        "source_name": "EPA AirNow Current Monitor Data",
                        "fetched_at": "2026-05-12T12:00:00Z",
                        "status": "ok",
                        "observation_count": 1,
                        "raw_record_count": 1
                    }
                ]
            }))
            .expect("serialize observation index"),
        )
        .expect("write observation index");
        fs::write(
            usgs_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "usgs_nwis_ok",
                "observations": [{
                    "station_id": "USGS:07148400",
                    "station_name": "Salt Fork Arkansas River nr Alva, OK",
                    "state": "OK",
                    "latitude": 36.815,
                    "longitude": -98.648,
                    "timestamp": "2026-05-12T11:30:00Z",
                    "streamflow_cfs": 44.6
                }]
            }))
            .expect("serialize USGS observations"),
        )
        .expect("write USGS observations");
        fs::write(
            ndbc_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "ndbc_marine",
                "observations": [{
                    "station_id": "NDBC:46029",
                    "station_name": "Columbia River Bar",
                    "state": "MARINE",
                    "latitude": 46.148,
                    "longitude": -124.508,
                    "timestamp": "2026-05-12T11:10:00Z",
                    "water_temperature_f": 55.9
                }]
            }))
            .expect("serialize NDBC observations"),
        )
        .expect("write NDBC observations");
        fs::write(
            nwps_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "noaa_nwps_river_forecasts",
                "observations": [{
                    "station_id": "NWPS:ABBG1",
                    "station_name": "Ocmulgee River at Abbeville",
                    "state": "GA",
                    "latitude": 31.9967,
                    "longitude": -83.2792,
                    "timestamp": "2026-05-12T18:00:00Z",
                    "gage_height_ft": 5.5,
                    "streamflow_cfs": 3460.0,
                    "weather_conditions": "no_flooding"
                }]
            }))
            .expect("serialize NWPS observations"),
        )
        .expect("write NWPS observations");
        fs::write(
            flash_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "maricopa_fcd_alert",
                "observations": [{
                    "station_id": "FCDMC:RAIN:1200",
                    "station_name": "Humboldt Mountain",
                    "state": "AZ",
                    "latitude": 33.98075,
                    "longitude": -111.79794,
                    "timestamp": "2026-05-12T06:00:00Z",
                    "precipitation_1hr_in": 0.16,
                    "precipitation_accum_in": 5.28,
                    "weather_conditions": "flash_flood_rain_signal"
                }]
            }))
            .expect("serialize Maricopa FCD observations"),
        )
        .expect("write Maricopa FCD observations");
        fs::write(
            coops_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "noaa_coops_water_level",
                "observations": [{
                    "station_id": "COOPS:9414290",
                    "station_name": "San Francisco",
                    "state": "CA",
                    "latitude": 37.8063,
                    "longitude": -122.4659,
                    "timestamp": "2026-05-12T12:12:00Z",
                    "water_level_ft": 2.882
                }]
            }))
            .expect("serialize CO-OPS observations"),
        )
        .expect("write CO-OPS observations");
        fs::write(
            coops_met_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "noaa_coops_meteorology",
                "observations": [{
                    "station_id": "COOPS_MET:9414290",
                    "station_name": "San Francisco",
                    "state": "CA",
                    "latitude": 37.8063,
                    "longitude": -122.4659,
                    "timestamp": "2026-05-12T15:30:00Z",
                    "temperature_f": 52.0,
                    "wind_speed_kts": 7.8,
                    "wind_direction_deg": 248.0,
                    "wind_gust_kts": 11.1,
                    "station_pressure_mb": 1014.9
                }]
            }))
            .expect("serialize CO-OPS met observations"),
        )
        .expect("write CO-OPS met observations");
        fs::write(
            aq_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "epa_airnow_monitors",
                "observations": [{
                    "station_id": "AIRNOW:010730023",
                    "station_name": "NO. BHAM",
                    "state": "AL",
                    "latitude": 33.5531,
                    "longitude": -86.815,
                    "timestamp": "2026-05-12T12:00:00Z",
                    "air_quality_index": 35.0,
                    "pm25_ugm3": 6.8,
                    "pm25_aqi": 32.0,
                    "ozone_ppb": 34.0,
                    "ozone_aqi": 35.0
                }]
            }))
            .expect("serialize AQ observations"),
        )
        .expect("write AQ observations");
        let observations = ObservationLane::open(&root).expect("open observation lane");

        let hydro = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("hydro_observation".to_string()),
                q: Some("07148400".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(hydro["matched_object_count"], 1);
        assert_eq!(
            hydro["objects"][0]["id"],
            "hydro_observation:usgs_nwis_ok:USGS:07148400"
        );
        assert_eq!(hydro["objects"][0]["category"], "water");
        assert_eq!(hydro["objects"][0]["quality_tier"], 1);

        let hydro_forecast = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("hydro_forecast_observation".to_string()),
                q: Some("NWPS:ABBG1".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(hydro_forecast["matched_object_count"], 1);
        assert_eq!(
            hydro_forecast["objects"][0]["id"],
            "hydro_forecast_observation:noaa_nwps_river_forecasts:NWPS:ABBG1"
        );
        assert_eq!(hydro_forecast["objects"][0]["category"], "water");

        let flash_flood = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("flash_flood_observation".to_string()),
                q: Some("FCDMC:RAIN:1200".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(flash_flood["matched_object_count"], 1);
        assert_eq!(
            flash_flood["objects"][0]["id"],
            "flash_flood_observation:maricopa_fcd_alert:FCDMC:RAIN:1200"
        );
        assert_eq!(flash_flood["objects"][0]["category"], "water");
        assert_eq!(flash_flood["objects"][0]["quality_tier"], 3);

        let marine = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("marine_observation".to_string()),
                q: Some("46029".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(marine["matched_object_count"], 1);
        assert_eq!(marine["objects"][0]["category"], "ocean");

        let coastal = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("coastal_water_observation".to_string()),
                q: Some("9414290".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(coastal["matched_object_count"], 1);
        assert_eq!(
            coastal["objects"][0]["id"],
            "coastal_water_observation:noaa_coops_water_level:COOPS:9414290"
        );
        assert_eq!(coastal["objects"][0]["category"], "water");

        let coastal_met = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("coastal_meteorology_observation".to_string()),
                q: Some("COOPS_MET:9414290".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(coastal_met["matched_object_count"], 1);
        assert_eq!(
            coastal_met["objects"][0]["id"],
            "coastal_meteorology_observation:noaa_coops_meteorology:COOPS_MET:9414290"
        );
        assert_eq!(coastal_met["objects"][0]["category"], "ocean");
        assert_eq!(coastal_met["objects"][0]["quality_tier"], 1);

        let aq = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("air_quality_observation".to_string()),
                q: Some("010730023".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(aq["matched_object_count"], 1);
        assert_eq!(
            aq["objects"][0]["id"],
            "air_quality_observation:epa_airnow_monitors:AIRNOW:010730023"
        );
        assert_eq!(aq["objects"][0]["category"], "air_quality");

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn weather_object_index_filters_direct_surface_observations_spatially() {
        let root = temp_test_root("weather_objects_spatial_observations");
        let source_dir = root.join("sources").join("aviation_weather_metar_conus");
        fs::create_dir_all(&source_dir).expect("create observation source dir");
        fs::write(
            root.join("index.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.index.v1",
                "updated_at": "2026-05-12T05:17:14Z",
                "records": [{
                    "id": "aviation_weather_metar_conus",
                    "kind": "asos_awos_metar",
                    "source_name": "Aviation Weather Center METAR Cache CONUS",
                    "fetched_at": "2026-05-12T05:17:13Z",
                    "status": "ok",
                    "observation_count": 2,
                    "raw_record_count": 2
                }]
            }))
            .expect("serialize observation index"),
        )
        .expect("write observation index");
        fs::write(
            source_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "aviation_weather_metar_conus",
                "observations": [
                    {
                        "station_id": "KOUN",
                        "station_name": "Norman/Univ Oklahoma Arpt",
                        "state": "OK",
                        "latitude": 35.24359,
                        "longitude": -97.47133,
                        "timestamp": "2026-05-12T05:15:00Z"
                    },
                    {
                        "station_id": "KLAX",
                        "station_name": "Los Angeles Intl",
                        "state": "CA",
                        "latitude": "33.9382",
                        "longitude": "-118.3866",
                        "timestamp": "2026-05-12T05:15:00Z"
                    }
                ]
            }))
            .expect("serialize observations"),
        )
        .expect("write latest observations");
        let observations = ObservationLane::open(&root).expect("open observation lane");

        let bbox = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("surface_observation".to_string()),
                bbox: Some("-98,35,-97,36".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(bbox["matched_object_count"], 1);
        assert_eq!(bbox["objects"][0]["station_id"], "KOUN");

        let radius = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("surface_observation".to_string()),
                lat: Some(35.24),
                lon: Some(-97.47),
                radius_km: Some(25.0),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(radius["matched_object_count"], 1);
        assert_eq!(radius["objects"][0]["station_id"], "KOUN");

        let lax_radius = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("surface_observation".to_string()),
                lat: Some(33.94),
                lon: Some(-118.39),
                radius_km: Some(10.0),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(lax_radius["matched_object_count"], 1);
        assert_eq!(lax_radius["objects"][0]["station_id"], "KLAX");

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn weather_object_index_filters_direct_observation_facets() {
        let root = temp_test_root("weather_objects_observation_facets");
        let metar_dir = root.join("sources").join("aviation_weather_metar_conus");
        let ndbc_dir = root.join("sources").join("ndbc_marine");
        fs::create_dir_all(&metar_dir).expect("create METAR source dir");
        fs::create_dir_all(&ndbc_dir).expect("create NDBC source dir");
        fs::write(
            root.join("index.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.index.v1",
                "updated_at": "2026-05-12T05:17:14Z",
                "records": [
                    {
                        "id": "aviation_weather_metar_conus",
                        "kind": "asos_awos_metar",
                        "source_name": "Aviation Weather Center METAR Cache CONUS",
                        "fetched_at": "2026-05-12T05:17:13Z",
                        "status": "ok",
                        "observation_count": 2,
                        "raw_record_count": 2
                    },
                    {
                        "id": "ndbc_marine",
                        "kind": "marine_current_observation",
                        "source_name": "NOAA NDBC Latest Marine Observations",
                        "fetched_at": "2026-05-12T11:46:39Z",
                        "status": "ok",
                        "observation_count": 1,
                        "raw_record_count": 1
                    }
                ]
            }))
            .expect("serialize observation index"),
        )
        .expect("write observation index");

        let fresh_time = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let old_time = (chrono::Utc::now() - chrono::Duration::hours(4))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        fs::write(
            metar_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "aviation_weather_metar_conus",
                "observations": [
                    {
                        "station_id": "KOUN",
                        "station_name": "Norman/Univ Oklahoma Arpt",
                        "state": "OK",
                        "latitude": 35.24359,
                        "longitude": -97.47133,
                        "network": "ASOS_AWOS",
                        "timestamp": fresh_time,
                        "temperature_f": 77.0,
                        "dewpoint_f": 68.0,
                        "wind_speed_kts": 18.0,
                        "wind_gust_kts": 26.0
                    },
                    {
                        "station_id": "KOLD",
                        "station_name": "Old Observation",
                        "state": "OK",
                        "latitude": 35.0,
                        "longitude": -97.0,
                        "network": "ASOS_AWOS",
                        "timestamp": old_time,
                        "temperature_f": 76.0,
                        "wind_speed_kts": 12.0
                    }
                ]
            }))
            .expect("serialize METAR observations"),
        )
        .expect("write METAR observations");
        fs::write(
            ndbc_dir.join("latest_observations.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.observations.normalized.v1",
                "source": "ndbc_marine",
                "observations": [{
                    "station_id": "NDBC:46029",
                    "station_name": "Columbia River Bar",
                    "state": "MARINE",
                    "latitude": 46.148,
                    "longitude": -124.508,
                    "network": "NDBC",
                    "timestamp": fresh_time,
                    "wave_height_ft": 7.2,
                    "water_temperature_f": 55.9
                }]
            }))
            .expect("serialize NDBC observations"),
        )
        .expect("write NDBC observations");
        let observations = ObservationLane::open(&root).expect("open observation lane");

        let fresh_wind = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("surface_observation".to_string()),
                category: Some("weather".to_string()),
                network: Some("ASOS_AWOS".to_string()),
                parameter: Some("wind".to_string()),
                quality_tier: Some(1),
                max_age_minutes: Some(60.0),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(fresh_wind["matched_object_count"], 1);
        assert_eq!(fresh_wind["objects"][0]["station_id"], "KOUN");
        assert_eq!(fresh_wind["objects"][0]["network"], "ASOS_AWOS");
        assert_eq!(fresh_wind["objects"][0]["quality_tier"], 1);
        assert_eq!(fresh_wind["query"]["parameter"], "wind");
        assert_eq!(fresh_wind["query"]["quality_tier"], 1);
        assert_eq!(fresh_wind["query"]["max_age_minutes"], 60.0);
        let fresh_wind_parameters = fresh_wind["objects"][0]["parameters"]
            .as_array()
            .expect("fresh wind parameters");
        assert!(fresh_wind_parameters
            .iter()
            .any(|value| value.as_str() == Some("temperature")));
        assert!(fresh_wind_parameters
            .iter()
            .any(|value| value.as_str() == Some("dewpoint")));
        assert!(fresh_wind_parameters
            .iter()
            .any(|value| value.as_str() == Some("wind")));
        assert!(!fresh_wind_parameters
            .iter()
            .any(|value| value.as_str() == Some("wave_height")));

        let direct_field = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("surface_observation".to_string()),
                parameter: Some("wind_speed_kts".to_string()),
                max_age_minutes: Some(60.0),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(direct_field["matched_object_count"], 1);
        assert_eq!(direct_field["objects"][0]["station_id"], "KOUN");

        let wave = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("marine_observation".to_string()),
                category: Some("ocean".to_string()),
                parameter: Some("wave_height".to_string()),
                quality_tier: Some(1),
                network: Some("ndbc".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(wave["matched_object_count"], 1);
        assert_eq!(
            wave["objects"][0]["id"],
            "marine_observation:ndbc_marine:NDBC:46029"
        );

        let ocean_temperature = weather_objects_index(
            &WeatherObjectQuery {
                category: Some("ocean".to_string()),
                parameter: Some("temperature".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(ocean_temperature["matched_object_count"], 0);

        let water_temperature = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("marine_observation".to_string()),
                category: Some("ocean".to_string()),
                parameter: Some("water_temperature".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(water_temperature["matched_object_count"], 1);
        assert_eq!(
            water_temperature["objects"][0]["id"],
            "marine_observation:ndbc_marine:NDBC:46029"
        );
        let water_temperature_parameters = water_temperature["objects"][0]["parameters"]
            .as_array()
            .expect("water temperature parameters");
        assert!(water_temperature_parameters
            .iter()
            .any(|value| value.as_str() == Some("water_temperature")));
        assert!(water_temperature_parameters
            .iter()
            .any(|value| value.as_str() == Some("wave_height")));
        assert!(!water_temperature_parameters
            .iter()
            .any(|value| value.as_str() == Some("temperature")));

        let impossible = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("surface_observation".to_string()),
                parameter: Some("wave_height".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            Some(&observations),
            None,
            None,
        );
        assert_eq!(impossible["matched_object_count"], 0);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn weather_object_index_lists_evidence_and_static_plot_artifacts() {
        let evidence_root = temp_test_root("weather_objects_evidence");
        let bundle_dir = evidence_root.join("bundles");
        fs::create_dir_all(&bundle_dir).expect("create bundle dir");
        fs::write(
            bundle_dir.join("case-001.json"),
            serde_json::to_vec_pretty(&json!({
                "schema": "rustwx-runner.evidence.bundle.v1",
                "id": "case-001",
                "claim": "Dryline initiation evidence packet",
                "conclusion": "mixed",
                "source": "unit-test",
                "updated_at": "2026-05-11T00:00:00Z",
                "artifacts": []
            }))
            .expect("serialize bundle"),
        )
        .expect("write bundle");
        let evidence = EvidenceBundleLane::open(&evidence_root).expect("open evidence lane");

        let static_root = temp_test_root("weather_objects_static");
        let output_root = static_root.join("plots");
        fs::create_dir_all(&output_root).expect("create plot output root");
        fs::write(output_root.join("theta_e.png"), b"png").expect("write artifact");
        fs::write(
            static_root.join("hrrr_run_manifest.json"),
            serde_json::to_vec_pretty(&json!({
                "run_kind": "hrrr_non_ecape_hour",
                "run_label": "hrrr_20260511_00z_f001_conus",
                "output_root": output_root,
                "model": "hrrr",
                "date_yyyymmdd": "20260511",
                "cycle_utc": 0,
                "forecast_hour": 1,
                "source": "nomads",
                "domain_slug": "conus",
                "state": "complete",
                "artifacts": [{
                    "artifact_key": "theta_e",
                    "relative_path": "theta_e.png",
                    "state": "complete",
                    "input_fetch_keys": ["hrrr.t00z.wrfsfcf01"]
                }]
            }))
            .expect("serialize static manifest"),
        )
        .expect("write static manifest");
        let static_plots = StaticPlotLane::open(&static_root).expect("open static lane");

        let index = weather_objects_index(
            &WeatherObjectQuery {
                limit: Some(10),
                ..WeatherObjectQuery::default()
            },
            None,
            Some(&static_plots),
            Some(&evidence),
            None,
            None,
            None,
        );
        let objects = index["objects"].as_array().expect("objects array");
        assert_eq!(index["matched_object_count"], 2);
        assert!(objects.iter().any(|object| {
            object["kind"] == "evidence_bundle"
                && object["id"] == "evidence_bundle:case-001"
                && object["source"] == "unit-test"
        }));
        assert!(objects.iter().any(|object| {
            object["kind"] == "static_plot_artifact"
                && object["model"] == "hrrr"
                && object["product"] == "theta_e"
                && object["exists"] == true
        }));

        let hrrr_static = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("static_plot_artifact".to_string()),
                model: Some("hrrr".to_string()),
                q: Some("theta".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            Some(&static_plots),
            Some(&evidence),
            None,
            None,
            None,
        );
        assert_eq!(hrrr_static["matched_object_count"], 1);
        assert_eq!(hrrr_static["objects"][0]["product"], "theta_e");

        fs::remove_dir_all(evidence_root).ok();
        fs::remove_dir_all(static_root).ok();
    }

    #[test]
    fn weather_object_index_filters_model_grid_hour_metadata() {
        let root = temp_test_root("weather_objects_spatial_grid_facets");
        let data_dir = root
            .join("hrrr")
            .join("20260430_23z")
            .join("members")
            .join("001")
            .join("temperature_2m.zarr")
            .join("data");
        fs::create_dir_all(&data_dir).expect("create spatial zarr fixture");
        fs::write(data_dir.join(".zarray"), br#"{"zarr_format":2}"#).expect("write zarray");
        fs::write(data_dir.join("0.0.0"), b"").expect("write f000 chunk marker");
        fs::write(data_dir.join("1.0.0"), b"").expect("write f001 chunk marker");
        let spatial = SpatialLane::open(&root).expect("open spatial lane");

        let index = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("model_grid_field".to_string()),
                product: Some("temperature_2m".to_string()),
                member: Some("001".to_string()),
                forecast_hour: Some(1),
                valid_time: Some("2026-05-01T00:00:00Z".to_string()),
                ..WeatherObjectQuery::default()
            },
            Some(&spatial),
            None,
            None,
            None,
            None,
            None,
        );

        assert_eq!(index["matched_object_count"], 1);
        assert_eq!(index["objects"][0]["product"], "temperature_2m");
        assert_eq!(index["objects"][0]["member"], "001");
        assert_eq!(index["objects"][0]["forecast_hour"], 1);
        assert_eq!(index["objects"][0]["valid_time"], "2026-05-01T00:00:00Z");

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn weather_object_index_filters_static_artifact_deep_facets() {
        let root = temp_test_root("weather_objects_static_deep_facets");
        let output_root = root.join("plots");
        fs::create_dir_all(&output_root).expect("create plot output root");
        fs::write(output_root.join("theta_e.png"), b"png").expect("write artifact");
        fs::write(
            root.join("hrrr_run_manifest.json"),
            serde_json::to_vec_pretty(&json!({
                "run_kind": "hrrr_non_ecape_hour",
                "run_label": "hrrr_20260511_00z_f003_conus",
                "output_root": output_root,
                "model": "hrrr",
                "date_yyyymmdd": "20260511",
                "cycle_utc": 0,
                "forecast_hour": 3,
                "domain_slug": "conus",
                "ensemble_stat": "p90",
                "state": "complete",
                "artifacts": [{
                    "artifact_key": "theta_e",
                    "relative_path": "theta_e.png",
                    "state": "complete",
                    "content_identity": {
                        "threshold": 0.75
                    }
                }]
            }))
            .expect("serialize static manifest"),
        )
        .expect("write static manifest");
        let static_plots = StaticPlotLane::open(&root).expect("open static lane");

        let index = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("static_plot_artifact".to_string()),
                product: Some("theta_e".to_string()),
                forecast_hour: Some(3),
                valid_time: Some("2026-05-11T03:00:00Z".to_string()),
                threshold: Some("0.75".to_string()),
                ensemble_stat: Some("p90".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            Some(&static_plots),
            None,
            None,
            None,
            None,
        );

        assert_eq!(index["matched_object_count"], 1);
        assert_eq!(index["objects"][0]["product"], "theta_e");
        assert_eq!(index["objects"][0]["forecast_hour"], 3);
        assert_eq!(index["objects"][0]["valid_time"], "2026-05-11T03:00:00Z");
        assert_eq!(index["objects"][0]["threshold"], 0.75);
        assert_eq!(index["objects"][0]["ensemble_statistic"], "p90");
        assert_eq!(index["query"]["ensemble_statistic"], "p90");

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn weather_object_index_filters_satellite_frames_and_radar_tilts() {
        let satellite_root = temp_test_root("weather_objects_satellite_frames");
        let satellite_layer_id = "goes_east_conus_ch13";
        let satellite_layer_root = satellite_root.join(satellite_layer_id);
        fs::create_dir_all(&satellite_layer_root).expect("create satellite layer");
        fs::write(
            satellite_layer_root.join("frames.json"),
            serde_json::to_vec_pretty(&json!({
                "frames": [{
                    "id": "20260511T010000Z",
                    "product": "ch13",
                    "source": "goes-east",
                    "scan_time_utc": "2026-05-11T01:00:00Z",
                    "url_template": "goes_east_conus_ch13/frames/20260511T010000Z/{z}/{x}/{y}.png"
                }]
            }))
            .expect("serialize satellite frames"),
        )
        .expect("write satellite frames");
        let satellite_tiles =
            SatelliteTileLane::open(&satellite_root).expect("open satellite lane");

        let radar_root = temp_test_root("weather_objects_radar_tilts");
        let radar_layer_id = "nexrad_level2_ktlx_ref";
        let radar_layer_root = radar_root.join(radar_layer_id);
        fs::create_dir_all(&radar_layer_root).expect("create radar layer");
        fs::write(
            radar_layer_root.join("frames.json"),
            serde_json::to_vec_pretty(&json!({
                "frames": [{
                    "id": "20260511T010000Z",
                    "product": "ref",
                    "source_key_or_url": "s3://noaa-nexrad-level2/2026/05/11/KTLX/KTLX20260511_010000_V06",
                    "scan_time_utc": "2026-05-11T01:00:00Z",
                    "url_template": "nexrad_level2_ktlx_ref/frames/20260511T010000Z/{z}/{x}/{y}.png",
                    "tilts": [{
                        "id": "sweep00_el0p44",
                        "elevation_deg": 0.44,
                        "url_template": "nexrad_level2_ktlx_ref/frames/20260511T010000Z/sweep00_el0p44/{z}/{x}/{y}.png"
                    }]
                }]
            }))
            .expect("serialize radar frames"),
        )
        .expect("write radar frames");
        let radar_tiles = RadarTileLane::open(&radar_root).expect("open radar lane");

        let satellite_index = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("satellite_frame".to_string()),
                product: Some("ch13".to_string()),
                frame: Some("20260511T010000Z".to_string()),
                valid_time: Some("2026-05-11T01:00:00Z".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            None,
            Some(&satellite_tiles),
            None,
        );
        assert_eq!(satellite_index["matched_object_count"], 1);
        assert_eq!(satellite_index["objects"][0]["frame"], "20260511T010000Z");

        let radar_index = weather_objects_index(
            &WeatherObjectQuery {
                kind: Some("radar_tilt".to_string()),
                product: Some("ref".to_string()),
                frame: Some("20260511T010000Z".to_string()),
                tilt: Some("sweep00_el0p44".to_string()),
                valid_time: Some("2026-05-11T01:00:00Z".to_string()),
                ..WeatherObjectQuery::default()
            },
            None,
            None,
            None,
            None,
            None,
            Some(&radar_tiles),
        );
        assert_eq!(radar_index["matched_object_count"], 1);
        assert_eq!(radar_index["objects"][0]["tilt"], "sweep00_el0p44");
        assert_eq!(radar_index["objects"][0]["product"], "ref");

        fs::remove_dir_all(satellite_root).ok();
        fs::remove_dir_all(radar_root).ok();
    }

    #[test]
    fn inspect_args_profile_store_is_optional_for_non_profile_lanes() {
        use clap::CommandFactory;
        InspectArgs::command().debug_assert();
    }

    fn static_plot_test_manifest(run_label: &str) -> StaticPlotRunManifest {
        StaticPlotRunManifest {
            run_kind: "hrrr_non_ecape_hour".to_string(),
            run_label: run_label.to_string(),
            output_root: PathBuf::from("plots"),
            model: None,
            date_yyyymmdd: None,
            cycle_utc: None,
            forecast_hour: None,
            source: None,
            domain_slug: None,
            member: None,
            ensemble_kind: None,
            ensemble_stat: None,
            projection_variant: None,
            plot_variant: None,
            variant: None,
            state: "complete".to_string(),
            detail: None,
            artifacts: Vec::new(),
        }
    }

    #[test]
    fn static_plot_identity_defaults_old_manifests_to_auto_variant() {
        let manifest = static_plot_test_manifest("rustwx_hrrr_20260503_14z_f000_conus");
        let identity = static_plot_identity_with_path(&manifest, None);

        assert_eq!(identity.domain_slug.as_deref(), Some("conus"));
        assert_eq!(static_plot_variant_key(&identity), "auto");
    }

    #[test]
    fn static_plot_identity_reads_and_normalizes_manifest_projection_variant() {
        let mut manifest = static_plot_test_manifest("rustwx_hrrr_20260503_14z_f000_conus");
        manifest.projection_variant = Some("Lambert-Conformal".to_string());
        let identity = static_plot_identity_with_path(&manifest, None);

        assert_eq!(identity.domain_slug.as_deref(), Some("conus"));
        assert_eq!(static_plot_variant_key(&identity), "lambert");
    }

    #[test]
    fn static_plot_identity_infers_variant_from_label_suffix() {
        let manifest = static_plot_test_manifest(
            "rustwx_hrrr_20260503_14z_f000_conus_projection_variant_robinson_non_ecape_hour",
        );
        let identity = static_plot_identity_with_path(&manifest, None);

        assert_eq!(identity.domain_slug.as_deref(), Some("conus"));
        assert_eq!(static_plot_variant_key(&identity), "robinson");
    }

    #[test]
    fn static_plot_identity_infers_variant_from_manifest_path() {
        let manifest = static_plot_test_manifest("rustwx_hrrr_20260503_14z_f000_conus");
        let path = PathBuf::from(
            "plots/hrrr/20260503_14z/projection_mercator/rustwx_hrrr_20260503_14z_f000_conus_run_manifest.json",
        );
        let identity = static_plot_identity_with_path(&manifest, Some(&path));

        assert_eq!(identity.domain_slug.as_deref(), Some("conus"));
        assert_eq!(static_plot_variant_key(&identity), "mercator");
    }

    #[test]
    fn static_plot_record_matches_projection_and_variant_queries() {
        let mut manifest = static_plot_test_manifest("rustwx_hrrr_20260503_14z_f000_conus");
        manifest.plot_variant = Some("geo".to_string());
        let record = StaticPlotManifestRecord {
            id: "test".to_string(),
            path: PathBuf::from("geo/test_run_manifest.json"),
            manifest,
        };

        let projection_query = StaticPlotCatalogQuery {
            projection: Some("geographic".to_string()),
            ..StaticPlotCatalogQuery::default()
        };
        let variant_query = StaticPlotCatalogQuery {
            variant: Some("lambert".to_string()),
            ..StaticPlotCatalogQuery::default()
        };

        assert!(static_plot_record_matches(&record, &projection_query));
        assert!(!static_plot_record_matches(&record, &variant_query));
    }

    #[test]
    fn publish_latest_pointer_does_not_regress_to_older_cycle() {
        let root = temp_test_root("latest_regression");
        let model = "hrrr";
        let newer = "20260503_hrrr_12z";
        let older = "20260503_hrrr_11z";
        fs::create_dir_all(root.join(model).join(newer)).expect("create newer run");
        fs::create_dir_all(root.join(model).join(older)).expect("create older run");

        publish_latest_pointer(&root, model, newer, "test").expect("publish newer");
        let skipped =
            publish_latest_pointer(&root, model, older, "test").expect("older publish skips");

        assert_eq!(
            skipped.get("published").and_then(Value::as_bool),
            Some(false)
        );
        assert_eq!(
            read_latest_pointer_run(&root, model).as_deref(),
            Some(newer)
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn radar_viewer_uses_dedicated_qc_app() {
        assert!(RADAR_HTML.contains("/v1/radar/layers"));
        assert!(RADAR_HTML.contains("/v1/radar/sample"));
        assert!(RADAR_HTML.contains("numeric_sidecar"));
        assert!(RADAR_HTML.contains("Bounds clip"));
        assert!(RADAR_HTML.contains("contextmenu"));
        assert!(!RADAR_HTML.contains("hoverSample"));
        assert!(!RADAR_HTML.contains(r#"sampleAt(event.latlng, "hover")"#));
        assert!(!RADAR_HTML.contains("/v1/satellite"));
    }

    #[test]
    fn radar_frames_json_preserves_native_resolution_metadata() {
        let root = temp_test_root("radar_native_metadata");
        let layer_id = "nexrad_level2_ksjt_ref";
        let frame_id = "20260511T012134Z";
        let layer_root = root.join(layer_id);
        fs::create_dir_all(&layer_root).expect("create radar layer root");
        fs::write(
            layer_root.join("frames.json"),
            serde_json::to_vec_pretty(&json!({
                "ok": true,
                "layer": layer_id,
                "frames": [
                    {
                        "id": frame_id,
                        "layer": layer_id,
                        "site": "KSJT",
                        "product": "ref",
                        "scan_time_utc": "2026-05-11T01:21:34Z",
                        "url_template": format!("{layer_id}/frames/{frame_id}/{{z}}/{{x}}/{{y}}.png"),
                        "bounds": [-101.7, 30.3, -99.1, 32.2],
                        "clip_to_bounds": false,
                        "sampling_bounds": [-105.3, 27.2, -95.6, 35.5],
                        "minzoom": 8,
                        "maxzoom": 9,
                        "tile_count": 27,
                        "native_gate_size_m": 250,
                        "native_azimuth_spacing_deg": 0.4998779296875_f64,
                        "maxzoom_site_meters_per_pixel": 261.0518623255104_f64,
                        "velocity_quality_filter": true,
                        "velocity_quality_qc": {
                            "finite_gate_count": 264580,
                            "masked_gate_count": 13613,
                            "masked_gate_fraction": 0.05145135686748809_f64
                        },
                        "numeric_sidecar": {
                            "schema": "rustwx.radar.polar_sidecar.v2",
                            "manifest_path": "staging/polar_sidecar_manifest.json",
                            "values_path": "staging/polar_values_f32le.bin",
                            "gate_flags_path": "staging/polar_gate_flags_u8.bin",
                            "radial_count": 360,
                            "max_gate_count": 1000,
                            "gate_count": 360000,
                            "processing_state": "raw"
                        },
                        "tilts": [
                            {
                                "id": "sweep00_el0p44",
                                "name": "sweep00_el0p44",
                                "sweep_index": 0,
                                "elevation_deg": 0.43945312,
                                "url_template": format!("{layer_id}/frames/{frame_id}/sweep00_el0p44/{{z}}/{{x}}/{{y}}.png"),
                                "bounds": [-101.7, 30.3, -99.1, 32.2],
                                "clip_to_bounds": false,
                                "sampling_bounds": [-105.3, 27.2, -95.6, 35.5],
                                "minzoom": 8,
                                "maxzoom": 9,
                                "tile_count": 27,
                                "native_gate_size_m": 250,
                                "native_azimuth_spacing_deg": 0.4998779296875_f64,
                                "maxzoom_site_meters_per_pixel": 261.0518623255104_f64,
                                "velocity_quality_filter": true,
                                "velocity_quality_qc": {
                                    "finite_gate_count": 264580,
                                    "masked_gate_count": 13613,
                                    "masked_gate_fraction": 0.05145135686748809_f64
                                },
                                "numeric_sidecar": {
                                    "schema": "rustwx.radar.polar_sidecar.v2",
                                    "manifest_path": "staging/sweep00_el0p44/polar_sidecar_manifest.json",
                                    "values_path": "staging/sweep00_el0p44/polar_values_f32le.bin",
                                    "gate_flags_path": "staging/sweep00_el0p44/polar_gate_flags_u8.bin",
                                    "radial_count": 360,
                                    "max_gate_count": 1000,
                                    "gate_count": 360000,
                                    "processing_state": "raw"
                                }
                            }
                        ]
                    }
                ]
            }))
            .unwrap(),
        )
        .expect("write radar frames index");

        let lane = RadarTileLane::open(&root).expect("open radar lane");
        let value = lane.frames_json(layer_id).expect("read radar frames");
        let frame = &value["frames"][0];
        let tilt = &frame["tilts"][0];

        assert_eq!(frame["native_gate_size_m"].as_u64(), Some(250));
        assert_eq!(
            frame["native_azimuth_spacing_deg"].as_f64(),
            Some(0.4998779296875)
        );
        assert_eq!(
            frame["maxzoom_site_meters_per_pixel"].as_f64(),
            Some(261.0518623255104)
        );
        assert_eq!(frame["clip_to_bounds"].as_bool(), Some(false));
        assert_eq!(frame["sampling_bounds"][0].as_f64(), Some(-105.3));
        assert_eq!(tilt["native_gate_size_m"].as_u64(), Some(250));
        assert_eq!(tilt["clip_to_bounds"].as_bool(), Some(false));
        assert_eq!(tilt["sampling_bounds"][2].as_f64(), Some(-95.6));
        assert_eq!(frame["velocity_quality_filter"].as_bool(), Some(true));
        assert_eq!(
            frame["velocity_quality_qc"]["masked_gate_count"].as_u64(),
            Some(13613)
        );
        assert_eq!(tilt["velocity_quality_filter"].as_bool(), Some(true));
        assert_eq!(
            tilt["velocity_quality_qc"]["masked_gate_fraction"].as_f64(),
            Some(0.05145135686748809)
        );
        assert_eq!(
            frame["tile_url_template"].as_str(),
            Some("/v1/radar/tiles/nexrad_level2_ksjt_ref/frames/20260511T012134Z/{z}/{x}/{y}.png")
        );
        assert_eq!(
            tilt["tile_url_template"].as_str(),
            Some("/v1/radar/tiles/nexrad_level2_ksjt_ref/frames/20260511T012134Z/sweep00_el0p44/{z}/{x}/{y}.png")
        );
        assert_eq!(
            frame["numeric_sidecar_url"].as_str(),
            Some("/v1/radar/sidecars/nexrad_level2_ksjt_ref/frames/20260511T012134Z/polar_sidecar_manifest.json")
        );
        assert_eq!(
            tilt["numeric_sidecar_url"].as_str(),
            Some("/v1/radar/sidecars/nexrad_level2_ksjt_ref/frames/20260511T012134Z/sweep00_el0p44/polar_sidecar_manifest.json")
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn radar_frames_json_preserves_product_provenance() {
        let root = temp_test_root("radar_product_provenance");
        let layer_id = "nexrad_level2_ksjt_kdp";
        let frame_id = "20260511T043240Z";
        let layer_root = root.join(layer_id);
        fs::create_dir_all(&layer_root).expect("create radar layer root");
        let product_provenance = json!({
            "source": "derived",
            "derived": true,
            "inputs": ["phi"],
            "method": "centered_phi_range_derivative"
        });
        fs::write(
            layer_root.join("frames.json"),
            serde_json::to_vec_pretty(&json!({
                "ok": true,
                "layer": layer_id,
                "frames": [{
                    "id": frame_id,
                    "url_template": format!("{layer_id}/frames/{frame_id}/{{z}}/{{x}}/{{y}}.png"),
                    "product_provenance": product_provenance.clone(),
                    "tilts": [{
                        "id": "sweep00_el0p31",
                        "url_template": format!("{layer_id}/frames/{frame_id}/sweep00_el0p31/{{z}}/{{x}}/{{y}}.png"),
                        "product_provenance": product_provenance
                    }]
                }]
            }))
            .unwrap(),
        )
        .expect("write radar frames index");

        let lane = RadarTileLane::open(&root).expect("open radar lane");
        let value = lane.frames_json(layer_id).expect("read radar frames");
        let frame = &value["frames"][0];
        let tilt = &frame["tilts"][0];

        assert_eq!(
            frame["product_provenance"]["method"].as_str(),
            Some("centered_phi_range_derivative")
        );
        assert_eq!(
            tilt["product_provenance"]["inputs"][0].as_str(),
            Some("phi")
        );

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn radar_sample_reads_numeric_sidecar_gate_values() {
        let root = temp_test_root("radar_numeric_sidecar_sample");
        let layer_id = "nexrad_level2_ktlx_ref";
        let frame_id = "20260511T010000Z";
        let tilt_id = "sweep00_el0p00";
        let sidecar_dir = root
            .join(layer_id)
            .join("frames")
            .join(frame_id)
            .join(tilt_id);
        fs::create_dir_all(&sidecar_dir).expect("create sidecar dir");
        fs::write(
            sidecar_dir.join(RADAR_POLAR_SIDECAR_MANIFEST_FILE),
            serde_json::to_vec_pretty(&json!({
                "schema": RADAR_POLAR_SIDECAR_SCHEMA,
                "sidecar_version": 2,
                "ok": true,
                "name": "ktlx_ref_sweep00_el0p00",
                "site": {
                    "id": "KTLX",
                    "name": "Oklahoma City",
                    "state": "OK",
                    "lat": 35.0,
                    "lon": -97.0,
                    "elevation_m": 370.0,
                    "feedhorn_height_m": 20.0,
                    "antenna_elevation_m": 390.0
                },
                "product": "ref",
                "product_name": "Reflectivity",
                "units": "dBZ",
                "product_provenance": {
                    "source": "native",
                    "derived": false
                },
                "source_key_or_url": "s3://noaa-nexrad-level2/2026/05/11/KTLX/KTLX20260511_010000_V06",
                "scan_time_utc": "2026-05-11T01:00:00Z",
                "sweep_index": 0,
                "elevation_deg": 0.0,
                "nyquist_velocity_ms": null,
                "processing_state": "raw",
                "radial_count": 2,
                "max_gate_count": 4,
                "gate_count": 8,
                "values_path": RADAR_POLAR_VALUES_FILE,
                "values_encoding": "f32_le_row_major_radial_gate_nan_missing",
                "gate_flags_path": RADAR_POLAR_GATE_FLAGS_FILE,
                "gate_flags_encoding": "u8_bitmask_row_major_radial_gate",
                "gate_flag_meanings": [
                    {"bit": 0, "mask": RADAR_GATE_FLAG_VALID, "name": "valid", "description": "finite value"},
                    {"bit": 1, "mask": RADAR_GATE_FLAG_MISSING, "name": "missing", "description": "missing value"},
                    {"bit": 2, "mask": RADAR_GATE_FLAG_RANGE_FOLDED, "name": "range_folded", "description": "range folded"},
                    {"bit": 3, "mask": RADAR_GATE_FLAG_FILTERED, "name": "filtered", "description": "filtered by QC"},
                    {"bit": 4, "mask": RADAR_GATE_FLAG_DERIVED, "name": "derived", "description": "derived product"},
                    {"bit": 5, "mask": RADAR_GATE_FLAG_DEALIASED, "name": "dealiased", "description": "dealiased velocity"}
                ],
                "radials": [
                    {
                        "radial_index": 0,
                        "azimuth_deg": 0.0,
                        "elevation_deg": 0.0,
                        "azimuth_spacing_deg": 1.0,
                        "gate_count": 4,
                        "first_gate_range_m": 0,
                        "gate_spacing_m": 250,
                        "nyquist_velocity_ms": null,
                        "data_word_size_bits": 8,
                        "scale": 2.0,
                        "offset": 66.0
                    },
                    {
                        "radial_index": 1,
                        "azimuth_deg": 90.0,
                        "elevation_deg": 0.0,
                        "azimuth_spacing_deg": 1.0,
                        "gate_count": 4,
                        "first_gate_range_m": 0,
                        "gate_spacing_m": 250,
                        "nyquist_velocity_ms": null,
                        "data_word_size_bits": 8,
                        "scale": 2.0,
                        "offset": 66.0
                    }
                ],
                "qc": {
                    "reflectivity_qc": {
                        "despeckle_applied": false
                    }
                }
            }))
            .unwrap(),
        )
        .expect("write sidecar manifest");
        write_test_f32_le(
            &sidecar_dir.join(RADAR_POLAR_VALUES_FILE),
            &[1.0, 2.0, 3.0, 4.0, 10.0, 20.0, 30.0, 40.0],
        );
        fs::write(
            sidecar_dir.join(RADAR_POLAR_GATE_FLAGS_FILE),
            vec![RADAR_GATE_FLAG_VALID; 8],
        )
        .expect("write flags");

        let lane = RadarTileLane::open(&root).expect("open radar lane");
        assert_eq!(lane.cached_sidecar_count(), 0);
        let (query_lat, query_lon) = radar_polar_to_lat_lon(35.0, -97.0, 0.0, 750.0);
        let query = RadarSampleQuery {
            layer: layer_id.to_string(),
            frame: frame_id.to_string(),
            product: Some("ref".to_string()),
            tilt: Some(tilt_id.to_string()),
            lat: query_lat,
            lon: query_lon,
            method: Some("nearest".to_string()),
        };
        let sample = lane.sample_json(&query).expect("sample sidecar");

        assert_eq!(lane.cached_sidecar_count(), 1);
        assert_eq!(sample["value"].as_f64(), Some(4.0));
        assert_eq!(sample["units"].as_str(), Some("dBZ"));
        assert_eq!(sample["product"].as_str(), Some("ref"));
        assert_eq!(sample["sweep_index"].as_u64(), Some(0));
        assert_eq!(sample["radial_index"].as_u64(), Some(0));
        assert_eq!(sample["gate_index"].as_u64(), Some(3));
        assert_eq!(sample["processing_state"].as_str(), Some("raw"));
        assert_eq!(sample["raw"].as_bool(), Some(true));
        assert_eq!(sample["derived"].as_bool(), Some(false));
        assert_eq!(
            sample["provenance"]["source_key_or_url"].as_str(),
            Some("s3://noaa-nexrad-level2/2026/05/11/KTLX/KTLX20260511_010000_V06")
        );
        assert_eq!(
            sample["qc"]["reflectivity_qc"]["despeckle_applied"].as_bool(),
            Some(false)
        );
        assert_eq!(sample["site"]["feedhorn_height_m"].as_f64(), Some(20.0));
        assert_eq!(sample["site"]["antenna_elevation_m"].as_f64(), Some(390.0));
        assert!((sample["range_m"].as_f64().unwrap() - 750.0).abs() < 1e-6);

        let cached_sample = lane.sample_json(&query).expect("sample cached sidecar");
        assert_eq!(cached_sample["value"].as_f64(), Some(4.0));
        assert_eq!(lane.cached_sidecar_count(), 1);

        let updated_values_file = "polar_values_updated_f32le.bin";
        write_test_f32_le(
            &sidecar_dir.join(updated_values_file),
            &[1.0, 2.0, 3.0, 5.5, 10.0, 20.0, 30.0, 40.0],
        );
        let manifest_path = sidecar_dir.join(RADAR_POLAR_SIDECAR_MANIFEST_FILE);
        let mut manifest: Value =
            serde_json::from_slice(&fs::read(&manifest_path).expect("read sidecar manifest"))
                .expect("parse sidecar manifest");
        manifest["values_path"] = json!(updated_values_file);
        manifest["processing_state"] = json!("raw_filtered");
        fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).expect("serialize updated sidecar manifest"),
        )
        .expect("write updated sidecar manifest");

        let refreshed_sample = lane
            .sample_json(&query)
            .expect("sample refreshed sidecar after manifest update");
        assert_eq!(refreshed_sample["value"].as_f64(), Some(5.5));
        assert_eq!(
            refreshed_sample["processing_state"].as_str(),
            Some("raw_filtered")
        );
        assert_eq!(refreshed_sample["filtered"].as_bool(), Some(true));
        assert_eq!(lane.cached_sidecar_count(), 1);

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn radar_sample_labels_categorical_sidecar_values() {
        let root = temp_test_root("radar_categorical_sidecar_sample");
        let layer_id = "nexrad_level2_ktlx_hca";
        let frame_id = "20260511T010000Z";
        let tilt_id = "sweep00_el0p00";
        let sidecar_dir = root
            .join(layer_id)
            .join("frames")
            .join(frame_id)
            .join(tilt_id);
        fs::create_dir_all(&sidecar_dir).expect("create sidecar dir");
        fs::write(
            sidecar_dir.join(RADAR_POLAR_SIDECAR_MANIFEST_FILE),
            serde_json::to_vec_pretty(&json!({
                "schema": RADAR_POLAR_SIDECAR_SCHEMA,
                "sidecar_version": 2,
                "ok": true,
                "name": "ktlx_hca_sweep00_el0p00",
                "site": {
                    "id": "KTLX",
                    "name": "Oklahoma City",
                    "state": "OK",
                    "lat": 35.0,
                    "lon": -97.0,
                    "elevation_m": 370.0
                },
                "product": "hhc",
                "product_name": "Hydrometeor Class (HHC)",
                "units": "category",
                "value_meanings": [
                    {"value": 7.0, "name": "heavy_rain", "label": "Heavy Rain", "description": "high-reflectivity rain"}
                ],
                "product_provenance": {
                    "source": "derived",
                    "derived": true,
                    "inputs": ["ref", "zdr", "cc", "phi"],
                    "method": "dual_pol_rule_hca_v1"
                },
                "source_key_or_url": "s3://noaa-nexrad-level2/2026/05/11/KTLX/KTLX20260511_010000_V06",
                "scan_time_utc": "2026-05-11T01:00:00Z",
                "sweep_index": 0,
                "elevation_deg": 0.0,
                "nyquist_velocity_ms": null,
                "processing_state": "derived",
                "radial_count": 1,
                "max_gate_count": 1,
                "gate_count": 1,
                "values_path": RADAR_POLAR_VALUES_FILE,
                "values_encoding": "f32_le_row_major_radial_gate_nan_missing",
                "gate_flags_path": RADAR_POLAR_GATE_FLAGS_FILE,
                "gate_flags_encoding": "u8_bitmask_row_major_radial_gate",
                "gate_flag_meanings": [
                    {"bit": 0, "mask": RADAR_GATE_FLAG_VALID, "name": "valid", "description": "finite value"},
                    {"bit": 1, "mask": RADAR_GATE_FLAG_MISSING, "name": "missing", "description": "missing value"},
                    {"bit": 2, "mask": RADAR_GATE_FLAG_RANGE_FOLDED, "name": "range_folded", "description": "range folded"},
                    {"bit": 3, "mask": RADAR_GATE_FLAG_FILTERED, "name": "filtered", "description": "filtered by QC"},
                    {"bit": 4, "mask": RADAR_GATE_FLAG_DERIVED, "name": "derived", "description": "derived product"},
                    {"bit": 5, "mask": RADAR_GATE_FLAG_DEALIASED, "name": "dealiased", "description": "dealiased velocity"}
                ],
                "radials": [{
                    "radial_index": 0,
                    "azimuth_deg": 0.0,
                    "elevation_deg": 0.0,
                    "azimuth_spacing_deg": 1.0,
                    "gate_count": 1,
                    "first_gate_range_m": 0,
                    "gate_spacing_m": 250,
                    "nyquist_velocity_ms": null,
                    "data_word_size_bits": null,
                    "scale": null,
                    "offset": null
                }],
                "qc": {}
            }))
            .unwrap(),
        )
        .expect("write sidecar manifest");
        write_test_f32_le(&sidecar_dir.join(RADAR_POLAR_VALUES_FILE), &[7.0]);
        fs::write(
            sidecar_dir.join(RADAR_POLAR_GATE_FLAGS_FILE),
            vec![RADAR_GATE_FLAG_VALID | RADAR_GATE_FLAG_DERIVED],
        )
        .expect("write flags");

        let lane = RadarTileLane::open(&root).expect("open radar lane");
        let query = RadarSampleQuery {
            layer: layer_id.to_string(),
            frame: frame_id.to_string(),
            product: Some("hhc".to_string()),
            tilt: Some(tilt_id.to_string()),
            lat: 35.0,
            lon: -97.0,
            method: Some("interpolated".to_string()),
        };
        let sample = lane.sample_json(&query).expect("sample sidecar");

        assert_eq!(sample["value"].as_f64(), Some(7.0));
        assert_eq!(sample["value_label"].as_str(), Some("Heavy Rain"));
        assert_eq!(sample["method"].as_str(), Some("nearest"));
        assert_eq!(sample["derived"].as_bool(), Some(true));

        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn radar_relative_coordinates_use_geodesic_round_trip() {
        let site_lat = 35.333;
        let site_lon = -97.277;
        let azimuth_deg = 90.0;
        let range_m = 460_000.0;

        let (lat, lon) = radar_polar_to_lat_lon(site_lat, site_lon, azimuth_deg, range_m);
        let polar = radar_lat_lon_to_polar(site_lat, site_lon, lat, lon);

        assert!(lat < site_lat - 0.05);
        assert!((polar.azimuth_deg - azimuth_deg).abs() < 0.001);
        assert!((polar.ground_range_m - range_m).abs() < 0.1);
    }

    #[test]
    fn radar_relative_coordinates_wrap_antimeridian() {
        let (lat, lon) = radar_polar_to_lat_lon(20.0, 179.8, 90.0, 80_000.0);
        let polar = radar_lat_lon_to_polar(20.0, 179.8, lat, lon);

        assert!(lon < -179.0);
        assert!((polar.azimuth_deg - 90.0).abs() < 0.001);
        assert!((polar.ground_range_m - 80_000.0).abs() < 0.1);
    }

    #[test]
    fn valid_times_from_run_id_rolls_over_days() {
        assert_eq!(
            valid_times_from_run_id("20260430_23z", &[0, 1, 25]),
            vec![
                "2026-04-30T23:00:00Z",
                "2026-05-01T00:00:00Z",
                "2026-05-02T00:00:00Z",
            ]
        );
    }

    #[test]
    fn valid_times_from_run_id_accepts_prefixed_uppercase_run_ids() {
        assert_eq!(
            valid_times_from_run_id("hrrr.20260430_06Z.current", &[0, 18]),
            vec!["2026-04-30T06:00:00Z", "2026-05-01T00:00:00Z"]
        );
    }

    #[test]
    fn valid_times_from_run_id_accepts_model_between_date_and_cycle() {
        assert_eq!(
            valid_times_from_run_id("20260503_hrrr_12z", &[0, 6]),
            vec!["2026-05-03T12:00:00Z", "2026-05-03T18:00:00Z"]
        );
    }

    #[test]
    fn valid_times_from_run_id_falls_back_for_unparsed_runs() {
        assert_eq!(
            valid_times_from_run_id("latest", &[3, 12]),
            vec!["latest+f003", "latest+f012"]
        );
    }

    #[test]
    fn wxa_metadata_reader_does_not_need_payload_read() {
        let root = temp_test_root("wxa_metadata_reader");
        let grid = SpatialGrid {
            model: "hrrr".to_string(),
            run_id: "20260503_hrrr_13z".to_string(),
            member: Some("control".to_string()),
            variable: "2m_temperature".to_string(),
            units: "degC".to_string(),
            forecast_hour: 0,
            nx: 2,
            ny: 2,
            grid_meta: json!({"type": "regular_latlon", "lat_start": 0.0, "lon_start": 0.0, "lat_step": 1.0, "lon_step": 1.0}),
            values: Arc::new(vec![1.0, 2.0, 3.0, 4.0]),
        };
        let mut grid_f1 = grid.clone();
        grid_f1.forecast_hour = 1;
        grid_f1.values = Arc::new(vec![5.0, 6.0, 7.0, 8.0]);

        let path = write_spatial_wxa_grids(
            &root,
            "hrrr",
            "20260503_hrrr_13z",
            Some("control"),
            "2m_temperature",
            &[grid, grid_f1],
        )
        .expect("write WXA");

        let (meta, index) = read_wxa_dense2d_metadata(&path).expect("read WXA metadata");
        assert_eq!(meta.model, "hrrr");
        assert_eq!(meta.forecast_hours, vec![0, 1]);
        assert_eq!(index.len(), 2);

        let full = read_spatial_wxa_grid(&path, 1).expect("read WXA grid");
        assert_eq!(full.forecast_hour, 1);
        assert_eq!(full.values.as_ref(), &vec![5.0, 6.0, 7.0, 8.0]);

        fs::remove_dir_all(root).ok();
    }
}
