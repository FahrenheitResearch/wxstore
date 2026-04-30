# HRRR Capability Inventory

Date: `20260429`  Forecast Hour: `F000`

## Smoke Source Probe

- Planned product: `nat`
- Native file family: `wrfnat`

- rustwx-models resolves HRRR product `nat` to `wrfnat` URLs.
- wxsection_ref smoke loading uses MASSDEN on hybrid levels from wrfnat, with hybrid pressure for vertical placement.
- wxsection_ref also has a COLMD extraction path for column-integrated smoke.

| Source | Available | Latest Cycle | Probe URL |
| --- | --- | --- | --- |
| nomads | yes | 20260429 23Z | `https://nomads.ncep.noaa.gov/pub/data/nccf/com/hrrr/prod/hrrr.20260429/conus/hrrr.t23z.wrfnatf00.grib2.idx` |
| aws | yes | 20260429 23Z | `https://noaa-hrrr-bdp-pds.s3.amazonaws.com/hrrr.20260429/conus/hrrr.t23z.wrfnatf00.grib2.idx` |
| google | yes | 20260429 23Z | `https://storage.googleapis.com/high-resolution-rapid-refresh/hrrr.20260429/conus/hrrr.t23z.wrfnatf00.grib2.idx` |
| azure | yes | 20260429 23Z | `https://noaahrrr.blob.core.windows.net/hrrr/hrrr.20260429/conus/hrrr.t23z.wrfnatf00.grib2` |

## Direct Maps

- Available now: `52`
- Blocked on HRRR: `1`

- `10m_wind_gusts`: 10m AGL Wind Gusts  (`direct_native_exact`)
- `1km_reflectivity`: 1km AGL Reflectivity  (`direct_native_exact`)
- `200mb_absolute_vorticity_height_winds`: 200mb Absolute Vorticity / Height / Winds  (`direct_native_exact`)
- `200mb_height_winds`: 200mb Height / Winds  (`direct_native_exact`)
- `200mb_rh_height_winds`: 200mb RH / Height / Winds  (`direct_native_exact`)
- `200mb_temperature_height_winds`: 200mb Temperature / Height / Winds  (`direct_native_exact`)
- `250mb_height_winds`: 250mb Height / Winds  (`direct_native_exact`)
- `250mb_temperature_height_winds`: 250mb Temperature / Height / Winds  (`direct_native_exact`)
- `2m_dewpoint`: 2m AGL Dewpoint  (`direct_native_exact`)
- `2m_dewpoint_10m_winds`: 2m AGL Dewpoint / 10m Winds  (`direct_native_exact`)
- `2m_relative_humidity`: 2m AGL Relative Humidity  (`direct_native_exact`)
- `2m_relative_humidity_10m_winds`: 2m AGL Relative Humidity / 10m Winds  (`direct_native_exact`)
- `2m_temperature`: 2m AGL Temperature  (`direct_native_exact`)
- `2m_temperature_10m_winds`: 2m AGL Temperature / 10m Winds  (`direct_native_exact`)
- `300mb_absolute_vorticity_height_winds`: 300mb Absolute Vorticity / Height / Winds  (`direct_native_exact`)
- `300mb_height_winds`: 300mb Height / Winds  (`direct_native_exact`)
- `300mb_rh_height_winds`: 300mb RH / Height / Winds  (`direct_native_exact`)
- `300mb_temperature_height_winds`: 300mb Temperature / Height / Winds  (`direct_native_exact`)
- `500mb_absolute_vorticity_height_winds`: 500mb Absolute Vorticity / Height / Winds  (`direct_native_exact`)
- `500mb_height_winds`: 500mb Height / Winds  (`direct_native_exact`)
- `500mb_rh_height_winds`: 500mb RH / Height / Winds  (`direct_native_exact`)
- `500mb_temperature_height_winds`: 500mb Temperature / Height / Winds  (`direct_native_exact`)
- `700mb_absolute_vorticity_height_winds`: 700mb Absolute Vorticity / Height / Winds  (`direct_native_exact`)
- `700mb_dewpoint_height_winds`: 700mb Dewpoint / Height / Winds  (`direct_native_exact`)
- `700mb_height_winds`: 700mb Height / Winds  (`direct_native_exact`)
- `700mb_rh_height_winds`: 700mb RH / Height / Winds  (`direct_native_exact`)
- `700mb_temperature_height_winds`: 700mb Temperature / Height / Winds  (`direct_native_exact`)
- `850mb_absolute_vorticity_height_winds`: 850mb Absolute Vorticity / Height / Winds  (`direct_native_exact`)
- `850mb_dewpoint_height_winds`: 850mb Dewpoint / Height / Winds  (`direct_native_exact`)
- `850mb_height_winds`: 850mb Height / Winds  (`direct_native_exact`)
- `850mb_rh_height_winds`: 850mb RH / Height / Winds  (`direct_native_exact`)
- `850mb_temperature_height_winds`: 850mb Temperature / Height / Winds  (`direct_native_exact`)
- `categorical_freezing_rain`: Categorical Freezing Rain  (`direct_native_exact`)
- `categorical_ice_pellets`: Categorical Ice Pellets  (`direct_native_exact`)
- `categorical_rain`: Categorical Rain  (`direct_native_exact`)
- `categorical_snow`: Categorical Snow  (`direct_native_exact`)
- `cloud_cover`: Cloud Cover  (`direct_native_exact`)
- `cloud_cover_levels`: Cloud Cover, Levels  (`direct_native_composite_exact`)
- `composite_reflectivity`: Composite Reflectivity  (`direct_native_exact`)
- `composite_reflectivity_uh`: Composite Reflectivity / UH  (`direct_native_composite_exact`)
- `high_cloud_cover`: High Cloud Cover  (`direct_native_exact`)
- `low_cloud_cover`: Low Cloud Cover  (`direct_native_exact`)
- `middle_cloud_cover`: Middle Cloud Cover  (`direct_native_exact`)
- `mslp_10m_winds`: MSLP / 10m Winds  (`direct_native_exact`)
- `precipitable_water`: Precipitable Water  (`direct_native_exact`)
- `precipitation_type`: Precipitation Type  (`direct_native_composite_exact`)
- `simulated_ir_satellite`: Simulated IR Satellite  (`direct_native_exact`)
- `smoke_column`: Column-Integrated Smoke  (`direct_native_exact`)
- `smoke_pm25_native`: PM2.5 Smoke  (`direct_native_exact`)
- `total_qpf`: Total QPF  (`direct_native_exact`)
- `uh_2to5km`: Updraft Helicity 2-5 km  (`direct_native_exact`)
- `visibility`: Visibility  (`direct_native_exact`)

