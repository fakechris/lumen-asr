# Windows local signing

This workflow creates a free, self-signed MSIX for development and limited
testing. It does not replace Microsoft Store signing or SignPath Foundation
signing for public releases.

The private key is created as non-exportable in the current user's certificate
store. Installation trusts only the corresponding public certificate in the
local computer's **Trusted People** store. Never commit a certificate, private
key, PFX file, or generated package to this repository.

## Why the CI package must be repacked

Pull-request and development builds use an unsigned MSIX identity such as:

```text
CN=Lumen ASR Development, OID.2.25...=1
```

The special OID allows unsigned-package testing on supported Windows versions,
but it is an unsigned identity. Microsoft requires a signed package to remove
that OID and to use a manifest `Publisher` that exactly matches the signing
certificate subject.

`sign-local-msix.ps1` performs this conversion only in a temporary copy. It
does not change the downloaded package or any source file.

## Prerequisites

- Windows 10 or Windows 11 x64
- PowerShell 5.1 or later
- Windows 10/11 SDK with `MakeAppx.exe` and `SignTool.exe`
- An unsigned Lumen ASR MSIX from the Windows CI artifact, or one produced by
  `scripts/windows/build-msix.ps1`

## 1. Sign a development package

Run from a normal PowerShell window at the repository root:

```powershell
.\scripts\windows\sign-local-msix.ps1 `
  -PackagePath C:\path\to\Lumen-ASR-windows-x64.msix
```

By default, the signed package and public certificate are written below:

```text
%LOCALAPPDATA%\Lumen ASR\Development\
```

The script:

1. unpacks the MSIX into a temporary directory;
2. replaces only the special unsigned development publisher with
   `CN=Lumen ASR Development`;
3. increments the newest source, installed, or previously generated local
   four-part version so Windows can update an already installed test package
   without deleting its data;
4. creates or reuses a non-exportable RSA development certificate in
   `Cert:\CurrentUser\My`;
5. signs every executable payload with SHA-256 before repacking (Smart App
   Control can reject an unsigned inner `.exe` even when the MSIX envelope is
   signed);
6. repacks and signs the MSIX with SHA-256; and
7. prints the package version, hash, certificate thumbprint, and expiration
   date.

Use `-OutputPath` to select another output outside the repository. Pass
`-Force` only when intentionally replacing that output file. The default local
version is recorded in `last-version.txt` under the development directory and
increments with four-component carry handling. An explicit `-Version` must
contain four numeric components and be newer than the source version.

Before trusting a certificate, compare the displayed subject and thumbprint
with the values from the signing step.

## 2. Trust, install, and launch

Open PowerShell with **Run as administrator**, change to the repository root,
and run:

```powershell
.\scripts\windows\install-local-msix.ps1 `
  -PackagePath "$env:LOCALAPPDATA\Lumen ASR\Development\Packages\Lumen-ASR-windows-x64-local-signed.msix" `
  -ExpectedThumbprint <THUMBPRINT_FROM_SIGNING_STEP> `
  -Launch
```

Administrator access is required because Windows checks self-signed MSIX
certificates in the local computer's **Trusted People** store. The installer
script refuses unsigned packages, hash mismatches, and certificates whose
subject or thumbprint does not match the explicitly expected development
certificate. Before changing trust, it also verifies the supplied manifest's
identity, publisher, and version. It verifies the signature again after
trusting the public certificate and selects the installed package only when
all three manifest values match.

Installing a newer package with the same identity updates the local package.
Rapidly activating multiple builds can leave more than one process running;
close older Lumen ASR instances before comparing builds.

## Clean up the local test identity

The signing command prints the exact certificate thumbprint. To remove the
test installation and certificate later, run the following in an elevated
PowerShell window after replacing `<THUMBPRINT>`:

```powershell
Get-AppxPackage -Name LumenASR.Dev | Remove-AppxPackage
Remove-Item -LiteralPath "Cert:\LocalMachine\TrustedPeople\<THUMBPRINT>"
Remove-Item -LiteralPath "Cert:\CurrentUser\My\<THUMBPRINT>"
```

Delete generated packages under `%LOCALAPPDATA%\Lumen ASR\Development` when
they are no longer needed.

## Distribution boundaries

- A self-signed package is free and suitable for the developer's own machine
  or testers who explicitly install and trust its public certificate.
- It does not build SmartScreen reputation and must not be presented as a
  publicly trusted release.
- Microsoft Store builds keep the Partner Center publisher identity and are
  signed by Microsoft after certification.
- Direct-download releases continue to use the SignPath workflow after the
  project is accepted. Local development keys must never enter that workflow.
- These scripts are Windows-only and do not alter the macOS build, signing,
  entitlements, bundle identifier, or runtime behavior.
