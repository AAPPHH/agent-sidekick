# Installer for agent-sidekick (Windows x86_64; ARM64 runs the x86_64 build via emulation).
#
#   irm https://github.com/OWNER/agent-sidekick/releases/latest/download/install.ps1 | iex
#
# The copy attached to a GitHub release has the repository filled in by the release workflow
# (from `repository` in Cargo.toml). The copy in the source tree carries a placeholder and is
# meant for testing with AGENT_SIDEKICK_INSTALL_BASE_URL.
#
# Needs only Windows PowerShell 5.1 (preinstalled). No Rust, Git or Node.
#
# Environment:
#   VERSION                          release tag to install, e.g. v0.3.0 (default: latest)
#   AGENT_SIDEKICK_INSTALL_DIR       target directory (default: %LOCALAPPDATA%\Programs\agent-sidekick)
#   AGENT_SIDEKICK_INSTALL_BASE_URL  download from this URL or local directory instead of the
#                                    GitHub release (it must contain the archive and SHA256SUMS)
#   AGENT_SIDEKICK_REPO              GitHub repository "owner/name" (default: set at release time)
#   AGENT_SIDEKICK_TARGET            override the target triple (testing)
#   AGENT_SIDEKICK_NO_SETUP=1        do not run `agent-sidekick setup` at the end
#   AGENT_SIDEKICK_NO_MODIFY_PATH=1  leave the user PATH and the session PATH untouched
#
# The body runs in its own script block: with `irm | iex` the script would otherwise run in the
# caller's scope (preferences and variables would leak into their session). Errors are thrown,
# never `exit`, so a failure does not close the user's window.

[Diagnostics.CodeAnalysis.SuppressMessageAttribute('PSAvoidUsingWriteHost', '',
    Justification = 'Interactive installer: messages are meant for the person at the console, not for the pipeline.')]
param()