Blocked:
- `lightning_flash_density`: Lightning Flash Density

## Derived Maps

- Available now: `44`
- Blocked on HRRR: `3`

- `apparent_temperature_2m`: 2 m Apparent Temperature  (`canonical_derived`)
- `bulk_shear_0_1km`: 0-1 km Bulk Shear  (`canonical_derived`)
- `bulk_shear_0_6km`: 0-6 km Bulk Shear  (`canonical_derived`)
- `dewpoint_depression_2m`: 2 m Dewpoint Depression  (`canonical_derived`)
- `ecape_ehi_0_1km`: ECAPE EHI 0-1 km (EXP)  (`canonical_derived`)
- `ecape_ehi_0_3km`: ECAPE EHI 0-3 km (EXP)  (`canonical_derived`)
- `ecape_scp`: ECAPE SCP (EXP)  (`canonical_derived`)
- `ecape_stp`: ECAPE STP (EXP)  (`canonical_derived`)
- `ehi_0_1km`: EHI 0-1 km  (`canonical_derived`)
- `ehi_0_3km`: EHI 0-3 km  (`canonical_derived`)
- `fire_weather_composite`: Fire Weather Composite  (`canonical_derived`)
- `heat_index_2m`: 2 m Heat Index  (`canonical_derived`)
- `lapse_rate_0_3km`: 0-3 km Lapse Rate  (`canonical_derived`)
- `lapse_rate_700_500`: 700-500 mb Virtual Temperature Lapse Rate  (`canonical_derived`)
- `lifted_index`: Surface-Based Lifted Index  (`canonical_derived, native_exact`)
- `ml_ecape_derived_cape_ratio`: ML ECAPE / Derived CAPE Ratio (EXP)  (`canonical_derived`)
- `ml_ecape_native_cape_ratio`: ML ECAPE / Native CAPE Ratio (EXP)  (`canonical_derived`)
- `mlcape`: MLCAPE  (`canonical_derived, native_proxy`)
- `mlcin`: MLCIN  (`canonical_derived, native_proxy`)
- `mlecape`: MLECAPE  (`canonical_derived`)
- `mlecin`: MLECIN  (`canonical_derived`)
- `mu_ecape_derived_cape_ratio`: MU ECAPE / Derived CAPE Ratio (EXP)  (`canonical_derived`)
- `mu_ecape_native_cape_ratio`: MU ECAPE / Native CAPE Ratio (EXP)  (`canonical_derived`)
- `mucape`: MUCAPE  (`canonical_derived, native_proxy`)
- `mucin`: MUCIN  (`canonical_derived, native_proxy`)
- `muecape`: MUECAPE  (`canonical_derived`)
- `sb_ecape_derived_cape_ratio`: SB ECAPE / Derived CAPE Ratio (EXP)  (`canonical_derived`)
- `sb_ecape_native_cape_ratio`: SB ECAPE / Native CAPE Ratio (EXP)  (`canonical_derived`)
- `sbcape`: SBCAPE  (`canonical_derived, native_exact`)
- `sbcin`: SBCIN  (`canonical_derived, native_exact`)
- `sbecape`: SBECAPE  (`canonical_derived`)
- `sbecin`: SBECIN  (`canonical_derived`)
- `sblcl`: SBLCL  (`canonical_derived, native_exact`)
- `sbncape`: SBNCAPE  (`canonical_derived`)
- `scp_mu_0_3km_0_6km_proxy`: SCP (MU / 0-3 km / 0-6 km PROXY)  (`canonical_derived`)
- `srh_0_1km`: 0-1 km SRH  (`canonical_derived`)
- `srh_0_3km`: 0-3 km SRH  (`canonical_derived`)
- `stp_fixed`: STP (FIXED)  (`canonical_derived`)
- `temperature_advection_700mb`: 700 mb Temperature Advection  (`canonical_derived`)
- `temperature_advection_850mb`: 850 mb Temperature Advection  (`canonical_derived`)
- `theta_e_2m_10m_winds`: 2 m Theta-e, 10 m Wind Barbs  (`canonical_derived`)
- `vpd_2m`: 2 m Vapor Pressure Deficit  (`canonical_derived`)
- `wetbulb_2m`: 2 m Wet-Bulb Temperature  (`canonical_derived`)
- `wind_chill_2m`: 2 m Wind Chill  (`canonical_derived`)

