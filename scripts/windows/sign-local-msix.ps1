[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$PackagePath,

    [string]$OutputPath,
    [string]$Publisher = "CN=Lumen ASR Development",
    [string]$CertificateFriendlyName = "Lumen ASR Local Development Signing",
    [string]$Version,

    [ValidateRange(1, 5)]
    [int]$CertificateValidityYears = 2,

    [switch]$Force
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

function Get-NextMsixVersion {
    param(
        [Parameter(Mandatory = $true)]
        [version]$Baseline
    )

    $parts = @($Baseline.Major, $Baseline.Minor, $Baseline.Build, $Baseline.Revision)
    for ($index = 3; $index -ge 0; $index--) {
        if ($parts[$index] -lt 65535) {
            $parts[$index]++
            for ($resetIndex = $index + 1; $resetIndex -lt 4; $resetIndex++) {
                $parts[$resetIndex] = 0
            }
            return $parts -join "."
        }
    }

    throw "The local MSIX version has reached 65535.65535.65535.65535."
}

$resolvedPackage = (Resolve-Path -LiteralPath $PackagePath).Path
if ([System.IO.Path]::GetExtension($resolvedPackage) -ne ".msix") {
    throw "PackagePath must point to an MSIX package: $resolvedPackage"
}

$localData = [Environment]::GetFolderPath("LocalApplicationData")
$developmentRoot = Join-Path $localData "Lumen ASR\Development"
if ([string]::IsNullOrWhiteSpace($OutputPath)) {
    $baseName = [System.IO.Path]::GetFileNameWithoutExtension($resolvedPackage)
    $OutputPath = Join-Path $developmentRoot "Packages\$baseName-local-signed.msix"
}
$resolvedOutput = [System.IO.Path]::GetFullPath($OutputPath)
if ($resolvedOutput -eq $resolvedPackage) {
    throw "OutputPath must not overwrite the input package."
}
if ((Test-Path -LiteralPath $resolvedOutput) -and -not $Force) {
    throw "Output package already exists. Pass -Force to replace it: $resolvedOutput"
}

$outputDirectory = Split-Path -Parent $resolvedOutput
New-Item -ItemType Directory -Path $outputDirectory -Force | Out-Null

$makeAppx = Resolve-WindowsSdkTool -Name "makeappx.exe"
$signTool = Resolve-WindowsSdkTool -Name "signtool.exe"
$certificate = Get-ChildItem -Path "Cert:\CurrentUser\My" |
    Where-Object {
        $enhancedKeyUsages = @(
            $_.EnhancedKeyUsageList |
                ForEach-Object { [string]$_.ObjectId }
        )
        $_.FriendlyName -eq $CertificateFriendlyName -and
        $_.Subject -eq $Publisher -and
        $_.HasPrivateKey -and
        $_.NotAfter -gt (Get-Date).AddDays(30) -and
        ($enhancedKeyUsages -contains "1.3.6.1.5.5.7.3.3")
    } |
    Sort-Object NotAfter -Descending |
    Select-Object -First 1

if ($null -eq $certificate) {
    $certificate = New-SelfSignedCertificate `
        -Type Custom `
        -Subject $Publisher `
        -KeyAlgorithm RSA `
        -KeyLength 3072 `
        -HashAlgorithm SHA256 `
        -KeyUsage DigitalSignature `
        -KeyExportPolicy NonExportable `
        -FriendlyName $CertificateFriendlyName `
        -CertStoreLocation "Cert:\CurrentUser\My" `
        -TextExtension @(
            "2.5.29.37={text}1.3.6.1.5.5.7.3.3",
            "2.5.29.19={text}"
        ) `
        -NotAfter (Get-Date).AddYears($CertificateValidityYears)
}

$certificateDirectory = Join-Path $developmentRoot "Certificates"
New-Item -ItemType Directory -Path $certificateDirectory -Force | Out-Null
$certificatePath = Join-Path $certificateDirectory "lumen-asr-local-dev-$($certificate.Thumbprint).cer"
Export-Certificate -Cert $certificate -FilePath $certificatePath -Force | Out-Null

$stagingRoot = Join-Path ([System.IO.Path]::GetTempPath()) "lumen-asr-local-sign-$([guid]::NewGuid().ToString('N'))"

try {
    & $makeAppx unpack /p $resolvedPackage /d $stagingRoot | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "MakeAppx unpack failed with exit code $LASTEXITCODE."
    }

    $manifestPath = Join-Path $stagingRoot "AppxManifest.xml"
    $manifest = [System.Xml.XmlDocument]::new()
    $manifest.PreserveWhitespace = $true
    $manifest.Load($manifestPath)
    $namespace = [System.Xml.XmlNamespaceManager]::new($manifest.NameTable)
    $namespace.AddNamespace("appx", "http://schemas.microsoft.com/appx/manifest/foundation/windows10")
    $identity = $manifest.SelectSingleNode("/appx:Package/appx:Identity", $namespace)
    if ($null -eq $identity) {
        throw "The package manifest does not contain an Identity element."
    }

    $originalPublisher = $identity.GetAttribute("Publisher")
    if ($originalPublisher -ne $Publisher) {
        if ($originalPublisher -notmatch "(?:^|,\s*)OID\.2\.25\.") {
            throw "Refusing to replace non-development publisher '$originalPublisher'."
        }
        $identity.SetAttribute("Publisher", $Publisher)
    }

    $originalVersion = [version]$identity.GetAttribute("Version")
    $identityName = $identity.GetAttribute("Name")
    $versionStatePath = Join-Path $developmentRoot "last-version.txt"
    if ([string]::IsNullOrWhiteSpace($Version)) {
        $baselineVersion = $originalVersion
        $installedVersion = Get-AppxPackage -Name $identityName |
            Where-Object { $_.Publisher -eq $Publisher } |
            ForEach-Object { [version]$_.Version } |
            Sort-Object -Descending |
            Select-Object -First 1
        if ($null -ne $installedVersion -and $installedVersion -gt $baselineVersion) {
            $baselineVersion = $installedVersion
        }

        if (Test-Path -LiteralPath $versionStatePath) {
            $stateText = (Get-Content -LiteralPath $versionStatePath -Raw).Trim()
            $stateVersion = $null
            if (-not [version]::TryParse($stateText, [ref]$stateVersion)) {
                throw "The local version state is invalid: $versionStatePath"
            }
            if ($stateVersion -gt $baselineVersion) {
                $baselineVersion = $stateVersion
            }
        }

        $Version = Get-NextMsixVersion -Baseline $baselineVersion
    }
    $parsedVersion = $null
    if (-not [version]::TryParse($Version, [ref]$parsedVersion)) {
        throw "Version must have four numeric components: $Version"
    }
    $versionParts = @($Version.Split("."))
    if ($versionParts.Count -ne 4 -or @($versionParts | Where-Object { [int]$_ -lt 0 -or [int]$_ -gt 65535 }).Count -gt 0) {
        throw "Each MSIX version component must be between 0 and 65535: $Version"
    }
    if ($parsedVersion -le $originalVersion) {
        throw "Local signing version must be newer than package version $originalVersion."
    }
    $identity.SetAttribute("Version", $Version)

    $writerSettings = [System.Xml.XmlWriterSettings]::new()
    $writerSettings.Encoding = [System.Text.UTF8Encoding]::new($false)
    $writerSettings.Indent = $false
    $writer = [System.Xml.XmlWriter]::Create($manifestPath, $writerSettings)
    try {
        $manifest.Save($writer)
    }
    finally {
        $writer.Dispose()
    }

    foreach ($footprint in @("AppxBlockMap.xml", "AppxSignature.p7x", "[Content_Types].xml")) {
        $footprintPath = Join-Path $stagingRoot $footprint
        if (Test-Path -LiteralPath $footprintPath) {
            Remove-Item -LiteralPath $footprintPath -Force
        }
    }
    $codeIntegrityCatalog = Join-Path $stagingRoot "AppxMetadata\CodeIntegrity.cat"
    if (Test-Path -LiteralPath $codeIntegrityCatalog) {
        Remove-Item -LiteralPath $codeIntegrityCatalog -Force
    }

    # Smart App Control evaluates the packaged executable as well as the MSIX
    # envelope. Sign every executable payload before MakeAppx hashes it into
    # the package; signing only the final MSIX can still yield error 4551.
    $payloadExecutables = @(Get-ChildItem -LiteralPath $stagingRoot -Recurse -Filter "*.exe" -File)
    if ($payloadExecutables.Count -eq 0) {
        throw "The package does not contain an executable payload."
    }
    foreach ($payload in $payloadExecutables) {
        & $signTool sign /fd SHA256 /sha1 $certificate.Thumbprint $payload.FullName | Out-Host
        if ($LASTEXITCODE -ne 0) {
            throw "SignTool failed for executable payload '$($payload.FullName)' with exit code $LASTEXITCODE."
        }
        $payloadSignature = Get-AuthenticodeSignature -LiteralPath $payload.FullName
        if ($null -eq $payloadSignature.SignerCertificate -or
            $payloadSignature.SignerCertificate.Thumbprint -ne $certificate.Thumbprint) {
            throw "Executable payload '$($payload.FullName)' does not contain the expected signature."
        }
    }

    & $makeAppx pack /o /d $stagingRoot /p $resolvedOutput | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "MakeAppx pack failed with exit code $LASTEXITCODE."
    }

    & $signTool sign /fd SHA256 /sha1 $certificate.Thumbprint $resolvedOutput | Out-Host
    if ($LASTEXITCODE -ne 0) {
        throw "SignTool failed with exit code $LASTEXITCODE."
    }

    $signature = Get-AuthenticodeSignature -LiteralPath $resolvedOutput
    if ($null -eq $signature.SignerCertificate -or $signature.SignerCertificate.Thumbprint -ne $certificate.Thumbprint) {
        throw "The output package does not contain the expected signature."
    }

    [System.IO.File]::WriteAllText(
        $versionStatePath,
        $Version,
        [System.Text.UTF8Encoding]::new($false)
    )

    [pscustomobject]@{
        Package = $resolvedOutput
        Publisher = $Publisher
        OriginalVersion = $originalVersion
        Version = $Version
        VersionState = $versionStatePath
        Certificate = $certificatePath
        Thumbprint = $certificate.Thumbprint
        CertificateExpires = $certificate.NotAfter
        SignatureStatus = $signature.Status
        Sha256 = (Get-FileHash -LiteralPath $resolvedOutput -Algorithm SHA256).Hash
    } | Format-List
}
finally {
    if (Test-Path -LiteralPath $stagingRoot) {
        Remove-Item -LiteralPath $stagingRoot -Recurse -Force
    }
}
