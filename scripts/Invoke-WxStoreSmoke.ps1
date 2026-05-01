param(
    [string]$BaseUrl = "http://127.0.0.1:8897",
    [string[]]$Models = @(),
    [int]$RunsPerModel = 1,
    [double]$Latitude = 35.22,
    [double]$Longitude = -97.44,
    [int]$TileZ = 4,
    [int]$TileX = 3,
    [int]$TileY = 6,
    [int]$TimeoutSec = 30,
    [string]$OutFile = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$BaseUrl = $BaseUrl.TrimEnd("/")
$results = New-Object System.Collections.Generic.List[object]

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

function Invoke-WxJson([string]$Name, [string]$Url) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $response = Invoke-WebRequest -Uri $Url -TimeoutSec $TimeoutSec -UseBasicParsing
        $sw.Stop()
        if ([int]$response.StatusCode -lt 200 -or [int]$response.StatusCode -gt 299) {
            throw "HTTP $($response.StatusCode)"
        }
        $json = $response.Content | ConvertFrom-Json
        $script:results.Add([pscustomobject]@{
            name = $Name; status = "pass"; code = [int]$response.StatusCode
            ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2); bytes = $response.RawContentLength
            url = $Url; detail = ""
        })
        return $json
    } catch {
        $sw.Stop()
        $script:results.Add([pscustomobject]@{
            name = $Name; status = "fail"; code = 0
            ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2); bytes = 0
            url = $Url; detail = $_.Exception.Message
        })
        return $null
    }
}

function Invoke-WxBytes([string]$Name, [string]$Url, [switch]$ExpectPng) {
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $response = Invoke-WebRequest -Uri $Url -TimeoutSec $TimeoutSec -UseBasicParsing
        $sw.Stop()
        if ([int]$response.StatusCode -lt 200 -or [int]$response.StatusCode -gt 299) {
            throw "HTTP $($response.StatusCode)"
        }
        $bytes = [byte[]]$response.Content
        if ($ExpectPng) {
            $signature = [byte[]](0x89, 0x50, 0x4E, 0x47)
            for ($i = 0; $i -lt $signature.Length; $i++) {
                if ($bytes.Length -le $i -or $bytes[$i] -ne $signature[$i]) {
                    throw "response did not start with a PNG signature"
                }
            }
        }
        $script:results.Add([pscustomobject]@{
            name = $Name; status = "pass"; code = [int]$response.StatusCode
            ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2); bytes = $bytes.Length
            url = $Url; detail = ""
        })
    } catch {
        $sw.Stop()
        $script:results.Add([pscustomobject]@{
            name = $Name; status = "fail"; code = 0
            ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 2); bytes = 0
            url = $Url; detail = $_.Exception.Message
        })
    }
}

function Add-Skip([string]$Name, [string]$Detail) {
    $script:results.Add([pscustomobject]@{
        name = $Name; status = "skip"; code = 0; ms = 0; bytes = 0; url = ""; detail = $Detail
    })
}

function Pick-Variable($VariablesDoc) {
    $available = $VariablesDoc.available_hours
    $variables = @($VariablesDoc.variables)
    foreach ($preferred in @("vpd_2m", "2m_temperature", "temperature_2m", "dew_point_2m", "wind_gusts_10m")) {
        if ($variables -contains $preferred -and $available.PSObject.Properties.Name -contains $preferred) {
            return $preferred
        }
    }
    foreach ($variable in $variables) {
        if ($available.PSObject.Properties.Name -contains $variable) {
            return $variable
        }
    }
    return $null
}

function Get-Hours($VariablesDoc, [string]$Variable) {
    if ($null -eq $VariablesDoc.available_hours.$Variable) {
        return @()
    }
    return ,@($VariablesDoc.available_hours.$Variable)
}

Write-Host "WxStore smoke validation against $BaseUrl"

$modelsDoc = Invoke-WxJson "GET /v1/models" (Build-Url "/v1/models")
if ($null -eq $modelsDoc) {
    throw "Unable to load /v1/models from $BaseUrl"
}

