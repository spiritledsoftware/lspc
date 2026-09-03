Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

if ($env:OS -ne "Windows_NT") {
    throw "lspctl installer: Windows is required"
}
$architecture = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
if ($architecture -ne "AMD64") {
    throw "lspctl installer: unsupported architecture: $architecture"
}

$repository = "spiritledsoftware/lspctl"
$releaseRoot = if ($env:LSPCTL_RELEASE_ROOT) { $env:LSPCTL_RELEASE_ROOT } else { "https://github.com/$repository/releases" }
$version = if ($env:LSPCTL_VERSION) {
    $env:LSPCTL_VERSION
} else {
    (Invoke-RestMethod -Uri "https://api.github.com/repos/$repository/releases/latest").tag_name
}
$version = $version -replace '^v', ''
if ($version -notmatch '^[0-9A-Za-z.+-]+$') {
    throw "lspctl installer: invalid release version: $version"
}

$target = "x86_64-pc-windows-msvc"
$archive = "lspctl-v$version-$target.zip"
$url = "$releaseRoot/download/v$version/$archive"
$temp = Join-Path ([IO.Path]::GetTempPath()) ("lspctl-" + [Guid]::NewGuid())
New-Item -ItemType Directory -Path $temp | Out-Null

try {
    $archivePath = Join-Path $temp $archive
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $archivePath
    Invoke-WebRequest -UseBasicParsing -Uri "$url.sha256" -OutFile "$archivePath.sha256"
    $expected = ((Get-Content -Raw "$archivePath.sha256") -split '\s+')[0].ToLowerInvariant()
    $actual = (Get-FileHash -Algorithm SHA256 $archivePath).Hash.ToLowerInvariant()
    if ($expected -notmatch '^[a-f0-9]{64}$' -or $actual -ne $expected) {
        throw "lspctl installer: download checksum mismatch"
    }

    Expand-Archive -Path $archivePath -DestinationPath $temp
    if ($env:LSPCTL_INSTALL_DIR) {
        $installDir = $env:LSPCTL_INSTALL_DIR
    } elseif ($env:LOCALAPPDATA) {
        $installDir = Join-Path $env:LOCALAPPDATA "Programs\lspctl"
    } else {
        throw "lspctl installer: LOCALAPPDATA must be set"
    }
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    $destination = Join-Path $installDir "lspctl.exe"
    Copy-Item -Force (Join-Path $temp "lspctl-v$version-$target\lspctl.exe") $destination
    if ([Environment]::OSVersion.Platform -eq [PlatformID]::Win32NT) {
        Unblock-File -Path $destination
    }
    Write-Output "Installed lspctl $version to $destination"
    Write-Output "Add $installDir to PATH to run lspctl."
} finally {
    Remove-Item -Recurse -Force $temp -ErrorAction SilentlyContinue
}
