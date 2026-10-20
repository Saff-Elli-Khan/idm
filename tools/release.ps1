# Builds dist\IDM2-<version>-windows-x64.zip from the version in Cargo.toml:
#   idm2.exe, extension\ (Chrome/Edge) and extension-firefox\ (manifest.firefox.json as its manifest.json).
# Refuses to package if the extension manifests or the README download links aren't on that version.
$ErrorActionPreference = "Stop"
Set-Location (Split-Path $PSScriptRoot)
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$zip = "IDM2-$version-windows-x64.zip"

foreach ($m in "extension\manifest.json", "extension\manifest.firefox.json") {
    $v = (Get-Content $m -Raw | ConvertFrom-Json).version
    if ($v -ne $version) { throw "$m is version $v, Cargo.toml is $version" }
}
$readme = Get-Content README.md -Raw
if (([regex]::Matches($readme, [regex]::Escape($zip))).Count -lt 2 -or $readme -notmatch "Download IDM2 $([regex]::Escape($version)) ") {
    throw "README.md must link $zip at the top (""Download IDM2 $version ..."") and in the Installation step"
}
if (Test-Path "dist\$zip") { throw "dist\$zip already exists; released zips are never rebuilt, bump the version instead" }

$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
cargo build --release
if ($LASTEXITCODE) { throw "cargo build failed" }

$stage = "dist\IDM2-$version-windows-x64"
New-Item -ItemType Directory -Force "$stage\extension", "$stage\extension-firefox" | Out-Null
Copy-Item target\release\idm2.exe $stage -Force
foreach ($dir in "extension", "extension-firefox") {
    Copy-Item extension\background.js, extension\icons "$stage\$dir" -Recurse -Force
}
Copy-Item extension\manifest.json "$stage\extension\manifest.json" -Force
Copy-Item extension\manifest.firefox.json "$stage\extension-firefox\manifest.json" -Force
Compress-Archive -Path "$stage\*" -DestinationPath "dist\$zip"
"Packaged dist\$zip ($([math]::Round((Get-Item "dist\$zip").Length / 1MB, 1)) MB)"
