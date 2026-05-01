param(
    [string]$BaseUrl = "http://127.0.0.1:8897",
    [int]$Requests = 100,
    [int]$Warmup = 5,
    [double]$Latitude = 35.22,
    [double]$Longitude = -97.44,
    [string]$Model = "",
    [string]$Run = "",
    [string]$Variable = "",
    [int]$ForecastHour = -1,
    [string]$OutFile = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$BaseUrl = $BaseUrl.TrimEnd("/")

function ConvertTo-QueryValue([object]$Value) {
    return [System.Uri]::EscapeDataString([string]$Value)
}

function Build-Url([string]$Path, [hashtable]$Query = @{}) {
    $pairs = @()
    foreach ($key in $Query.Keys) {
        if ($null -ne $Query[$key] -and [string]$Query[$key] -ne "") {
            $pairs += "$(ConvertTo-QueryValue $key)=$(ConvertTo-QueryValue $Query[$key])"
        }
    }
    if ($pairs.Count -gt 0) {
        return "$BaseUrl${Path}?$($pairs -join '&')"
    }
    return "$BaseUrl$Path"
}

function Invoke-Json([string]$Url) {
    return (Invoke-WebRequest -Uri $Url -TimeoutSec 30 -UseBasicParsing).Content | ConvertFrom-Json
}

function Percentile([double[]]$Values, [double]$P) {
    if ($Values.Count -eq 0) { return 0 }
    $sorted = @($Values | Sort-Object)
    $index = [math]::Ceiling(($P / 100.0) * $sorted.Count) - 1
    $index = [Math]::Max(0, [Math]::Min($sorted.Count - 1, $index))
    return [math]::Round($sorted[$index], 2)
}

function Measure-Url([string]$Name, [string]$Url, [int]$Count, [int]$WarmupCount) {
    for ($i = 0; $i -lt $WarmupCount; $i++) {
        Invoke-WebRequest -Uri $Url -TimeoutSec 30 -UseBasicParsing | Out-Null
    }

    $durations = New-Object System.Collections.Generic.List[double]
    $bytes = 0
    $failures = 0
    $total = [System.Diagnostics.Stopwatch]::StartNew()
    for ($i = 0; $i -lt $Count; $i++) {
        $sw = [System.Diagnostics.Stopwatch]::StartNew()
        try {
            $response = Invoke-WebRequest -Uri $Url -TimeoutSec 30 -UseBasicParsing
            $sw.Stop()
            if ([int]$response.StatusCode -lt 200 -or [int]$response.StatusCode -gt 299) {
                $failures++
            } else {
                $durations.Add($sw.Elapsed.TotalMilliseconds)
                $bytes += $response.RawContentLength
            }
        } catch {
            $sw.Stop()
            $failures++
        }
    }
    $total.Stop()
    $successes = $durations.Count
    [pscustomobject]@{
        name = $Name
        requests = $Count
        successes = $successes
        failures = $failures
        req_per_sec = if ($total.Elapsed.TotalSeconds -gt 0) { [math]::Round($successes / $total.Elapsed.TotalSeconds, 2) } else { 0 }
        p50_ms = Percentile $durations.ToArray() 50
        p95_ms = Percentile $durations.ToArray() 95
        p99_ms = Percentile $durations.ToArray() 99
        avg_bytes = if ($successes -gt 0) { [math]::Round($bytes / $successes, 0) } else { 0 }
        url = $Url
    }
}

Write-Host "WxStore sequential benchmark against $BaseUrl"

$modelsDoc = Invoke-Json (Build-Url "/v1/models")
$modelEntry = $null
if ($Model) {
    $matches = @($modelsDoc.spatial_loaded.models | Where-Object { $_.id -eq $Model } | Select-Object -First 1)
    if ($matches.Count -gt 0) { $modelEntry = $matches[0] }
} else {
    $matches = @($modelsDoc.spatial_loaded.models | Select-Object -First 1)
    if ($matches.Count -gt 0) { $modelEntry = $matches[0] }
}
if ($null -eq $modelEntry) {
    throw "No model found. Pass -Model with a loaded model ID."
}

if (-not $Model) { $Model = [string]$modelEntry.id }
if (-not $Run) {
    $Run = if ($modelEntry.latest_run) { [string]$modelEntry.latest_run } else { [string]@($modelEntry.runs | Select-Object -Last 1)[0] }
}

$varsDoc = Invoke-Json (Build-Url "/v1/variables" @{ model = $Model; run = $Run })
if (-not $Variable) {
    foreach ($candidate in @("vpd_2m", "2m_temperature", "temperature_2m", "dew_point_2m")) {
        if (@($varsDoc.variables) -contains $candidate) { $Variable = $candidate; break }
    }
    if (-not $Variable) { $Variable = [string]@($varsDoc.variables | Select-Object -First 1)[0] }
}
if ($ForecastHour -lt 0) {
    [array]$hours = @($varsDoc.available_hours.$Variable)
    $ForecastHour = if ($hours.Count -gt 0) { [int]$hours[0] } else { 0 }
}
$frame = "f{0:D3}" -f $ForecastHour
$layerUrl = Build-Url "/v1/mapbox/layers/$Model/$Run/$Variable" @{ hours = $ForecastHour; palette = "viridis"; range = "0,1"; base_url = $BaseUrl }
$testLat = $Latitude
$testLon = $Longitude
try {
    $layerDoc = Invoke-Json $layerUrl
    if ($null -ne $layerDoc.bounds -and @($layerDoc.bounds).Count -eq 4) {
        $bounds = @($layerDoc.bounds)
        $testLon = ([double]$bounds[0] + [double]$bounds[2]) / 2.0
        $testLat = ([double]$bounds[1] + [double]$bounds[3]) / 2.0
    }
} catch {
    Write-Warning "Could not probe layer bounds for $Model/$Run/$Variable ${frame}: $($_.Exception.Message)"
}

$urls = @(
    [pscustomobject]@{ name = "models"; url = Build-Url "/v1/models" },
    [pscustomobject]@{ name = "variables $Model/$Run"; url = Build-Url "/v1/variables" @{ model = $Model; run = $Run } },
    [pscustomobject]@{ name = "sample $Model/$Run/$Variable $frame"; url = Build-Url "/v1/sample" @{ model = $Model; run = $Run; variable = $Variable; forecast_hour = $ForecastHour; lat = $testLat; lon = $testLon } },
    [pscustomobject]@{ name = "forecast $Model/$Run"; url = Build-Url "/v1/forecast" @{ latitude = $testLat; longitude = $testLon; model = $Model; run = $Run; hourly = $Variable; forecast_hours = $ForecastHour } },
    [pscustomobject]@{ name = "mapbox layers $Model/$Run/$Variable $frame"; url = $layerUrl },
    [pscustomobject]@{ name = "mapbox tile $Model/$Run/$Variable $frame"; url = Build-Url "/v1/mapbox/tiles/$Model/$Run/$Variable/$frame/4/3/6.png" @{ palette = "viridis"; range = "0,1" } }
)

if ((@($varsDoc.variables) -contains "wind_u_10m_ms") -and (@($varsDoc.variables) -contains "wind_v_10m_ms")) {
    $urls += [pscustomobject]@{
        name = "wind-field $Model/$Run $frame"
        url = Build-Url "/v1/wind-field" @{
            model = $Model; run = $Run; forecast_hour = $ForecastHour
            u = "wind_u_10m_ms"; v = "wind_v_10m_ms"; stride = 80
            bounds = "-125,24,-66,50"
        }
    }
}

$results = foreach ($item in $urls) {
    Measure-Url $item.name $item.url $Requests $Warmup
}

$results | Format-Table -AutoSize name,requests,successes,failures,req_per_sec,p50_ms,p95_ms,p99_ms,avg_bytes

if ($OutFile) {
    $results | ConvertTo-Json -Depth 8 | Set-Content -Path $OutFile -Encoding UTF8
    Write-Host "Wrote $OutFile"
}

if (@($results | Where-Object { $_.failures -gt 0 }).Count -gt 0) {
    exit 1
}
