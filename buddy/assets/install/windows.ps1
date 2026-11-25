param([string]$Url, [string]$Sha, [long]$Size, [string]$Ca, [string]$Enrollment)
$ErrorActionPreference = 'Stop'
if ([Environment]::GetEnvironmentVariable('PROCESSOR_ARCHITEW6432') -ne 'AMD64' -and $env:PROCESSOR_ARCHITECTURE -ne 'AMD64') { throw 'This build requires x64 Windows.' }
$dir = Join-Path ([Environment]::GetFolderPath('LocalApplicationData')) 'Programs\Buddy'
[IO.Directory]::CreateDirectory($dir) | Out-Null
$target = Join-Path $dir 'buddy.exe'
$staged = Join-Path $dir ('.buddy-install-' + [Guid]::NewGuid().ToString('N') + '.exe')
function Confirm-ExistingImage {
    $existing = Get-Item -Force -LiteralPath $target
    if ($existing.PSIsContainer -or ($existing.Attributes -band [IO.FileAttributes]::ReparsePoint) -or (Get-FileHash -LiteralPath $target -Algorithm SHA256).Hash -ne $Sha) { throw "Already installed at $target. Update Buddy in Settings, or move the existing executable aside and rerun this command." }
}
try {
    $tls = @()
    if ($Ca) { $tls = @('--cacert', $Ca) }
    & curl.exe --fail --silent --show-error --proto '=http,https' --max-filesize $Size @tls --output $staged -- $Url
    if ($LASTEXITCODE -ne 0 -or (Get-Item -LiteralPath $staged).Length -ne $Size -or (Get-FileHash -LiteralPath $staged -Algorithm SHA256).Hash -ne $Sha) { throw 'Buddy download failed verification.' }
    $candidate = $staged
    if (Get-Item -Force -LiteralPath $target -ErrorAction SilentlyContinue) {
        Confirm-ExistingImage
        $candidate = $target
    }
    $storage = @()
    if ($env:BUDDY_ALLOW_FILE_CREDENTIAL -eq '1') { $storage = @('--private-file-credential') }
    $Enrollment | & $candidate redeem-device @storage
    if ($LASTEXITCODE -ne 0) { throw 'Could not link this device.' }
    if ($candidate -eq $staged) {
        try { [IO.File]::Move($staged, $target) }
        catch {
            if (!(Get-Item -Force -LiteralPath $target -ErrorAction SilentlyContinue)) { throw }
            Confirm-ExistingImage
        }
    }
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    if (($userPath -split ';') -notcontains $dir) {
        [Environment]::SetEnvironmentVariable('Path', (($userPath, $dir | Where-Object { $_ }) -join ';'), 'User')
    }
    & $target finish-install
    if ($LASTEXITCODE -ne 0) { Write-Warning 'Buddy is installed and paired. Open Buddy to finish Steam setup later.' }
} finally { if ([IO.File]::Exists($staged)) { [IO.File]::Delete($staged) } }
