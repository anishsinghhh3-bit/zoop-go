[CmdletBinding()]
param(
    [string] $WorkDir = (Join-Path $PSScriptRoot '..\.ffmpeg-build'),
    [ValidateRange(1, 32)][int] $Jobs = 2
)

$ErrorActionPreference = 'Stop'
$project = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$gitExe = (Get-Command git.exe).Source
$gitBash = (Resolve-Path (Join-Path (Split-Path $gitExe) '..\bin\bash.exe')).Path
$kitVersion = '2.11.0'
$x264Commit = '31e19f92f00c7003fa115047ce50978bc98c3a0d'

function Assert-ArchiveHash($File, $Hash) {
    if ((Get-FileHash -LiteralPath $File -Algorithm SHA256).Hash -ne $Hash) {
        throw "Checksum mismatch for $File. Replace it with the specified upstream archive."
    }
}

function Copy-VerifiedArchive($Asset, $CacheRoot, $StageRoot) {
    $cached = Join-Path $CacheRoot $Asset.Name
    $staged = Join-Path $StageRoot $Asset.Name
    if (Test-Path -LiteralPath $cached -PathType Leaf) {
        Copy-Item -LiteralPath $cached -Destination $staged
        Assert-ArchiveHash $staged $Asset.Sha
        return $staged
    }

    & curl.exe --fail --location --silent --show-error --retry 2 --proto '=https' --proto-redir '=https' --output $staged $Asset.Uri
    if ($LASTEXITCODE) { throw "Download failed for $($Asset.Name)." }
    Assert-ArchiveHash $staged $Asset.Sha

    $pending = Join-Path $CacheRoot ('.download-' + [Guid]::NewGuid().ToString('N'))
    try {
        [IO.File]::Copy($staged, $pending)
        try {
            [IO.File]::Move($pending, $cached)
        } catch [IO.IOException] {
            # Another build may have published the cache entry while we were downloading.
            if (!(Test-Path -LiteralPath $cached -PathType Leaf)) { throw }
            Assert-ArchiveHash $cached $Asset.Sha
        }
    } finally {
        if (Test-Path -LiteralPath $pending -PathType Leaf) {
            Remove-Item -LiteralPath $pending -Force
        }
    }
    return $staged
}

function Remove-BuildStage($StageRoot, $CacheRoot) {
    $stage = Get-Item -LiteralPath $StageRoot -Force
    if (!$stage.PSIsContainer -or ($stage.Attributes -band [IO.FileAttributes]::ReparsePoint) -or
        $stage.Parent.FullName -ne $CacheRoot -or $stage.Name -notmatch '^build-[a-f0-9]{32}$') {
        throw "Refusing to remove an unexpected build directory: $StageRoot"
    }
    Remove-Item -LiteralPath $stage.FullName -Recurse -Force
}

