# Installs or updates AudioSync for the current user from a GitHub release.
#   irm https://github.com/yassersutojoyo/audiosync/releases/latest/download/install.ps1 | iex
#   & ([scriptblock]::Create((irm .../install.ps1))) -Version 0.8.0   a given version instead of the latest
#   powershell -File scripts\install.ps1 -DryRun [-Version 0.8.0]    print what it would download, change nothing
# Checks the zip against the release's SHA256SUMS.txt before anything is
# installed, needs no administrator rights and writes only to the user's own
# folders: the app under %LOCALAPPDATA%\Programs\AudioSync, a Start menu shortcut.
param([string]$Version = '', [switch]$DryRun)

function Install-AudioSync([string]$Version, [bool]$DryRun) {
  # Function scope, so a run through iex leaves the caller's preferences alone.
  $ErrorActionPreference = 'Stop'
  # Windows PowerShell's progress bar slows Invoke-WebRequest down many times over.
  $ProgressPreference = 'SilentlyContinue'
  $repo = 'yassersutojoyo/audiosync'
  # The release key's public half ('ssh-ed25519 AAAA...'). Empty until that key exists
  # (RELEASE.md, "Signed checksums"); until then the install trusts the tag over HTTPS alone.
  $releaseKey = ''
  # The first version signed with that key; from it on a release without SHA256SUMS.txt.sig is refused.
  $keySince = ''
  $dir = Join-Path $env:LOCALAPPDATA 'Programs\AudioSync'
  $exe = Join-Path $dir 'audiosync.exe'
  $shortcut = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\AudioSync.lnk'

  # The registry names the machine's own architecture, also to an x64 PowerShell emulated on ARM64.
  $key = Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Environment' -ErrorAction SilentlyContinue
  $native = if ($key) { $key.PROCESSOR_ARCHITECTURE } else { $env:PROCESSOR_ARCHITECTURE }
  switch ($native) {
    'AMD64' { $arch = 'x64' }
    'ARM64' { $arch = 'arm64' }
    default { throw "AudioSync install: AudioSync runs on x64 and ARM64 PCs; this one is '$native'." }
  }

  if (-not $Version) {
    Write-Host 'Looking up the latest release'
    try {
      $Version = (Invoke-RestMethod -UseBasicParsing "https://api.github.com/repos/$repo/releases/latest").tag_name
    } catch {
      throw "AudioSync install: couldn't reach the GitHub API (it allows 60 lookups an hour per address). Name a version instead: & ([scriptblock]::Create((irm https://github.com/$repo/releases/latest/download/install.ps1))) -Version 0.8.0"
    }
    if (-not $Version) { throw 'AudioSync install: the GitHub API answer had no release tag.' }
  }
  $Version = $Version -replace '^v', ''
  if ($Version -notmatch '\A[0-9]+(\.[0-9]+)*(-[0-9A-Za-z.]+)?\z') {
    throw "AudioSync install: '$Version' isn't a version number like 0.8.0."
  }

  $base = "https://github.com/$repo/releases/download/v$Version"
  $zip = "AudioSync-$Version-windows-$arch.zip"
  if ($DryRun) {
    "version $Version"
    "zip $base/$zip"
    "sums $base/SHA256SUMS.txt"
    "app $exe"
    "shortcut $shortcut"
    return
  }

  $tmp = Join-Path ([IO.Path]::GetTempPath()) ('audiosync-install-' + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory $tmp | Out-Null
  try {
    try {
      Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS.txt" -OutFile "$tmp\SHA256SUMS.txt"
    } catch {
      throw "AudioSync install: couldn't download $base/SHA256SUMS.txt"
    }
    if ($releaseKey) {
      $sig = "$tmp\SHA256SUMS.txt.sig"
      $signed = $true
      try {
        Invoke-WebRequest -UseBasicParsing "$base/SHA256SUMS.txt.sig" -OutFile $sig
      } catch {
        # A release from before the key has no signature.
        if ([int]$_.Exception.Response.StatusCode -ne 404) { throw "AudioSync install: couldn't download $base/SHA256SUMS.txt.sig" }
        # [version] wants two to four numeric fields: drop a -suffix and pad a bare major.
        $plain = { param($v) $v = $v -replace '-.*$', ''; if ($v -notmatch '\.') { $v += '.0' }; [version]$v }
        if ($keySince -and (& $plain $Version) -ge (& $plain $keySince)) {
          throw "AudioSync install: AudioSync $Version has no SHA256SUMS.txt.sig, though every release from $keySince on is signed. Nothing was installed."
        }
        $signed = $false
      }
      if ($signed) {
        if (-not (Get-Command ssh-keygen -ErrorAction SilentlyContinue)) {
          throw "AudioSync install: ssh-keygen (the Windows OpenSSH client) is missing, so the release's signature can't be checked. Nothing was installed."
        }
        [IO.File]::WriteAllText("$tmp\allowed_signers", "audiosync-release $releaseKey`n")
        # Start-Process hands ssh-keygen the file itself as input; a PowerShell pipe would re-encode it.
        $verify = Start-Process ssh-keygen -NoNewWindow -Wait -PassThru `
          -ArgumentList '-Y', 'verify', '-f', "`"$tmp\allowed_signers`"", '-I', 'audiosync-release', '-n', 'file', '-s', "`"$sig`"" `
          -RedirectStandardInput "$tmp\SHA256SUMS.txt" -RedirectStandardOutput "$tmp\verify.out" -RedirectStandardError "$tmp\verify.err"
        if ($verify.ExitCode -ne 0) {
          throw "AudioSync install: SHA256SUMS.txt doesn't carry the release key's signature. Nothing was installed."
        }
      }
    }
    $expected = $null
    foreach ($line in Get-Content "$tmp\SHA256SUMS.txt") {
      $hash, $name = $line -split '\s+', 2
      if ($name -eq $zip -or $name -eq "*$zip") { $expected = $hash; break }
    }
    if ($expected -notmatch '\A[0-9a-fA-F]{64}\z') {
      throw "AudioSync install: AudioSync $Version has no Windows $arch zip ($zip isn't in its SHA256SUMS.txt)."
    }

    Write-Host "Downloading AudioSync $Version"
    try {
      Invoke-WebRequest -UseBasicParsing "$base/$zip" -OutFile "$tmp\$zip"
    } catch {
      throw "AudioSync install: couldn't download $base/$zip"
    }
    Write-Host 'Checking the SHA-256 checksum'
    if ((Get-FileHash -Algorithm SHA256 "$tmp\$zip").Hash -ne $expected) {
      throw "AudioSync install: the checksum of $zip doesn't match SHA256SUMS.txt. Nothing was installed."
    }
    Expand-Archive "$tmp\$zip" "$tmp\unpacked"
    if (-not (Test-Path "$tmp\unpacked\audiosync.exe")) { throw "AudioSync install: $zip doesn't hold audiosync.exe." }

    $running = @(Get-Process audiosync -ErrorAction SilentlyContinue)
    # Another copy keeps the single-instance lock, so the new exe would only show its window.
    if (@($running | Where-Object { $_.Path -ne $exe }).Count -gt 0) {
      throw "AudioSync install: a copy of AudioSync from outside $dir is running. Quit it from its tray icon and run this again. Nothing was installed."
    }
    if ($running.Count -gt 0) {
      Write-Host 'Closing the running AudioSync'
      $running | Stop-Process -Force
      $running | Wait-Process -Timeout 10
    }

    Write-Host "Installing to $dir"
    New-Item -ItemType Directory -Force $dir | Out-Null
    Copy-Item "$tmp\unpacked\*" $dir -Force
    $link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
    $link.TargetPath = $exe
    $link.WorkingDirectory = $dir
    $link.Save()

    # Invoke-WebRequest marks no download as coming from the internet, so SmartScreen doesn't stop this copy.
    Write-Host 'Opening AudioSync'
    Start-Process $exe
    Write-Host "Done: AudioSync $Version is in $dir, with a shortcut in the Start menu."
  } finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
  }
}

Install-AudioSync -Version $Version -DryRun $DryRun.IsPresent
