[CmdletBinding()]
param(
    [Parameter(Position = 0)]
    [ValidateSet('help', 'build', 'test', 'package', 'release', 'release-key')]
    [string]$Command = 'help',
    [string]$Tag,
    # The release key on this machine; one per publishing machine.
    [string]$KeyId = $env:COMPUTERNAME.ToLowerInvariant()
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$projectRoot = $PSScriptRoot
$target = 'x86_64-pc-windows-msvc'

function Invoke-Checked {
    param([string]$Program, [string[]]$Arguments)
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Program failed (exit $LASTEXITCODE)." }
}

# Runs the built app in one of its publishing modes. It is a GUI program, so
# PowerShell would not wait for it; errors come back in <last arg>.error.txt.
function Invoke-App {
    param([string[]]$Arguments)
    $output = $Arguments[-1]
    $errorFile = "$output.error.txt"
    Remove-Item -LiteralPath $errorFile -ErrorAction SilentlyContinue
    $exe = Join-Path $projectRoot "target\$target\release\just-snapshot.exe"
    $quoted = $Arguments | ForEach-Object { '"' + $_ + '"' }
    $process = Start-Process -FilePath $exe -ArgumentList $quoted -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        $reason = if (Test-Path -LiteralPath $errorFile) { Get-Content -LiteralPath $errorFile -Raw -Encoding UTF8 } else { "exit $($process.ExitCode)" }
        Remove-Item -LiteralPath $errorFile -ErrorAction SilentlyContinue
        throw "just-snapshot $($Arguments[0]) failed: $reason"
    }
}

# The section "## v<version>" of RELEASE_NOTES.md: what users read in the
# update card and on the release page.
function Get-ReleaseNotes {
    param([string]$Version)
    $text = Get-Content -LiteralPath (Join-Path $projectRoot 'RELEASE_NOTES.md') -Raw -Encoding UTF8
    $match = [regex]::Match($text, "(?ms)^##\s+v$([regex]::Escape($Version))\s*$(.*?)(?=^##\s|\z)")
    $notes = $match.Groups[1].Value.Trim()
    if (-not $match.Success -or -not $notes) { throw "Write the notes for v$Version in RELEASE_NOTES.md (## v$Version) first." }
    return $notes
}

function Initialize-BuildTools {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        $cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
        if (Test-Path (Join-Path $cargoBin 'cargo.exe')) {
            $env:PATH = "$cargoBin;$env:PATH"
        } else { throw 'Install Rust (MSVC toolchain) from https://rustup.rs first.' }
    }
    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
        if (-not (Test-Path $vswhere)) { throw 'Install Visual Studio C++ build tools and Windows SDK first.' }
        $vsPath = & $vswhere -latest -products '*' -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
        if (-not $vsPath) { throw 'Visual Studio x64 C++ build tools were not found.' }
        & (Join-Path $vsPath 'Common7\Tools\Launch-VsDevShell.ps1') -Arch amd64 -HostArch amd64 -SkipAutomaticLocation | Out-Host
    }
    Invoke-Checked rustup @('target', 'add', $target)
}

if ($Command -eq 'help') {
    @'
just-snapshot build CLI (Windows x64)
  .\snapshot.ps1 build     Build the release executable
  .\snapshot.ps1 test      Run release tests
  .\snapshot.ps1 package   Test, build, and create dist ZIP + SHA256
  .\snapshot.ps1 release -Tag v0.1.0 [-KeyId <id>]
                          Package, sign the update manifest and upload a
                          GitHub Release draft
  .\snapshot.ps1 release-key [-KeyId <id>]
                          Create this machine's release key (once per machine)
Release requires gh auth login, a clean checkout, an origin tag at HEAD, the
notes for the version in RELEASE_NOTES.md and this machine's release key
(default id: the computer name) among TRUSTED_KEYS in src/release_key.rs.
The tag must match the version in Cargo.toml. Publish the draft on GitHub.
Run release and release-key in your desktop session: the key lives in the
Windows Credential Manager, which remote sessions cannot reach.
'@ | Write-Host
    exit 0
}

