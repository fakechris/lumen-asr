param(
    [Parameter(Mandatory = $true)]
    [string]$BundleDirectory,

    [Parameter(Mandatory = $true)]
    [string]$VersionTag,

    [Parameter(Mandatory = $true)]
    [string]$OutputDirectory
)

$ErrorActionPreference = 'Stop'

if ($VersionTag -notmatch '^v\d+\.\d+\.\d+$') {
    throw "VersionTag must use vMAJOR.MINOR.PATCH format: $VersionTag"
}

$bundle = (Resolve-Path -LiteralPath $BundleDirectory).Path
$installers = @(Get-ChildItem -LiteralPath $bundle -File -Filter '*-setup.exe')
if ($installers.Count -ne 1) {
    throw "Expected exactly one NSIS installer in $bundle, found $($installers.Count)"
}

New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$output = Join-Path $OutputDirectory "Lumen-ASR-$VersionTag-windows-x64-setup.exe"
Copy-Item -LiteralPath $installers[0].FullName -Destination $output -Force

if (-not (Test-Path -LiteralPath $output -PathType Leaf)) {
    throw "Windows release asset was not created: $output"
}

Write-Output $output
