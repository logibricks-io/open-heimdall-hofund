# Sign every .exe (hofund.exe, hofundd.exe) inside each Windows zip, then rebuild the zip and its checksum.
# An empty certificate group skips. A certificate the platform rejects fails the job.

$ErrorActionPreference = 'Stop'

if ([string]::IsNullOrEmpty($env:WINDOWS_CERTIFICATE) -or [string]::IsNullOrEmpty($env:WINDOWS_CERTIFICATE_PASSWORD)) {
    Write-Host 'Windows signing secrets are absent; signing is skipped.'
    exit 0
}

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
Set-Location $root

$zips = @(Get-ChildItem -Path 'target/distrib' -Filter '*-pc-windows-msvc.zip' -File -ErrorAction SilentlyContinue)
if ($zips.Count -eq 0) {
    Write-Host 'No Windows CLI archive was built in this job; signing is skipped.'
    exit 0
}

$pfx = Join-Path $env:RUNNER_TEMP 'hofund.pfx'
try {
    [IO.File]::WriteAllBytes($pfx, [Convert]::FromBase64String($env:WINDOWS_CERTIFICATE))
    $secure = ConvertTo-SecureString $env:WINDOWS_CERTIFICATE_PASSWORD -AsPlainText -Force
    $cert = Import-PfxCertificate -FilePath $pfx -CertStoreLocation Cert:\CurrentUser\My -Password $secure
    $signtool = Get-ChildItem 'C:\Program Files (x86)\Windows Kits\10\bin' -Recurse -Filter signtool.exe |
        Where-Object { $_.FullName -match '\\x64\\signtool.exe$' } |
        Sort-Object FullName -Descending |
        Select-Object -First 1
    if (-not $signtool) {
        throw 'signtool.exe was not found'
    }

    foreach ($zip in $zips) {
        $dir = Join-Path $zip.DirectoryName $zip.BaseName
        $files = @(Get-ChildItem -Path $dir -File)
        if ($files.Count -eq 0) {
            throw "Windows archive directory $dir is empty"
        }
        $exes = @($files | Where-Object { $_.Extension -eq '.exe' })
        if ($exes.Count -eq 0) {
            throw "Windows archive directory $dir has no executable"
        }
        foreach ($exe in $exes) {
            & $signtool.FullName sign /fd SHA256 /sha1 $cert.Thumbprint /tr http://timestamp.digicert.com /td SHA256 $exe.FullName
            if ($LASTEXITCODE -ne 0) {
                throw "signtool failed for $($exe.FullName)"
            }
        }
        $rebuilt = Join-Path $env:RUNNER_TEMP $zip.Name
        if (Test-Path $rebuilt) {
            Remove-Item $rebuilt
        }
        & tar -a -c -f $rebuilt -C $dir @($files.Name)
        if ($LASTEXITCODE -ne 0) {
            throw "repacking $($zip.Name) failed"
        }
        Move-Item -Force $rebuilt $zip.FullName
        & bash .github/scripts/rewrite-archive-checksum.sh $zip.FullName dist-manifest.json
        if ($LASTEXITCODE -ne 0) {
            throw "checksum rewrite failed for $($zip.Name)"
        }
    }
    Write-Host 'Windows signing secrets are present; archives were signed.'
}
finally {
    if (Test-Path $pfx) {
        Remove-Item $pfx
    }
}
