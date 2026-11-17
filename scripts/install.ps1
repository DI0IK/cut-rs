$ErrorActionPreference = "Stop"

$Repo = "DI0IK/cut-rs"
$Target = "x86_64-pc-windows-msvc"
$Archive = "cut-rs-$Target.zip"
$BaseUrl = "https://github.com/$Repo/releases/latest/download"
$InstallDir = if ($env:CUT_RS_INSTALL_DIR) { $env:CUT_RS_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "cut-rs\bin" }

$TmpDir = Join-Path ([System.IO.Path]::GetTempPath()) ("cut-rs-" + [System.Guid]::NewGuid().ToString("N"))
New-Item -ItemType Directory -Force -Path $TmpDir | Out-Null

try {
    $ArchivePath = Join-Path $TmpDir $Archive
    Write-Host "Downloading $Archive..."
    Invoke-WebRequest -Uri "$BaseUrl/$Archive" -OutFile $ArchivePath

    try {
        $SumsPath = Join-Path $TmpDir "SHA256SUMS.txt"
        Invoke-WebRequest -Uri "$BaseUrl/SHA256SUMS.txt" -OutFile $SumsPath
        $Line = Get-Content $SumsPath | Where-Object { $_ -match "\s$([regex]::Escape($Archive))$" } | Select-Object -First 1
        if ($Line) {
            $Expected = ($Line -split "\s+")[0].ToUpperInvariant()
            $Actual = (Get-FileHash -Algorithm SHA256 $ArchivePath).Hash.ToUpperInvariant()
            if ($Expected -ne $Actual) {
                throw "Checksum verification failed for $Archive!"
            }
        }
    } catch {
        if ($_.Exception.Message -like "Checksum verification failed*") { throw }
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    Expand-Archive -Path $ArchivePath -DestinationPath $TmpDir -Force
    Copy-Item (Join-Path $TmpDir "cut-rs.exe") (Join-Path $InstallDir "cut-rs.exe") -Force

    $UserPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($UserPath -notlike "*$InstallDir*") {
        $NewPath = if ([string]::IsNullOrEmpty($UserPath)) { $InstallDir } else { "$UserPath;$InstallDir" }
        [Environment]::SetEnvironmentVariable("Path", $NewPath, "User")
        $env:Path = "$env:Path;$InstallDir"
        Write-Host "Added $InstallDir to your User PATH (restart your terminal if needed)."
    }

    Write-Host "Installed cut-rs to $(Join-Path $InstallDir 'cut-rs.exe')"
    Write-Host ""
    Write-Host "Next step: run 'cut-rs init' to auto-configure your AI coding tools."
} finally {
    Remove-Item -Recurse -Force $TmpDir -ErrorAction SilentlyContinue
}