Blocked:
- `scp`: SCP
- `scp_effective`: SCP (EFFECTIVE)
- `stp_effective`: STP (EFFECTIVE)

## Heavy Map Sets

- Available now: `1`
- Blocked on HRRR: `0`

- `severe_proof_panel`: Severe Map Set  (`canonical_derived`)

## Windowed Maps

- Available now: `49`
- Blocked on HRRR: `0`

- `10m_wind_0_24h_max`: 10 m Wind Speed (0-24 h max)  (`cheap_derived`)
- `10m_wind_0_48h_max`: 10 m Wind Speed (0-48 h max)  (`cheap_derived`)
- `10m_wind_1h_max`: 10 m Wind Speed (1 h max)  (`cheap_derived`)
- `10m_wind_24_48h_max`: 10 m Wind Speed (24-48 h max)  (`cheap_derived`)
- `10m_wind_run_max`: 10 m Wind Speed (run max)  (`cheap_derived`)
- `2m_dewpoint_0_24h_max`: 2 m Dewpoint (0-24 h max)  (`cheap_derived`)
- `2m_dewpoint_0_24h_min`: 2 m Dewpoint (0-24 h min)  (`cheap_derived`)
- `2m_dewpoint_0_24h_range`: 2 m Dewpoint Range (0-24 h)  (`cheap_derived`)
- `2m_dewpoint_0_48h_max`: 2 m Dewpoint (0-48 h max)  (`cheap_derived`)
- `2m_dewpoint_0_48h_min`: 2 m Dewpoint (0-48 h min)  (`cheap_derived`)
- `2m_dewpoint_0_48h_range`: 2 m Dewpoint Range (0-48 h)  (`cheap_derived`)
- `2m_dewpoint_24_48h_max`: 2 m Dewpoint (24-48 h max)  (`cheap_derived`)
- `2m_dewpoint_24_48h_min`: 2 m Dewpoint (24-48 h min)  (`cheap_derived`)
- `2m_dewpoint_24_48h_range`: 2 m Dewpoint Range (24-48 h)  (`cheap_derived`)
- `2m_rh_0_24h_max`: 2 m Relative Humidity (0-24 h max)  (`cheap_derived`)
- `2m_rh_0_24h_min`: 2 m Relative Humidity (0-24 h min)  (`cheap_derived`)
- `2m_rh_0_24h_range`: 2 m Relative Humidity Range (0-24 h)  (`cheap_derived`)
- `2m_rh_0_48h_max`: 2 m Relative Humidity (0-48 h max)  (`cheap_derived`)
- `2m_rh_0_48h_min`: 2 m Relative Humidity (0-48 h min)  (`cheap_derived`)
- `2m_rh_0_48h_range`: 2 m Relative Humidity Range (0-48 h)  (`cheap_derived`)
- `2m_rh_24_48h_max`: 2 m Relative Humidity (24-48 h max)  (`cheap_derived`)
- `2m_rh_24_48h_min`: 2 m Relative Humidity (24-48 h min)  (`cheap_derived`)
- `2m_rh_24_48h_range`: 2 m Relative Humidity Range (24-48 h)  (`cheap_derived`)
- `2m_temp_0_24h_max`: 2 m Temperature (0-24 h max)  (`cheap_derived`)
- `2m_temp_0_24h_min`: 2 m Temperature (0-24 h min)  (`cheap_derived`)
- `2m_temp_0_24h_range`: 2 m Temperature Range (0-24 h)  (`cheap_derived`)
- `2m_temp_0_48h_max`: 2 m Temperature (0-48 h max)  (`cheap_derived`)
- `2m_temp_0_48h_min`: 2 m Temperature (0-48 h min)  (`cheap_derived`)
- `2m_temp_0_48h_range`: 2 m Temperature Range (0-48 h)  (`cheap_derived`)
- `2m_temp_24_48h_max`: 2 m Temperature (24-48 h max)  (`cheap_derived`)
- `2m_temp_24_48h_min`: 2 m Temperature (24-48 h min)  (`cheap_derived`)
- `2m_temp_24_48h_range`: 2 m Temperature Range (24-48 h)  (`cheap_derived`)
- `2m_vpd_0_24h_max`: 2 m Vapor Pressure Deficit (0-24 h max)  (`cheap_derived`)
- `2m_vpd_0_24h_min`: 2 m Vapor Pressure Deficit (0-24 h min)  (`cheap_derived`)
- `2m_vpd_0_24h_range`: 2 m Vapor Pressure Deficit Range (0-24 h)  (`cheap_derived`)
- `2m_vpd_0_48h_max`: 2 m Vapor Pressure Deficit (0-48 h max)  (`cheap_derived`)
- `2m_vpd_0_48h_min`: 2 m Vapor Pressure Deficit (0-48 h min)  (`cheap_derived`)
- `2m_vpd_0_48h_range`: 2 m Vapor Pressure Deficit Range (0-48 h)  (`cheap_derived`)
- `2m_vpd_24_48h_max`: 2 m Vapor Pressure Deficit (24-48 h max)  (`cheap_derived`)
- `2m_vpd_24_48h_min`: 2 m Vapor Pressure Deficit (24-48 h min)  (`cheap_derived`)
- `2m_vpd_24_48h_range`: 2 m Vapor Pressure Deficit Range (24-48 h)  (`cheap_derived`)
- `qpf_12h`: 12-h QPF  (`cheap_derived`)
- `qpf_1h`: 1-h QPF  (`cheap_derived`)
- `qpf_24h`: 24-h QPF  (`cheap_derived`)
- `qpf_6h`: 6-h QPF  (`cheap_derived`)
- `qpf_total`: Total QPF  (`cheap_derived`)
- `uh_2to5km_1h_max`: Updraft Helicity: 2-5 km AGL (1 h max)  (`cheap_derived`)
- `uh_2to5km_3h_max`: Updraft Helicity: 2-5 km AGL (3 h max)  (`cheap_derived`)
- `uh_2to5km_run_max`: Updraft Helicity: 2-5 km AGL (run max)  (`cheap_derived`)
## Cross Sections

