param(
    [Parameter(Mandatory = $true)]
    [string]$Target,

    [string]$Profile = "release",

    [string]$OutDir = "dist"
)

$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$packageRoot = Join-Path $repoRoot $OutDir
$binaryName = if ($Target -like "*windows*") { "flashgrep.exe" } else { "flashgrep" }
$binaryPath = Join-Path $repoRoot "target/$Target/$Profile/$binaryName"
$stagingName = "flashgrep-$Target"
$stagingPath = Join-Path $packageRoot $stagingName
$mcpSourceRoot = Join-Path $repoRoot "src/mcp"

if (-not (Test-Path $binaryPath)) {
    throw "built binary not found: $binaryPath"
}

New-Item -ItemType Directory -Force -Path $packageRoot | Out-Null

if (Test-Path $stagingPath) {
    Remove-Item -Recurse -Force $stagingPath
}

New-Item -ItemType Directory -Force -Path (Join-Path $stagingPath "bin") | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $stagingPath "mcp") | Out-Null

$packagedBinary = Join-Path $stagingPath "bin/$binaryName"
$packagedServer = Join-Path $stagingPath "mcp/server.js"
$packagedLauncher = Join-Path $stagingPath "mcp/launcher.js"

Copy-Item $binaryPath $packagedBinary
Copy-Item (Join-Path $repoRoot "README.md") (Join-Path $stagingPath "README.md")
Copy-Item (Join-Path $mcpSourceRoot "README.md") (Join-Path $stagingPath "mcp/README.md")
Copy-Item (Join-Path $mcpSourceRoot "server.js") $packagedServer
Copy-Item (Join-Path $mcpSourceRoot "launcher.js") $packagedLauncher

if ($Target -notlike "*windows*") {
    & chmod +x $packagedBinary $packagedServer $packagedLauncher
}

if ($Target -like "*windows*") {
    $archivePath = Join-Path $packageRoot "$stagingName.zip"
    if (Test-Path $archivePath) {
        Remove-Item -Force $archivePath
    }
    Compress-Archive -Path $stagingPath -DestinationPath $archivePath
} else {
    $archivePath = Join-Path $packageRoot "$stagingName.tar.gz"
    if (Test-Path $archivePath) {
        Remove-Item -Force $archivePath
    }
    tar -C $packageRoot -czf $archivePath $stagingName
}

Write-Host "created package: $archivePath"
