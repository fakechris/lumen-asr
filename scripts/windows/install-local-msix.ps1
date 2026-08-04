[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$PackagePath,

    [Parameter(Mandatory = $true)]
    [ValidatePattern("^[A-Fa-f0-9]{40}$")]
    [string]$ExpectedThumbprint,

    [string]$ExpectedPublisher = "CN=Lumen ASR Development",
    [string]$ExpectedIdentityName = "LumenASR.Dev",
    [switch]$Launch
)

Set-StrictMode -Version Latest
$ErrorActionPreference = "Stop"

function Resolve-WindowsSdkTool {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Name
    )

    $kitsRoot = Join-Path ${env:ProgramFiles(x86)} "Windows Kits\10\bin"
    $tool = Get-ChildItem -LiteralPath $kitsRoot -Recurse -Filter $Name -File |
        Where-Object { $_.Directory.Name -eq "x64" } |
        Sort-Object { [version]$_.Directory.Parent.Name } -Descending |
        Select-Object -First 1
    if ($null -eq $tool) {
        throw "$Name was not found. Install the Windows 10/11 SDK."
    }

    return $tool.FullName
}

$principal = [Security.Principal.WindowsPrincipal]::new(
    [Security.Principal.WindowsIdentity]::GetCurrent()
)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw "Run this script from an elevated PowerShell window."
}

$resolvedPackage = (Resolve-Path -LiteralPath $PackagePath).Path
$signature = Get-AuthenticodeSignature -LiteralPath $resolvedPackage
if ($null -eq $signature.SignerCertificate) {
    throw "The MSIX package is not signed. Run sign-local-msix.ps1 first."
}
if ($signature.Status -eq [System.Management.Automation.SignatureStatus]::HashMismatch) {
    throw "The MSIX signature does not match the package contents."
}
if ($signature.SignerCertificate.Subject -ne $ExpectedPublisher) {
    throw "Unexpected signer '$($signature.SignerCertificate.Subject)'. Expected '$ExpectedPublisher'."
}
$normalizedThumbprint = $ExpectedThumbprint.ToUpperInvariant()
if ($signature.SignerCertificate.Thumbprint -ne $normalizedThumbprint) {
    throw "Unexpected certificate thumbprint '$($signature.SignerCertificate.Thumbprint)'."
}

$temporaryCertificate = Join-Path ([System.IO.Path]::GetTempPath()) "lumen-asr-$([guid]::NewGuid().ToString('N')).cer"
$inspectionRoot = Join-Path ([System.IO.Path]::GetTempPath()) "lumen-asr-inspect-$([guid]::NewGuid().ToString('N'))"
try {
    $makeAppx = Resolve-WindowsSdkTool -Name "makeappx.exe"
    & $makeAppx unpack /p $resolvedPackage /d $inspectionRoot | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "MakeAppx unpack failed with exit code $LASTEXITCODE."
    }

    $manifest = [System.Xml.XmlDocument]::new()
    $manifest.Load((Join-Path $inspectionRoot "AppxManifest.xml"))
    $namespace = [System.Xml.XmlNamespaceManager]::new($manifest.NameTable)
    $namespace.AddNamespace("appx", "http://schemas.microsoft.com/appx/manifest/foundation/windows10")
    $identity = $manifest.SelectSingleNode("/appx:Package/appx:Identity", $namespace)
    if ($null -eq $identity) {
        throw "The supplied package manifest does not contain an Identity element."
    }

    $manifestName = $identity.GetAttribute("Name")
    $manifestPublisher = $identity.GetAttribute("Publisher")
    $manifestVersion = [version]$identity.GetAttribute("Version")
    if ($manifestName -ne $ExpectedIdentityName) {
        throw "Unexpected package identity '$manifestName'. Expected '$ExpectedIdentityName'."
    }
    if ($manifestPublisher -ne $ExpectedPublisher) {
        throw "Unexpected manifest publisher '$manifestPublisher'. Expected '$ExpectedPublisher'."
    }

    Export-Certificate -Cert $signature.SignerCertificate -FilePath $temporaryCertificate -Force | Out-Null
    Import-Certificate `
        -FilePath $temporaryCertificate `
        -CertStoreLocation "Cert:\LocalMachine\TrustedPeople" | Out-Null

    $signTool = Resolve-WindowsSdkTool -Name "signtool.exe"
    & $signTool verify /pa $resolvedPackage | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "The package signature is not trusted after certificate installation."
    }

    Add-AppxPackage -Path $resolvedPackage -ForceApplicationShutdown
    $package = Get-AppxPackage -Name $manifestName |
        Where-Object {
            $_.Publisher -eq $manifestPublisher -and
            $_.Version -eq $manifestVersion
        } |
        Select-Object -First 1
    if ($null -eq $package) {
        throw "The exact package identity, publisher, and version were not registered after installation."
    }

    if ($Launch) {
        $manifest = Get-AppxPackageManifest -Package $package
        $applicationId = @($manifest.Package.Applications.Application)[0].Id
        Start-Process -FilePath "explorer.exe" -ArgumentList "shell:AppsFolder\$($package.PackageFamilyName)!$applicationId"
    }

    [pscustomobject]@{
        Name = $package.Name
        Version = $package.Version
        Publisher = $package.Publisher
        PackageFamilyName = $package.PackageFamilyName
        InstallLocation = $package.InstallLocation
        CertificateThumbprint = $signature.SignerCertificate.Thumbprint
        Launched = [bool]$Launch
    } | Format-List
}
finally {
    if (Test-Path -LiteralPath $temporaryCertificate) {
        Remove-Item -LiteralPath $temporaryCertificate -Force
    }
    if (Test-Path -LiteralPath $inspectionRoot) {
        Remove-Item -LiteralPath $inspectionRoot -Recurse -Force
    }
}
