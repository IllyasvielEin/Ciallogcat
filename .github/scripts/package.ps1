$ErrorActionPreference = 'Stop'
if (-not $env:RELEASE_TAG -or $env:RELEASE_PLATFORM -notin @('linux-x86_64', 'windows-x86_64')) {
    throw 'RELEASE_TAG and a supported RELEASE_PLATFORM are required'
}
if ($env:RELEASE_TAG -cne 'v0.1' -and $env:RELEASE_TAG -notmatch '^v[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
    throw 'Invalid version tag'
}
$bundleName = "ciallogcat-$env:RELEASE_TAG-$env:RELEASE_PLATFORM"
$tempRoot = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { 'target/release-stage' }
$stageRoot = Join-Path $tempRoot ([Guid]::NewGuid().ToString('N'))
$stage = Join-Path $stageRoot $bundleName
$dist = 'target/release-dist'
if (Test-Path -LiteralPath $stage) { throw "Package staging directory already exists: $stage" }
New-Item -ItemType Directory -Path "$stage/assets/fonts", $dist -Force | Out-Null
$executable = if ($env:RELEASE_PLATFORM -eq 'windows-x86_64') { 'ciallogcat.exe' } else { 'ciallogcat' }
Copy-Item -LiteralPath "target/release/$executable" -Destination $stage
Copy-Item -LiteralPath 'README.md', 'LICENSE' -Destination $stage
# Bundle the usage and maintenance guides with the executable.
Copy-Item -LiteralPath 'CONTRIBUTING.md' -Destination $stage
Copy-Item -LiteralPath 'docs' -Destination $stage -Recurse
Copy-Item -LiteralPath 'assets/fonts/README.md', 'assets/fonts/LICENSE-OFL.txt' -Destination "$stage/assets/fonts"
@(
    "Tag: $env:RELEASE_TAG"
    "Platform: $env:RELEASE_PLATFORM"
    "Runner: $env:ImageOS $env:ImageVersion"
    "Commit: $env:GITHUB_SHA"
    (rustc --version)
) | Set-Content -LiteralPath "$stage/BUILD-INFO.txt" -Encoding utf8
if ($LASTEXITCODE -ne 0) { throw 'rustc version check failed' }
if ($env:RELEASE_PLATFORM -eq 'linux-x86_64') {
    $dependencies = ldd "target/release/$executable" 2>&1
    if ($LASTEXITCODE -ne 0 -or ($dependencies -match 'not found')) { throw 'Unresolved Linux dynamic dependencies' }
    $dependencies | Set-Content -LiteralPath "$stage/BUILD-INFO.txt" -Encoding utf8 -Append
    $archiveName = "$bundleName.tar.gz"
    tar -czf "$dist/$archiveName" -C $stageRoot $bundleName
    if ($LASTEXITCODE -ne 0) { throw 'tar failed' }
} else {
    $archiveName = "$bundleName.zip"
    Compress-Archive -LiteralPath $stage -DestinationPath "$dist/$archiveName" -Force
}
$stream = [IO.File]::OpenRead((Join-Path (Get-Location) "$dist/$archiveName"))
$sha = [Security.Cryptography.SHA256]::Create()
try {
    $digest = ([BitConverter]::ToString($sha.ComputeHash($stream))).Replace('-', '').ToLowerInvariant()
} finally {
    $stream.Dispose()
    $sha.Dispose()
}
[IO.File]::WriteAllText((Join-Path (Get-Location) "$dist/$archiveName.sha256"), "$digest  $archiveName`n")