- Declared styles: `20`
- Pressure-section builder wired now: `19`

| Product | Group | Units | Builder Wired |
| --- | --- | --- | --- |
| `temperature` | Temperature & Moisture | `C` | yes |
| `wind_speed` | Wind & Dynamics | `kt` | yes |
| `theta_e` | Temperature & Moisture | `K` | yes |
| `rh` | Temperature & Moisture | `%` | yes |
| `q` | Temperature & Moisture | `g/kg` | yes |
| `omega` | Wind & Dynamics | `hPa/hr` | yes |
| `vorticity` | Wind & Dynamics | `1e-5 s^-1` | yes |
| `shear` | Wind & Dynamics | `1e-3 s^-1` | yes |
| `lapse_rate` | Clouds & Precip | `C/km` | yes |
| `cloud` | Clouds & Precip | `g/kg` | yes |
| `cloud_total` | Clouds & Precip | `g/kg` | yes |
| `wetbulb` | Temperature & Moisture | `C` | yes |
| `icing` | Clouds & Precip | `g/kg` | yes |
| `frontogenesis` | Clouds & Precip | `K/100km/3hr` | yes |
| `smoke` | Hazards & Composites | `ug/m^3` | no |
| `vpd` | Temperature & Moisture | `hPa` | yes |
| `dewpoint_dep` | Temperature & Moisture | `C` | yes |
| `moisture_transport` | Wind & Dynamics | `g*m/kg/s` | yes |
| `pv` | Wind & Dynamics | `PVU` | yes |
| `fire_wx` | Hazards & Composites | `RH% + wind` | yes |

