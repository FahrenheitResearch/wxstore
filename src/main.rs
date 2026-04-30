use std::{
    collections::{BTreeMap, HashMap, VecDeque},
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
    response::{IntoResponse, Response},
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
}

#[derive(Parser, Clone)]
struct ServeArgs {
    #[arg(long)]
    profile_store: PathBuf,
    #[arg(long)]
    diagnostic_store: Option<PathBuf>,
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
            println!(
                "{}",
                serde_json::to_string_pretty(&store_status(&profile, diagnostic.as_ref()))?
            );
            Ok(())
        }
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
        cache: RwLock::new(ResponseCache::default()),
    });

    let app = Router::new()
        .route("/v1/status", get(status))
        .route("/api/status", get(status))
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

struct AppState {
    profile: Arc<ProfileLane>,
    diagnostic: Option<Arc<DiagnosticLane>>,
    cache: RwLock<ResponseCache>,
}

#[derive(Default)]
struct ResponseCache {
    entries: HashMap<String, Bytes>,
    order: VecDeque<String>,
}

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
    Json(store_status(&state.profile, state.diagnostic.as_deref()))
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
            "surface_ts": "unavailable"
        },
        "canonical_run_url": format!("/v1/runs/{}/{}/{}", manifest.model, manifest.domain, manifest.run_id),
    })))
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
        return binary_response_for_point(state, point, requested_lat, requested_lon, req, immutable)
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
    Ok(bytes_response(bytes, "application/json", cache_hit, immutable))
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

fn store_status(profile: &ProfileLane, diagnostic: Option<&DiagnosticLane>) -> Value {
    json!({
        "schema": "wxstore.status.v1",
        "service": "wxstore",
        "ok": true,
        "loaded_run": run_manifest_json(profile, diagnostic),
        "lanes": {
            "profile_pressure_core": profile.lane_manifest_json(),
            "diag_scalar_basic": diagnostic.map(DiagnosticLane::lane_manifest_json).unwrap_or_else(|| json!({"status": "unavailable"})),
            "surface_ts": {"status": "unavailable", "reason": "surface 0-48 lane not built on this node"}
        },
        "cache": {"entries_limit": CACHE_LIMIT}
    })
}

fn run_manifest_json(profile: &ProfileLane, diagnostic: Option<&DiagnosticLane>) -> Value {
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
            {"id": "surface_ts", "status": "unavailable"}
        ],
        "provenance": {
            "builder": "wxstore v0 from custom .wxp lane",
            "source": "rustwx/orwx generated HRRR profile lane",
            "notes": [
                "No Open-Meteo file format or code is used by this service.",
                "Diagnostic lane currently wraps a sparse precomputed diagnostic brick until dense diagnostics are built."
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
                    Some(f64::from(*value) / f64::from(series.scale_factor)
                        - f64::from(series.add_offset))
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
            let level = profile.manifest.levels_hpa[index % profile.manifest.levels_hpa.len()] as f64;
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
        Some(diagnostic_json(diagnostic, point, &req.hours, req.diagnostic_mode)?)
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
            let response = diagnostic.sample_point(point.lat, point.lon, &req.hours, req.diagnostic_mode);
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
    let mut output = Vec::with_capacity(WXBIN_MAGIC.len() + 12 + header_bytes.len() + payload.len());
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
            diagnostic_mode: DiagnosticMode::parse(query.diagnostics.as_deref().or(query.diag.as_deref())),
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
            diagnostic_mode: DiagnosticMode::parse(query.diagnostics.as_deref().or(query.diag.as_deref())),
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
        "sounding" | "core" => SOUNDING_CORE.iter().map(|value| value.to_string()).collect(),
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
        self.manifest.variables.iter().map(|v| v.name.clone()).collect()
    }

    fn has_variable(&self, variable: &str) -> bool {
        self.manifest.variables.iter().any(|item| item.name == variable)
    }

    fn locate_nearest(&self, lat: f64, lon: f64) -> Result<GridPoint> {
        if !lat.is_finite() || !lon.is_finite() {
            bail!("lat/lon must be finite");
        }
        let (x, y) = if self.manifest.model == "hrrr" && self.manifest.nx == 1799 && self.manifest.ny == 1059 {
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
            bail!("grid point ({x}, {y}) outside {}x{}", self.manifest.nx, self.manifest.ny);
        }
        let (lat, lon) = if self.manifest.model == "hrrr" && self.manifest.nx == 1799 && self.manifest.ny == 1059 {
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
            bail!("decoded chunk length mismatch: {} != {expected_len}", decoded.len());
        }
        let mut values = Vec::with_capacity(forecast_hours.len() * file.header.levels_len);
        for hour_index in hour_indices {
            for level_index in 0..file.header.levels_len {
                let value_index =
                    ((local_x * file.header.levels_len + level_index) * file.header.hours_len)
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
        let file = Arc::new(ProfileFile { mmap, header, index });
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
            .filter_map(|hour| self.payload.manifest.hours.iter().position(|stored| stored == hour))
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
        if lon < 0.0 { lon + 360.0 } else { lon }
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

fn validate_canonical(profile: &ProfileLane, model: &str, domain: &str, run: &str) -> Result<(), ApiError> {
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

fn bytes_response(bytes: Bytes, content_type: &'static str, cache_hit: bool, immutable: bool) -> Response {
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
    for part in value.split(',').map(str::trim).filter(|part| !part.is_empty()) {
        if let Some((start, end)) = part.split_once('-') {
            let start = start.parse::<u8>().with_context(|| format!("invalid hour range '{part}'"))?;
            let end = end.parse::<u8>().with_context(|| format!("invalid hour range '{part}'"))?;
            if end < start {
                bail!("hour range '{part}' is reversed");
            }
            hours.extend(start..=end);
        } else {
            hours.push(part.parse::<u8>().with_context(|| format!("invalid hour '{part}'"))?);
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
    hours.iter().map(u8::to_string).collect::<Vec<_>>().join(",")
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
