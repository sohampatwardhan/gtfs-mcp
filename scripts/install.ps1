# Install the Windows x64 gtfs-mcp binary from GitHub Releases.
# $env:GTFS_MCP_VERSION = "X.Y.Z" selects a release. Unset uses the latest release.
# $env:PREFIX defaults to $env:LOCALAPPDATA\gtfs-mcp (the exe is placed in that directory).
$ErrorActionPreference = "Stop"

$repo = "sohampatwardhan/gtfs-mcp"
$version = $env:GTFS_MCP_VERSION
if ([string]::IsNullOrWhiteSpace($version)) {
    $base = "https://github.com/$repo/releases/latest/download"
} else {
    $version = $version.TrimStart("v")
    $base = "https://github.com/$repo/releases/download/v$version"
}

$archive = "gtfs-mcp-x86_64-pc-windows-msvc.zip"
$temp = Join-Path ([System.IO.Path]::GetTempPath()) ("gtfs-mcp-" + [guid]::NewGuid().ToString("n"))
New-Item -ItemType Directory -Path $temp | Out-Null
try {
    $archivePath = Join-Path $temp $archive
    $sumsPath = Join-Path $temp "sha256sums.txt"
    Invoke-WebRequest -Uri "$base/$archive" -OutFile $archivePath
    Invoke-WebRequest -Uri "$base/sha256sums.txt" -OutFile $sumsPath

    $expected = $null
    foreach ($line in Get-Content $sumsPath) {
        if ($line -match "^([0-9a-fA-F]{64})  \Q$archive\E$") {
            $expected = $Matches[1].ToLowerInvariant()
        }
    }
    if (-not $expected) {
        throw "sha256sums.txt has no entry for $archive"
    }
    $actual = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLowerInvariant()
    if ($actual -ne $expected) {
        throw "checksum mismatch for $archive"
    }

    Expand-Archive -Path $archivePath -DestinationPath $temp
    $prefix = if ([string]::IsNullOrWhiteSpace($env:PREFIX)) {
        Join-Path $env:LOCALAPPDATA "gtfs-mcp"
    } else {
        $env:PREFIX
    }
    New-Item -ItemType Directory -Force -Path $prefix | Out-Null
    Copy-Item -Force (Join-Path $temp "gtfs-mcp.exe") (Join-Path $prefix "gtfs-mcp.exe")
    Write-Output "installed $(Join-Path $prefix 'gtfs-mcp.exe')"
} finally {
    Remove-Item -Recurse -Force $temp
}
