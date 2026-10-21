# Package a release binary exactly as the release workflow publishes it (Windows).
#
#   scripts/package.ps1 <target-triple> <path to agent-sidekick.exe> <output dir>
#
# Writes <output dir>\agent-sidekick-<target>.zip (agent-sidekick.exe, README.md, LICENSE-MIT and
# LICENSE-APACHE at the archive root) and adds/replaces its line in <output dir>\SHA256SUMS
# ("<sha256>  <file>", lowercase hash, LF line endings, no BOM). Used by
# .github/workflows/release.yml and by the install dry-run in ci.yml.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$Target,
    [Parameter(Mandatory = $true)][string]$Binary,
    [Parameter(Mandatory = $true)][string]$OutDir
)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot
$Bin = 'agent-sidekick'
$Name = "$Bin-$Target.zip"

if (-not (Test-Path -LiteralPath $Binary -PathType Leaf)) { throw "binary not found: $Binary" }
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null
$OutDir = (Resolve-Path -LiteralPath $OutDir).Path

$Stage = Join-Path ([IO.Path]::GetTempPath()) ('agent-sidekick-pkg-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Stage | Out-Null
try {
    Copy-Item -LiteralPath $Binary -Destination (Join-Path $Stage "$Bin.exe")
    foreach ($File in 'README.md', 'LICENSE-MIT', 'LICENSE-APACHE') {
        Copy-Item -LiteralPath (Join-Path $Root $File) -Destination $Stage
    }
    $Zip = Join-Path $OutDir $Name
    if (Test-Path -LiteralPath $Zip) { Remove-Item -LiteralPath $Zip -Force }
    Compress-Archive -Path (Join-Path $Stage '*') -DestinationPath $Zip
} finally {
    Remove-Item -Recurse -Force -LiteralPath $Stage -ErrorAction SilentlyContinue
}

$Hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $Zip).Hash.ToLowerInvariant()
$Sums = Join-Path $OutDir 'SHA256SUMS'
$Lines = @()
if (Test-Path -LiteralPath $Sums) {
    $Lines = @([IO.File]::ReadAllLines($Sums) | Where-Object { $_ -and -not $_.EndsWith("  $Name") })
}
$Lines += "$Hash  $Name"
$Utf8NoBom = New-Object Text.UTF8Encoding $false
[IO.File]::WriteAllText($Sums, (($Lines -join "`n") + "`n"), $Utf8NoBom)
$Zip