## Wxsection-Inspired Missing Map Candidates

| Candidate | Priority | Upstream Basis | Inputs |
| --- | --- | --- | --- |
| `smoke_pm25_native` | high | wxsection smoke style + HRRR wrfnat MASSDEN on hybrid levels | `wrfnat MASSDEN (disc 0 / cat 20 / num 0), hybrid pressure` |

  Highest-value smoke add. Enables both plan-view smoke maps and true native-level smoke cross sections.

| `smoke_column` | high | wxsection COLMD extraction path | `wrfnat COLMD entire atmosphere` |

  Cheaper overview smoke map if the column field is present in the native file.

| `vpd_2m` | high | wxsection fire-weather style family | `2m temperature, 2m RH or dewpoint` |

  Useful fire-weather scalar and straightforward from current surface thermo fields.

| `dewpoint_depression_2m` | medium | wxsection dry-layer diagnostics | `2m temperature, 2m dewpoint` |

  Cheap diagnostic that pairs well with cloud-base / dryline style maps.

| `fire_weather_composite` | high | wxsection fire_wx composite | `2m RH, 10m wind, VPD` |

  Public-facing fire-weather composite candidate; likely best implemented as a clean derived lane product.

| `omega_700mb` | medium | wxsection omega style | `pressure vertical velocity, 700mb pressure level` |

  Straight synoptic add once the vertical-velocity field is wired through grib extraction.

| `wetbulb_2m` | medium | wxsection wetbulb style | `2m temperature, 2m RH or dewpoint` |

  Strong winter-weather / fire-weather crossover field and likely cheaper than many severe diagnostics.

| `frontogenesis_700_850mb` | medium | wxsection frontogenesis style | `temperature gradient, wind deformation/confluence` |

  Good winter-weather add, but needs a defensible derived implementation rather than a placeholder.

| `moisture_transport_850mb` | medium | wxsection moisture_transport style | `specific humidity, wind speed` |

  Simple, meteorologist-friendly plume diagnostic once specific humidity is exposed cleanly.

| `potential_vorticity` | low | wxsection pv style | `theta, vorticity, pressure derivatives` |

  More specialized and costlier, but valuable later for jet/tropopause work.

