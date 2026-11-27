param(
    [Parameter(Mandatory)]
    [ValidateSet('Close', 'Sign', 'Uninstall')]
    [string] $Action
)

$ErrorActionPreference = 'Stop'
$exe = Join-Path $PSScriptRoot 'igorio.exe'
$subject = 'CN=Igorio Local Code Signing'
$friendlyName = 'Igorio Local Code Signing'
$trustStores = @('Root', 'TrustedPublisher')

function Remove-IgorioCertificates([string[]] $Stores) {
    foreach ($name in $Stores) {
        $storePath = "Cert:\LocalMachine\$name"
        if (!(Test-Path -LiteralPath $storePath)) { continue }
        Get-ChildItem -LiteralPath $storePath |
            Where-Object { $_.Subject -eq $subject -and $_.FriendlyName -eq $friendlyName } |
            ForEach-Object {
                $path = Join-Path $storePath $_.Thumbprint
                Remove-Item -LiteralPath $path -DeleteKey:($name -eq 'My' -and $_.HasPrivateKey)
            }
    }
}

function Add-IgorioTrust([System.Security.Cryptography.X509Certificates.X509Certificate2] $Certificate) {
    $public = [System.Security.Cryptography.X509Certificates.X509Certificate2]::new($Certificate.RawData)
    try {
        foreach ($name in $trustStores) {
            $store = [System.Security.Cryptography.X509Certificates.X509Store]::new($name, 'LocalMachine')
            try {
                $store.Open('ReadWrite')
                $public.FriendlyName = $friendlyName
                $store.Add($public)
            } finally {
                $store.Dispose()
            }
        }
    } finally {
        $public.Dispose()
    }
}

try {
    switch ($Action) {
        'Close' {
            foreach ($process in (Get-Process -Name igorio -ErrorAction SilentlyContinue | Where-Object Path -EQ $exe)) {
                # Without /F, taskkill asks the app's windows to close so it can save its work.
                & "$env:SystemRoot\System32\taskkill.exe" /PID $process.Id
                if (!$process.HasExited -and !$process.WaitForExit(20000)) {
                    throw "Igorio (PID $($process.Id)) is still running."
                }
            }
        }
        'Sign' {
            Remove-IgorioCertificates ($trustStores + 'My')
            try {
                $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject $subject `
                    -FriendlyName $friendlyName -CertStoreLocation 'Cert:\LocalMachine\My' `
                    -KeyAlgorithm RSA -KeyLength 3072 -HashAlgorithm SHA256 `
                    -KeyExportPolicy NonExportable -NotAfter (Get-Date).AddYears(10)
                try {
                    Add-IgorioTrust $cert
                    $null = Set-AuthenticodeSignature -LiteralPath $exe -Certificate $cert -HashAlgorithm SHA256
                    $signature = Get-AuthenticodeSignature -LiteralPath $exe
                    if ($signature.Status -ne 'Valid' -or $signature.SignerCertificate.Thumbprint -ne $cert.Thumbprint) {
                        throw "Signature verification failed: $($signature.StatusMessage)"
                    }
                } finally {
                    Remove-Item -LiteralPath "Cert:\LocalMachine\My\$($cert.Thumbprint)" -DeleteKey
                }
                Write-Output "Verified Igorio signature ($($cert.Thumbprint)) and deleted the private key."
            } catch {
                Write-Error -ErrorRecord $_ -ErrorAction Continue
                Remove-IgorioCertificates $trustStores
                exit 1
            }
        }
        'Uninstall' {
            try {
                Remove-IgorioCertificates ($trustStores + 'My')
                Write-Output 'Removed Igorio signing certificates.'
            } finally {
                # The elevating administrator may differ from the user who enabled launch at login.
                # Clean up loaded user profiles and leave values pointing at other builds alone.
                foreach ($hive in Get-ChildItem 'Registry::HKEY_USERS') {
                    $path = "Registry::$($hive.Name)\Software\Microsoft\Windows\CurrentVersion\Run"
                    $run = Get-Item -LiteralPath $path -ErrorAction SilentlyContinue
                    if ($run -and $run.GetValue('Igorio') -eq ('"' + $exe + '"')) {
                        Remove-ItemProperty -LiteralPath $path -Name Igorio
                    }
                }
            }
        }
    }
} catch {
    Write-Error -ErrorRecord $_ -ErrorAction Continue
    exit 1
}