Push-Location $projectRoot
try {
    $manifest = Get-Content Cargo.toml -Raw
    $version = [regex]::Match($manifest, '(?m)^version\s*=\s*"([^"]+)"').Groups[1].Value
    if (-not $version) { throw 'Cannot read package version from Cargo.toml.' }
    if ($Tag -and $Command -ne 'release') { throw '-Tag is only supported by release.' }
    if ($Command -eq 'release-key') {
        Initialize-BuildTools
        Invoke-Checked cargo @('build', '--release', '--locked', '--target', $target, '--target-dir', 'target')
        $publicFile = Join-Path ([IO.Path]::GetTempPath()) "just-snapshot-release-key-$KeyId.txt"
        Invoke-App @('--release-key-new', $KeyId, $publicFile)
        $public = (Get-Content -LiteralPath $publicFile -Raw).Trim()
        Remove-Item -LiteralPath $publicFile
        Write-Host "Created release key '$KeyId'; the private key is in the Windows Credential Manager"
        Write-Host "(just-snapshot/release-key/$KeyId) and never leaves this machine."
        Write-Host 'Add this line to TRUSTED_KEYS in src/release_key.rs and ship it in a release'
        Write-Host 'signed by an already trusted machine:'
        Write-Host "    (`"$KeyId`", `"$public`"),"
        exit 0
    }
    if ($Command -eq 'release') {
        if ($Tag -cne "v$version") { throw "Use -Tag v$version (Cargo.toml version)." }
        $notes = Get-ReleaseNotes $version
        if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { throw 'Install GitHub CLI and run gh auth login first.' }
        Invoke-Checked gh @('auth', 'status')
        $status = & git status --porcelain
        if ($LASTEXITCODE -ne 0) { throw 'Cannot read git status.' }
        if ($status) { throw 'Commit changes before creating a release.' }
        $head = & git rev-parse HEAD
        if ($LASTEXITCODE -ne 0) { throw 'Cannot resolve HEAD.' }
        $remoteTag = & git ls-remote origin "refs/tags/$Tag" "refs/tags/$Tag^{}"
        if ($LASTEXITCODE -ne 0) { throw 'Cannot read origin tags.' }
        if (-not $remoteTag) { throw "Push tag $Tag to origin first." }
        $resolvedTag = @($remoteTag)[-1] -split '\s+'
        if ($resolvedTag[0] -ne $head) { throw 'Origin tag must point to the current HEAD.' }
    }
    Initialize-BuildTools
    if ($Command -in @('test', 'package', 'release')) {
        Invoke-Checked cargo @('test', '--release', '--locked', '--target', $target, '--target-dir', 'target')
    }
    if ($Command -ne 'test') {
        Invoke-Checked cargo @('build', '--release', '--locked', '--target', $target, '--target-dir', 'target')
    }
    if ($Command -in @('package', 'release')) {
        $dist = Join-Path $projectRoot 'dist'
        New-Item -ItemType Directory -Path $dist -Force | Out-Null
        $zip = Join-Path $dist "just-snapshot-v$version-windows-x64.zip"
        $staging = Join-Path ([IO.Path]::GetTempPath()) ('just-snapshot-package-' + [guid]::NewGuid())
        New-Item -ItemType Directory -Path $staging | Out-Null
        try {
            Copy-Item "target\$target\release\just-snapshot.exe", 'README.md', 'LICENSE' -Destination $staging
            Copy-Item licenses -Destination $staging -Recurse
            Compress-Archive -Path (Join-Path $staging '*') -DestinationPath $zip -Force
        } finally {
            $resolvedStaging = [IO.Path]::GetFullPath($staging)
            $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\') + '\'
            if (-not $resolvedStaging.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase)) {
                throw 'Refusing to remove a staging directory outside TEMP.'
            }
            Remove-Item -LiteralPath $resolvedStaging -Recurse
        }
        $checksum = "$zip.sha256"
        $hash = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLowerInvariant()
        [IO.File]::WriteAllText($checksum, "$hash  $([IO.Path]::GetFileName($zip))`n", [Text.UTF8Encoding]::new($false))
        Write-Host "Package: $zip"
        Write-Host "SHA256:  $checksum"
        if ($Command -eq 'release') {
            # The bare executable is what the in-app update downloads.
            $exe = Join-Path $dist 'just-snapshot.exe'
            Copy-Item "target\$target\release\just-snapshot.exe" $exe -Force
            $payload = [ordered]@{
                version = $version
                notes = $notes
                url = "https://github.com/AllToBeNize/just-snapshot/releases/download/$Tag/just-snapshot.exe"
                sha256 = (Get-FileHash -LiteralPath $exe -Algorithm SHA256).Hash.ToLowerInvariant()
                size = (Get-Item -LiteralPath $exe).Length
            } | ConvertTo-Json -Compress
            $payloadFile = Join-Path $dist 'latest.payload.json'
            $latest = Join-Path $dist 'latest.json'
            $notesFile = Join-Path $dist 'release-notes.md'
            [IO.File]::WriteAllText($payloadFile, $payload, [Text.UTF8Encoding]::new($false))
            [IO.File]::WriteAllText($notesFile, $notes, [Text.UTF8Encoding]::new($false))
            Invoke-App @('--release-sign', $KeyId, $payloadFile, $latest)
            Remove-Item -LiteralPath $payloadFile
            Write-Host "Signed:  $latest (key $KeyId)"
            Invoke-Checked gh @('release', 'create', $Tag, $zip, $checksum, $exe, $latest, '--verify-tag', '--draft', '--title', "just-snapshot $Tag", '--notes-file', $notesFile)
        }
    }
} catch {
    Write-Error $_ -ErrorAction Continue
    exit 1
} finally { Pop-Location }