& {
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest is very slow with the progress bar in 5.1
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $Bin = 'agent-sidekick'
    $Repo = if ($env:AGENT_SIDEKICK_REPO) { $env:AGENT_SIDEKICK_REPO } else { 'OWNER/agent-sidekick' }
    $Version = if ($env:VERSION) { $env:VERSION } else { 'latest' }
    $InstallDir = if ($env:AGENT_SIDEKICK_INSTALL_DIR) {
        $env:AGENT_SIDEKICK_INSTALL_DIR
    } else {
        Join-Path $env:LOCALAPPDATA 'Programs\agent-sidekick'
    }

    if ($env:AGENT_SIDEKICK_INSTALL_BASE_URL) {
        $Base = $env:AGENT_SIDEKICK_INSTALL_BASE_URL.TrimEnd('/', '\')
    } else {
        if ($Repo.StartsWith('OWNER/')) {
            throw 'This copy of install.ps1 has no repository filled in (only release assets have). Set $env:AGENT_SIDEKICK_REPO = "owner/name" or $env:AGENT_SIDEKICK_INSTALL_BASE_URL = "<url or directory>" and run it again.'
        }
        if ($Version -eq 'latest') {
            $Base = "https://github.com/$Repo/releases/latest/download"
        } elseif ($Version.StartsWith('v')) {
            $Base = "https://github.com/$Repo/releases/download/$Version"
        } else {
            $Base = "https://github.com/$Repo/releases/download/v$Version"
        }
    }
    $LocalBase = Test-Path -LiteralPath $Base -PathType Container

    if ($env:AGENT_SIDEKICK_TARGET) {
        $Target = $env:AGENT_SIDEKICK_TARGET
    } else {
        $Arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
        switch ($Arch) {
            'AMD64' { }
            'ARM64' { Write-Host 'ARM64 Windows detected: installing the x86_64 build (runs via emulation).' }
            default { throw "Unsupported architecture: $Arch. Only x86_64 Windows builds are published; build from source with: cargo install --locked --git https://github.com/$Repo" }
        }
        $Target = 'x86_64-pc-windows-msvc'
    }
    $Archive = "$Bin-$Target.zip"

    # Copy from the local base directory or download from the base URL.
    function Get-Asset([string]$Name, [string]$OutFile) {
        if ($LocalBase) {
            $Source = Join-Path $Base $Name
            if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) { throw "$Name not found in $Base" }
            Copy-Item -LiteralPath $Source -Destination $OutFile
        } else {
            try {
                Invoke-WebRequest -UseBasicParsing -Uri "$Base/$Name" -OutFile $OutFile
            } catch {
                throw "Download failed: $Base/$Name ($($_.Exception.Message)). Check `$env:VERSION and your network; releases: https://github.com/$Repo/releases"
            }
        }
    }

    $Tmp = Join-Path ([IO.Path]::GetTempPath()) ('agent-sidekick-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $Tmp | Out-Null
    try {
        Write-Host "Fetching $Archive from $Base ..."
        $ArchivePath = Join-Path $Tmp $Archive
        $SumsPath = Join-Path $Tmp 'SHA256SUMS'
        Get-Asset $Archive $ArchivePath
        Get-Asset 'SHA256SUMS' $SumsPath

        # Lines look like "<hash>  <file>" (or "<hash> *<file>").
        $Expected = $null
        foreach ($Line in [IO.File]::ReadAllLines($SumsPath)) {
            $Parts = $Line.Trim() -split '\s+', 2
            if ($Parts.Count -eq 2 -and $Parts[1].TrimStart('*') -eq $Archive) { $Expected = $Parts[0]; break }
        }
        if (-not $Expected) { throw "$Archive is not listed in SHA256SUMS; this release has no build for $Target." }
        $Actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $ArchivePath).Hash
        if ($Actual -ne $Expected) {  # -ne is case-insensitive
            throw "Checksum mismatch for $Archive (expected $Expected, got $Actual). The download is corrupt or was tampered with; nothing was installed."
        }
        Write-Host 'Checksum OK.'

        $Extract = Join-Path $Tmp 'extract'
        Expand-Archive -LiteralPath $ArchivePath -DestinationPath $Extract
        $NewExe = Get-ChildItem -LiteralPath $Extract -Recurse -Filter "$Bin.exe" | Select-Object -First 1
        if (-not $NewExe) { throw "$Archive does not contain $Bin.exe" }

        try {
            New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
        } catch {
            throw "Cannot create $InstallDir ($($_.Exception.Message)). Set `$env:AGENT_SIDEKICK_INSTALL_DIR to a writable directory."
        }
        $Exe = Join-Path $InstallDir "$Bin.exe"
        $Old = "$Exe.old"
        if (Test-Path -LiteralPath $Old) { Remove-Item -LiteralPath $Old -Force -ErrorAction SilentlyContinue }
        $WasLocked = $false
        try {
            Copy-Item -LiteralPath $NewExe.FullName -Destination $Exe -Force
        } catch {
            # A running MCP server keeps the exe open. Windows cannot overwrite it, but it can rename it.
            if (Test-Path -LiteralPath $Old) {
                throw "$Exe is in use and the previous $Old is still locked. Close VS Code / Claude Code (or reload the window) and run the installer again."
            }
            try {
                Rename-Item -LiteralPath $Exe -NewName "$Bin.exe.old"
            } catch {
                throw "$Exe is in use and cannot be replaced ($($_.Exception.Message)). Close VS Code / Claude Code and run the installer again."
            }
            Copy-Item -LiteralPath $NewExe.FullName -Destination $Exe -Force
            $WasLocked = $true
        }
    } finally {
        Remove-Item -Recurse -Force -LiteralPath $Tmp -ErrorAction SilentlyContinue
    }

    $Installed = & $Exe --version
    if ($LASTEXITCODE -ne 0) { throw "Installed $Exe but it does not run on this machine ($Target)." }
    Write-Host "Installed $Installed to $Exe"
    if ($WasLocked) {
        Write-Host "The previous version was still running and was moved to $Old. Reload VS Code (Developer: Reload Window) or restart Claude Code to use the new version."
    }

    if ($env:AGENT_SIDEKICK_NO_MODIFY_PATH -eq '1') {
        Write-Host "PATH left unchanged (AGENT_SIDEKICK_NO_MODIFY_PATH=1). Add $InstallDir to your PATH to run $Bin by name."
    } else {
        # Persistent user PATH (HKCU\Environment), without duplicates. The raw registry value is read
        # and written so %VAR% references in it survive: [Environment]::Get/SetEnvironmentVariable
        # would expand them and store the result as REG_SZ.
        $Wanted = [Environment]::ExpandEnvironmentVariables($InstallDir).TrimEnd('\')
        $EnvKey = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
        try {
            $RawPath = [string]$EnvKey.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $Entries = @($RawPath -split ';' | Where-Object { $_ })
            $Present = @($Entries | Where-Object { [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\') -ieq $Wanted })
            if ($Present.Count -eq 0) {
                $EnvKey.SetValue('Path', (@($Entries) + $InstallDir) -join ';', [Microsoft.Win32.RegistryValueKind]::ExpandString)
                # Broadcast WM_SETTINGCHANGE so programs started from Explorer see the new PATH:
                # SetEnvironmentVariable on a user variable does that as a side effect.
                $Dummy = 'AGENT_SIDEKICK_INSTALL_' + [Guid]::NewGuid().ToString('N')
                [Environment]::SetEnvironmentVariable($Dummy, '1', 'User')
                [Environment]::SetEnvironmentVariable($Dummy, $null, 'User')
                Write-Host "Added $InstallDir to your user PATH (new terminals pick it up)."
            }
        } finally {
            $EnvKey.Close()
        }
        # Current session.
        $SessionEntries = @($env:Path -split ';' | Where-Object { $_ } | ForEach-Object { $_.TrimEnd('\') })
        if ($SessionEntries -inotcontains $Wanted) { $env:Path = "$env:Path;$InstallDir" }
    }

    $Interactive = [Environment]::UserInteractive -and -not [Console]::IsInputRedirected
    if ($env:AGENT_SIDEKICK_NO_SETUP -ne '1' -and $Interactive) {
        Write-Host ''
        Write-Host "Running '$Bin setup' (stores the token and registers the MCP server with Claude Code) ..."
        & $Exe setup
        if ($LASTEXITCODE -ne 0) { Write-Host "Setup did not finish. Run it again later with: $Bin setup" }
    } else {
        Write-Host ''
        Write-Host "Next step: run '$Bin setup' in a terminal (stores the token and registers the MCP server"
        Write-Host "with Claude Code), then reload VS Code. Problems? Run: $Bin doctor"
    }
}