New-Item -ItemType Directory -Path $WorkDir -Force | Out-Null
$cacheRoot = (Get-Item -LiteralPath $WorkDir -Force).FullName
$buildStage = Join-Path $cacheRoot ('build-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $buildStage | Out-Null
$assets = @(
    @{ Name = 'ffmpeg-9.0.2.tar.xz'; Uri = 'https://ffmpeg.org/releases/ffmpeg-9.0.2.tar.xz'; Sha = '8C3850283EB25FA026482078A04051E0BE17347B09EF81A0849BEC15A96E002E' },
    @{ Name = 'x264-stable.tar.gz'; Uri = "https://codeload.github.com/mirror/x264/tar.gz/$x264Commit"; Sha = 'D053C9D86988D6BC78237CA5205865C5DDF99C98EF4CD9927EEC8F6D388F6DD9' },
    @{ Name = 'nasm.zip'; Uri = 'https://www.nasm.us/pub/nasm/releasebuilds/3.02/win64/nasm-3.02-win64.zip'; Sha = '161D0BFAFF53C2F9E9F3E69FD0672323EBABAFD1268976A5CEC11BE92A19AEE7' },
    @{ Name = 'w64devkit.7z.exe'; Uri = "https://github.com/skeeto/w64devkit/releases/download/v$kitVersion/w64devkit-x64-$kitVersion.7z.exe"; Sha = '54C2BB9962E8A30C8270E34C5EFC3D5FEE579F53087FA7D435B95FBBA2AF7B2B' }
)

$previousJobs = $env:ZOOP_FFMPEG_JOBS
$volume = $null
$locationPushed = $false
$success = $false
try {
    $archives = @{}
    foreach ($asset in $assets) {
        $archives[$asset.Name] = Copy-VerifiedArchive $asset $cacheRoot $buildStage
    }

    foreach ($source in @('ffmpeg-9.0.2.tar.xz', 'x264-stable.tar.gz')) {
        & tar.exe -xf $archives[$source] -C $buildStage
        if ($LASTEXITCODE) { throw "Could not extract $source." }
    }
    Expand-Archive -LiteralPath $archives['nasm.zip'] -DestinationPath (Join-Path $buildStage 'nasm')

    # Extract the verified 7z payload without launching the self-extracting archive.
    $kit = [IO.File]::ReadAllBytes($archives['w64devkit.7z.exe'])
    $offset = 353792
    if ([Convert]::ToBase64String($kit, $offset, 6) -ne 'N3q8rycc') {
        throw 'The w64devkit archive header is missing.'
    }
    $kitPayload = Join-Path $buildStage 'w64devkit.7z'
    $stream = [IO.File]::Create($kitPayload)
    try { $stream.Write($kit, $offset, $kit.Length - $offset) } finally { $stream.Dispose() }
    & tar.exe -xf $kitPayload -C $buildStage
    if ($LASTEXITCODE) { throw 'Could not extract w64devkit.' }

    $drive = @('Z', 'Y', 'X', 'W', 'V') | Where-Object { !(Test-Path ("${_}:\")) -and !(Get-PSDrive -Name $_ -ErrorAction SilentlyContinue) } | Select-Object -First 1
    if (!$drive) { throw 'No free drive letter is available for the temporary compiler workspace.' }
    & subst.exe "${drive}:" $buildStage
    if ($LASTEXITCODE) { throw 'Could not map the temporary compiler workspace.' }
    $volume = "${drive}:"

    Push-Location "$volume\"
    $locationPushed = $true
    $env:ZOOP_FFMPEG_JOBS = $Jobs
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'build-ffmpeg.sh') -Destination "$volume\build-ffmpeg.sh"
    & $gitBash ./build-ffmpeg.sh
    if ($LASTEXITCODE) { throw "FFmpeg build failed with exit code $LASTEXITCODE." }
    $sidecar = Join-Path $project 'src-tauri\binaries\ffmpeg-x86_64-pc-windows-msvc.exe'
    New-Item -ItemType Directory -Path (Split-Path $sidecar) -Force | Out-Null
    $pendingSidecar = "$sidecar.$([Guid]::NewGuid().ToString('N')).tmp"
    try {
        Copy-Item -LiteralPath "$volume\ffmpeg-fast\ffmpeg.exe" -Destination $pendingSidecar
        if (Test-Path -LiteralPath $sidecar -PathType Leaf) {
            [IO.File]::Replace($pendingSidecar, $sidecar, [NullString]::Value)
        } else {
            [IO.File]::Move($pendingSidecar, $sidecar)
        }
    } finally {
        if (Test-Path -LiteralPath $pendingSidecar -PathType Leaf) {
            Remove-Item -LiteralPath $pendingSidecar -Force
        }
    }
    $success = $true
    Get-Item -LiteralPath $sidecar
} finally {
    $env:ZOOP_FFMPEG_JOBS = $previousJobs
    if ($locationPushed) { Pop-Location }
    if ($volume) {
        & subst.exe $volume /d
        if ($LASTEXITCODE) {
            $success = $false
            Write-Warning "Could not remove the temporary build drive $volume."
        }
    }
    if ($success) {
        try { Remove-BuildStage $buildStage $cacheRoot }
        catch { Write-Warning "Could not clean up $buildStage. $_" }
    } else {
        Write-Warning "The build workspace is available at $buildStage."
    }
}
