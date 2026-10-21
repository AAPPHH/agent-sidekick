# Installer for agent-sidekick (Windows).
#
#   irm https://github.com/OWNER/agent-sidekick/releases/latest/download/install.ps1 | iex
#
# Environment:
#   REPO                        GitHub repository "owner/name" (default: OWNER/agent-sidekick;
#                               the placeholder must be replaced with the real repository)
#   VERSION                     release to install, e.g. 0.1.0 or v0.1.0 (default: latest)
#   AGENT_SIDEKICK_INSTALL_DIR  target directory (default: %LOCALAPPDATA%\Programs\agent-sidekick)
#
# Only an x86_64 build is published. On ARM64 Windows it runs via x64 emulation.

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is very slow with the progress bar in 5.1
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = if ($env:REPO) { $env:REPO } else { 'OWNER/agent-sidekick' }
$Version = if ($env:VERSION) { $env:VERSION } else { 'latest' }
$InstallDir = if ($env:AGENT_SIDEKICK_INSTALL_DIR) { $env:AGENT_SIDEKICK_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\agent-sidekick' }
$Bin = 'agent-sidekick'
$Target = 'x86_64-pc-windows-msvc'

$arch = $env:PROCESSOR_ARCHITECTURE
if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
switch ($arch) {
    'AMD64' { }
    'ARM64' { Write-Host 'ARM64 Windows detected: installing the x86_64 build (runs via emulation).' }
    default { throw "unsupported architecture: $arch" }
}

if ($Version -eq 'latest') {
    $Base = "https://github.com/$Repo/releases/latest/download"
} elseif ($Version.StartsWith('v')) {
    $Base = "https://github.com/$Repo/releases/download/$Version"
} else {
    $Base = "https://github.com/$Repo/releases/download/v$Version"
}
$Archive = "$Bin-$Target.zip"

$Tmp = Join-Path ([IO.Path]::GetTempPath()) ("agent-sidekick-" + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $Tmp | Out-Null
try {
    Write-Host "Downloading $Archive ($Version) from $Repo ..."
    $ArchivePath = Join-Path $Tmp $Archive
    $SumsPath = Join-Path $Tmp 'SHA256SUMS'
    Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Archive" -OutFile $ArchivePath
    Invoke-WebRequest -UseBasicParsing -Uri "$Base/SHA256SUMS" -OutFile $SumsPath

    # Lines look like "<hash>  <file>" (or "<hash> *<file>").
    $Expected = $null
    foreach ($line in Get-Content $SumsPath) {
        $parts = $line.Trim() -split '\s+', 2
        if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $Archive) { $Expected = $parts[0] }
    }
    if (-not $Expected) { throw "$Archive is not listed in SHA256SUMS" }
    $Actual = (Get-FileHash -Algorithm SHA256 -Path $ArchivePath).Hash
    if ($Actual -ne $Expected) {  # -ne is case-insensitive
        throw "checksum mismatch for $Archive (expected $Expected, got $Actual)"
    }
    Write-Host 'Checksum OK.'

    $Extract = Join-Path $Tmp 'extract'
    Expand-Archive -Path $ArchivePath -DestinationPath $Extract
    $Exe = Get-ChildItem -Path $Extract -Recurse -Filter "$Bin.exe" | Select-Object -First 1
    if (-not $Exe) { throw "archive does not contain $Bin.exe" }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Copy-Item -Force -Path $Exe.FullName -Destination (Join-Path $InstallDir "$Bin.exe")
    Write-Host "Installed $(Join-Path $InstallDir "$Bin.exe")"
} finally {
    Remove-Item -Recurse -Force -Path $Tmp -ErrorAction SilentlyContinue
}

# Add the install dir to the user PATH if it is missing.
# Read and write the raw registry value: [Environment]::Get/SetEnvironmentVariable would expand
# %VAR% references and store the result as REG_SZ, permanently losing them.
$EnvKey = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
try {
    $RawPath = [string]$EnvKey.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
    $Entries = @($RawPath -split ';' | Where-Object { $_ })
    $Wanted = [Environment]::ExpandEnvironmentVariables($InstallDir).TrimEnd('\')
    $Present = $Entries | Where-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $Wanted }
    if (-not $Present) {
        $NewPath = (@($Entries) + $InstallDir) -join ';'
        $EnvKey.SetValue('Path', $NewPath, [Microsoft.Win32.RegistryValueKind]::ExpandString)
        # Broadcast WM_SETTINGCHANGE so new processes started from Explorer see the new PATH:
        # SetEnvironmentVariable on a user variable does that as a side effect.
        $Dummy = 'AGENT_SIDEKICK_INSTALL_' + [Guid]::NewGuid().ToString('N')
        [Environment]::SetEnvironmentVariable($Dummy, '1', 'User')
        [Environment]::SetEnvironmentVariable($Dummy, $null, 'User')
        $env:Path = "$env:Path;$InstallDir"
        Write-Host "Added $InstallDir to your user PATH (open a new terminal to pick it up)."
    }
} finally {
    $EnvKey.Close()
}

Write-Host @"

Next steps:
  1. Create an OAuth token for the agent account (log in with that account):
       `$env:CLAUDE_CONFIG_DIR = "`$HOME\.claude-sidekick"; claude setup-token; Remove-Item Env:CLAUDE_CONFIG_DIR
  2. Store the token and register the MCP server with Claude Code:
       $Bin setup
  3. Reload VS Code (or restart Claude Code) and check /mcp.
  Problems? Run: $Bin doctor
"@
