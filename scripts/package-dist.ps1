param(
    [Parameter(Mandatory = $true)]
    [string]$Target,

    [string]$Profile = "release",

    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$packageRoot = Join-Path $repoRoot $OutDir
$binaryExtension = if ($Target -like "*windows*") { ".exe" } else { "" }
$binaryName = "flashgrep$binaryExtension"
$binaryPath = Join-Path $repoRoot "target/$Target/$Profile/$binaryName"
$artifactPath = Join-Path $packageRoot "flashgrep-$Target$binaryExtension"

if (-not (Test-Path $binaryPath)) {
    throw "built binary not found: $binaryPath"
}

New-Item -ItemType Directory -Force -Path $packageRoot | Out-Null

if (Test-Path $artifactPath) {
    Remove-Item -Force $artifactPath
}

Copy-Item $binaryPath $artifactPath

if ($Target -notlike "*windows*") {
    & chmod +x $artifactPath
}

Write-Host "created artifact: $artifactPath"