$modelEntries = @($modelsDoc.spatial_loaded.models)
if ($Models.Count -gt 0) {
    $modelEntries = @($modelEntries | Where-Object { $Models -contains $_.id })
}
if ($modelEntries.Count -eq 0) {
    throw "No spatial models matched. Check /v1/models or pass -Models with loaded model IDs."
}

foreach ($modelEntry in $modelEntries) {
    $model = [string]$modelEntry.id
    $runs = @($modelEntry.runs | Select-Object -Last $RunsPerModel)
    if ($runs.Count -eq 0 -and $modelEntry.latest_run) {
        $runs = @([string]$modelEntry.latest_run)
    }
    foreach ($run in $runs) {
        $variablesUrl = Build-Url "/v1/variables" @{ model = $model; run = $run }
        $variablesDoc = Invoke-WxJson "GET /v1/variables $model/$run" $variablesUrl
        if ($null -eq $variablesDoc) {
            continue
        }

        $variable = Pick-Variable $variablesDoc
        if (-not $variable) {
            Add-Skip "$model/$run model smoke" "no variable with available hours"
            continue
        }
        [array]$hours = Get-Hours $variablesDoc $variable
        if ($hours.Count -eq 0) {
            Add-Skip "$model/$run $variable smoke" "no available forecast hours"
            continue
        }
        $hour = [int]$hours[0]
        $frame = "f{0:D3}" -f $hour

        $forecastVars = @($variablesDoc.variables | Where-Object { $variablesDoc.available_hours.PSObject.Properties.Name -contains $_ } | Select-Object -First 3)
        if ($forecastVars.Count -eq 0) {
            $forecastVars = @($variable)
        }

        $layerDoc = Invoke-WxJson "GET /v1/mapbox/layers $model/$run/$variable $frame" (Build-Url "/v1/mapbox/layers/$model/$run/$variable" @{
            hours = $hour; palette = "viridis"; range = "0,1"; base_url = $BaseUrl
        })

        $testLat = $Latitude
        $testLon = $Longitude
        if ($null -ne $layerDoc -and $null -ne $layerDoc.bounds -and @($layerDoc.bounds).Count -eq 4) {
            $bounds = @($layerDoc.bounds)
            $testLon = ([double]$bounds[0] + [double]$bounds[2]) / 2.0
            $testLat = ([double]$bounds[1] + [double]$bounds[3]) / 2.0
        }

        Invoke-WxJson "GET /v1/sample $model/$run/$variable $frame" (Build-Url "/v1/sample" @{
            model = $model; run = $run; variable = $variable; forecast_hour = $hour
            lat = $testLat; lon = $testLon
        }) | Out-Null

        Invoke-WxBytes "GET /v1/mapbox/tiles $model/$run/$variable $frame" (Build-Url "/v1/mapbox/tiles/$model/$run/$variable/$frame/$TileZ/$TileX/$TileY.png" @{
            palette = "viridis"; range = "0,1"
        }) -ExpectPng

        Invoke-WxJson "GET /v1/forecast $model/$run" (Build-Url "/v1/forecast" @{
            latitude = $testLat; longitude = $testLon; model = $model; run = $run
            hourly = ($forecastVars -join ","); forecast_hours = $hour
        }) | Out-Null

        $hasU = @($variablesDoc.variables) -contains "wind_u_10m_ms"
        $hasV = @($variablesDoc.variables) -contains "wind_v_10m_ms"
        if ($hasU -and $hasV) {
            Invoke-WxJson "GET /v1/wind-field $model/$run $frame" (Build-Url "/v1/wind-field" @{
                model = $model; run = $run; forecast_hour = $hour
                u = "wind_u_10m_ms"; v = "wind_v_10m_ms"; stride = 80
                bounds = "-125,24,-66,50"
            }) | Out-Null
        } else {
            Add-Skip "GET /v1/wind-field $model/$run" "wind_u_10m_ms and wind_v_10m_ms are not both available"
        }
    }
}

$results | Format-Table -AutoSize name,status,code,ms,bytes,detail

if ($OutFile) {
    $results | ConvertTo-Json -Depth 8 | Set-Content -Path $OutFile -Encoding UTF8
    Write-Host "Wrote $OutFile"
}

$failures = @($results | Where-Object { $_.status -eq "fail" })
if ($failures.Count -gt 0) {
    exit 1
}
