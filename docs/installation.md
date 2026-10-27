# Install a native binary

Outpace AceStream runs as a standalone command-line daemon. A native installation
does not need Docker, Rust, Node.js, or an original Ace Stream engine. A separate
player opens the HTTP or HLS output. Optional HLS audio normalization needs an
operator-installed FFmpeg; ordinary playback does not automatically install or
invoke it.

This guide targets **v0.9.0**. Check the
[release page](https://github.com/open-ott-play/outpace-acestream/releases/tag/v0.9.0)
and verify that its assets have been published before installing. Commands below describe a fresh install;
use [Upgrade and rollback](#upgrade-and-rollback) for an existing service.

- [Choose an architecture](#choose-an-architecture)
- [Download and verify on Linux or macOS](#download-and-verify-on-linux-or-macos)
- [Linux: run and install a systemd service](#linux-run-and-install-a-systemd-service)
- [macOS: run and install a LaunchAgent](#macos-run-and-install-a-launchagent)
- [Windows: install and run with PowerShell](#windows-install-and-run-with-powershell)
- [Check a running installation](#check-a-running-installation)
- [Upgrade and rollback](#upgrade-and-rollback)
- [Source builds](#source-builds)

## Choose an architecture

The release workflow builds these seven binary archives. The target is part of the
filename; it is not a command-line argument.

| Operating system | Architecture | Target | Archive |
| --- | --- | --- | --- |
| Linux | x86-64 / amd64 | `x86_64-unknown-linux-musl` | `.tar.gz` |
| Linux | ARM64 / aarch64 | `aarch64-unknown-linux-musl` | `.tar.gz` |
| Linux | ARMv7, hard-float | `armv7-unknown-linux-musleabihf` | `.tar.gz` |
| macOS | Apple Silicon | `aarch64-apple-darwin` | `.tar.gz` |
| macOS | Intel | `x86_64-apple-darwin` | `.tar.gz` |
| Windows | x86-64 / amd64 | `x86_64-pc-windows-msvc` | `.zip` |
| Windows | ARM64 | `aarch64-pc-windows-msvc` | `.zip` |

For example, the Apple Silicon archive is
`outpace-0.9.0-aarch64-apple-darwin.tar.gz`. Its top-level directory has the same
name without `.tar.gz`. The Windows executable is `outpace.exe`; the others are
`outpace`.

On Linux, `uname -m` identifies the running architecture. On macOS, check **About
This Mac**: a terminal running under Rosetta can report an Intel process even on
Apple Silicon. On Windows, check **Settings → System → About → System type**.

Artifact availability is not a claim that every historical OS version or every
ARM board has been tested. Linux packages use musl rather than a host glibc ABI;
see [Linux portability](linux-portability.md) for constrained and 32-bit systems.
The release workflow builds macOS packages on macOS 15 runners and Windows
packages on Windows Server 2025 / Windows 11 ARM runners; those are build
environments, not a published minimum-OS support matrix.

## Download and verify on Linux or macOS

Choose `target` from the table before running this block. It downloads the binary
archive, its matching complete-source archive, and `SHA256SUMS` from the same
release. Both archives must pass verification before extraction. It deliberately
refuses to replace an existing executable or package directory.

```sh
(
  set -eu
  version=0.9.0
  target=aarch64-apple-darwin # Change this for your machine.
  case "$target" in
    x86_64-unknown-linux-musl|aarch64-unknown-linux-musl|armv7-unknown-linux-musleabihf|x86_64-apple-darwin|aarch64-apple-darwin) ;;
    *) echo 'Select a Linux or macOS target from the table.' >&2; exit 1 ;;
  esac
  package="outpace-$version-$target"
  archive="$package.tar.gz"
  base="https://github.com/open-ott-play/outpace-acestream/releases/download/v$version"
  stage=$(mktemp -d)
  cd "$stage"

  curl --fail --location --proto '=https' --proto-redir '=https' \
    --output SHA256SUMS "$base/SHA256SUMS"
  source_archive=$(awk -v prefix="outpace-source-$version-$target-" \
    'index($2, prefix) == 1 && $2 ~ /[.]tar[.]gz$/ { print $2 }' SHA256SUMS)
  case "$source_archive" in
    ''|*[!A-Za-z0-9._-]*) echo 'Missing or ambiguous source archive.' >&2; exit 1 ;;
  esac
  for name in "$archive" "$source_archive"; do
    curl --fail --location --proto '=https' --proto-redir '=https' \
      --output "$name" "$base/$name"
  done
  awk -v binary="$archive" -v source="$source_archive" \
    '$2 == binary || $2 == source' SHA256SUMS > SHA256SUMS.selected
  [ "$(wc -l < SHA256SUMS.selected | tr -d ' ')" = 2 ]
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum --check SHA256SUMS.selected
  else
    shasum -a 256 --check SHA256SUMS.selected
  fi

  tar -xzf "$archive"
  destination="$HOME/.local/share/outpace-packages/$version/$target"
  [ ! -e "$destination" ]
  [ ! -e "$HOME/.local/bin/outpace" ]
  mkdir -p "$(dirname "$destination")" "$HOME/.local/bin"
  cp -R "$package" "$destination"
  cp "$archive" "$source_archive" SHA256SUMS "$destination/"
  install -m 755 "$destination/outpace" "$HOME/.local/bin/outpace"
  "$HOME/.local/bin/outpace" --version
  printf 'Verified packages retained in %s\nDownload staging: %s\n' "$destination" "$stage"
)
```

Use `~/.local/bin/outpace` directly, or add `~/.local/bin` to your shell's `PATH`.
The retained package includes `LICENSE`, `NOTICE`, `AUTHORS.md`,
`THIRD_PARTY_NOTICES.txt`, `source.json`, and the embedded workspace source.
The separate complete-source archive adds vendored dependencies and rebuild
instructions. Keep these with a redistributed binary. SHA-256 verification
checks consistency with the downloaded release manifest; it is not a claim of
independent code signing or reproducible builds.

### Data, cache and ports

Keep a persistent data directory for `identity.seed` and broadcast state. Run
only one daemon against that directory. Preserve it when updating the binary.

**v0.8.1 requires an explicit cache path when you relocate the data directory.**
Setting `OUTPACE_DATA_DIR` alone leaves the old default cache location in that
release. The examples below set both variables. Version 0.9.0 includes the fix,
but that does not change an existing v0.8.1 executable.

HTTP `6878` and RTMP `1935` are bound to loopback in these examples. The peer
listener uses TCP `8621` on all interfaces, with automatic router port mapping
disabled. This keeps HTTP control/ingest local while allowing deliberate inbound
P2P configuration. Open or forward TCP `8621` only if you intend inbound peer
participation. DHT uses outbound UDP with return traffic; there is no UDP `8621`
listener to publish. See [deployment](deployment.md) for networking modes.

## Linux: run and install a systemd service

First run it in a terminal as your ordinary user:

```sh
mkdir -p "$HOME/.local/share/outpace"
OUTPACE_DATA_DIR="$HOME/.local/share/outpace" \
OUTPACE_CACHE_DIR="$HOME/.local/share/outpace/cache" \
OUTPACE_CACHE_TYPE=memory \
OUTPACE_BIND=127.0.0.1:6878 \
OUTPACE_RTMP_BIND=127.0.0.1:1935 \
OUTPACE_PEER_LISTEN=0.0.0.0:8621 \
OUTPACE_ENABLE_PORT_MAPPING=0 \
  "$HOME/.local/bin/outpace" serve
```

Leave it running and perform the [health check](#check-a-running-installation)
from another terminal. `Ctrl-C` stops this foreground instance. Stop it before
starting the system service, which uses the same ports and a **different** data
directory below.

For a new system-wide installation on a systemd host, create a dedicated account
and install the verified executable. The following account-creation command is
for distributions that provide `useradd`; use your distribution's equivalent if
needed. Do not change an existing account without inspecting it.

```sh
sudo useradd --system --user-group --home-dir /var/lib/outpace \
  --shell /usr/sbin/nologin outpace
sudo install -m 755 "$HOME/.local/bin/outpace" /usr/local/bin/outpace
```

Save this unit as `/etc/systemd/system/outpace.service`:

```ini
[Unit]
Description=Outpace AceStream engine
Wants=network-online.target
After=network-online.target

[Service]
Type=simple
User=outpace
Group=outpace
ExecStart=/usr/local/bin/outpace serve
Restart=on-failure
RestartSec=5
StateDirectory=outpace
StateDirectoryMode=0700
WorkingDirectory=/var/lib/outpace
Environment=OUTPACE_DATA_DIR=/var/lib/outpace
Environment=OUTPACE_CACHE_DIR=/var/lib/outpace/cache
Environment=OUTPACE_CACHE_TYPE=memory
Environment=OUTPACE_BIND=127.0.0.1:6878
Environment=OUTPACE_RTMP_BIND=127.0.0.1:1935
Environment=OUTPACE_PEER_LISTEN=0.0.0.0:8621
Environment=OUTPACE_ENABLE_PORT_MAPPING=0
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
UMask=0077

[Install]
WantedBy=multi-user.target
```

`StateDirectory` creates persistent writable state owned by the service account;
it remains writable with `ProtectSystem=strict`. These settings are described in
the [systemd execution documentation](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html).
This example starts a new identity. To migrate an existing identity, stop its
previous owner first and deliberately migrate its private state and ownership;
do not run two copies or regenerate `identity.seed` as a troubleshooting step.

```sh
sudo systemd-analyze verify /etc/systemd/system/outpace.service
sudo systemctl daemon-reload
sudo systemctl enable --now outpace.service
sudo systemctl status outpace.service
sudo journalctl -u outpace.service -n 100 --no-pager
```

Stop with `sudo systemctl stop outpace.service`. Disable automatic startup with
`sudo systemctl disable outpace.service`; neither command deletes state.

## macOS: run and install a LaunchAgent

Run the verified native binary directly. Apple Silicon does not need a Linux VM
or Rosetta when using the `aarch64-apple-darwin` package.

```sh
mkdir -p "$HOME/Library/Application Support/outpace"
OUTPACE_DATA_DIR="$HOME/Library/Application Support/outpace" \
OUTPACE_CACHE_DIR="$HOME/Library/Application Support/outpace/cache" \
OUTPACE_CACHE_TYPE=memory \
OUTPACE_BIND=127.0.0.1:6878 \
OUTPACE_RTMP_BIND=127.0.0.1:1935 \
OUTPACE_PEER_LISTEN=0.0.0.0:8621 \
OUTPACE_ENABLE_PORT_MAPPING=0 \
  "$HOME/.local/bin/outpace" serve
```

If macOS presents a security alert, verify the selected release and consult
[Apple's instructions for downloaded apps](https://support.apple.com/en-us/102445).
These packages do not promise Developer ID notarization. Do not disable
Gatekeeper globally or treat an unexpected alert as proof that the file is safe.

After the foreground health check, stop it with `Ctrl-C`. For a service that runs
while you are logged in, create `~/Library/LaunchAgents` and
`~/Library/Logs/outpace`, then save the following as
`~/Library/LaunchAgents/org.open-ott-play.outpace.plist`. Replace **every**
`/Users/YOUR_USER` with your actual home directory. LaunchAgent values require
absolute paths; `~` and `$HOME` are not shell-expanded inside the plist. XML-escape
special characters such as `&` in a path.

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>org.open-ott-play.outpace</string>
  <key>ProgramArguments</key>
  <array><string>/Users/YOUR_USER/.local/bin/outpace</string><string>serve</string></array>
  <key>EnvironmentVariables</key>
  <dict>
    <key>OUTPACE_DATA_DIR</key><string>/Users/YOUR_USER/Library/Application Support/outpace</string>
    <key>OUTPACE_CACHE_DIR</key><string>/Users/YOUR_USER/Library/Application Support/outpace/cache</string>
    <key>OUTPACE_CACHE_TYPE</key><string>memory</string>
    <key>OUTPACE_BIND</key><string>127.0.0.1:6878</string>
    <key>OUTPACE_RTMP_BIND</key><string>127.0.0.1:1935</string>
    <key>OUTPACE_PEER_LISTEN</key><string>0.0.0.0:8621</string>
    <key>OUTPACE_ENABLE_PORT_MAPPING</key><string>0</string>
  </dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardOutPath</key><string>/Users/YOUR_USER/Library/Logs/outpace/stdout.log</string>
  <key>StandardErrorPath</key><string>/Users/YOUR_USER/Library/Logs/outpace/stderr.log</string>
</dict>
</plist>
```

```sh
mkdir -p "$HOME/Library/LaunchAgents" "$HOME/Library/Logs/outpace"
plutil -lint "$HOME/Library/LaunchAgents/org.open-ott-play.outpace.plist"
launchctl bootstrap "gui/$(id -u)" "$HOME/Library/LaunchAgents/org.open-ott-play.outpace.plist"
launchctl print "gui/$(id -u)/org.open-ott-play.outpace"
tail -n 100 "$HOME/Library/Logs/outpace/stderr.log"
```

The GUI login session must exist for `gui/UID`. This is a per-user LaunchAgent,
not a boot-time system daemon. `KeepAlive` restarts it after exit; unload it before
an upgrade or intentional stop:

```sh
launchctl bootout "gui/$(id -u)" "$HOME/Library/LaunchAgents/org.open-ott-play.outpace.plist"
```

To prevent the next login from loading it, move the plist out of `LaunchAgents`;
leave the data directory intact. Log files in this example are ordinary files;
rotate them as part of host maintenance. See Apple's
[LaunchAgent documentation](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)
for the lifecycle and plist keys.

## Windows: install and run with PowerShell

The published Windows binaries import `VCRUNTIME140.dll`. Install or update the
[Microsoft Visual C++ v14 Redistributable](https://learn.microsoft.com/en-us/cpp/windows/latest-supported-vc-redist)
for your selected executable architecture, **x64** or **ARM64**, if it is not
already available. A full Visual Studio installation is not required. If Windows
reports a missing runtime DLL, use that Microsoft package rather than downloading
individual DLL files.

Use **PowerShell 7.5 or later** for this recipe, running as your ordinary user.
The engine itself is a native executable; this shell version is used to preserve
an explicitly empty environment variable. Windows PowerShell 5.1 does not have
the same empty-value behavior. See Microsoft's
[environment-variable documentation](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.core/about/about_environment_variables?view=powershell-7.5).

Set `$Target` to the native Windows architecture from the table. This downloads
and verifies both archives, then installs into a new versioned directory without
replacing another installation.

```powershell
$ErrorActionPreference = 'Stop'
if ($PSVersionTable.PSVersion -lt [version]'7.5') {
    throw 'Use PowerShell 7.5 or later for this recipe.'
}
$Version = '0.9.0'
$Target = 'x86_64-pc-windows-msvc' # Use aarch64-pc-windows-msvc on Windows ARM64.
if ($Target -notin @('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')) {
    throw 'Select a Windows target from the architecture table.'
}
$Package = "outpace-$Version-$Target"
$Archive = "$Package.zip"
$Base = "https://github.com/open-ott-play/outpace-acestream/releases/download/v$Version"
$Stage = Join-Path ([IO.Path]::GetTempPath()) ('outpace-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $Stage | Out-Null
Invoke-WebRequest "$Base/SHA256SUMS" -OutFile (Join-Path $Stage 'SHA256SUMS')
$Rows = @(Get-Content (Join-Path $Stage 'SHA256SUMS') | ForEach-Object {
    if ($_ -match '^([0-9a-fA-F]{64})\s+([A-Za-z0-9._-]+)$') {
        [pscustomobject]@{ Hash = $Matches[1]; Name = $Matches[2] }
    }
})
$SourceRows = @($Rows | Where-Object Name -Like "outpace-source-$Version-$Target-*.tar.gz")
if ($SourceRows.Count -ne 1) { throw 'Missing or ambiguous complete-source archive.' }
$SourceArchive = $SourceRows[0].Name
foreach ($Name in @($Archive, $SourceArchive)) {
    $Expected = @($Rows | Where-Object Name -EQ $Name)
    if ($Expected.Count -ne 1) { throw "Missing or duplicate checksum for $Name" }
    $File = Join-Path $Stage $Name
    Invoke-WebRequest "$Base/$Name" -OutFile $File
    if ((Get-FileHash -LiteralPath $File -Algorithm SHA256).Hash -ne $Expected[0].Hash) {
        throw "Checksum mismatch for $Name"
    }
}
$Install = Join-Path $env:LOCALAPPDATA "Programs\Outpace\$Version\$Target"
if (Test-Path -LiteralPath $Install) { throw 'Installation already exists; use the upgrade procedure.' }
Expand-Archive -LiteralPath (Join-Path $Stage $Archive) -DestinationPath $Stage
New-Item -ItemType Directory -Path (Split-Path $Install) -Force | Out-Null
Move-Item -LiteralPath (Join-Path $Stage $Package) -Destination $Install
Copy-Item (Join-Path $Stage 'SHA256SUMS'), (Join-Path $Stage $Archive), (Join-Path $Stage $SourceArchive) -Destination $Install
& (Join-Path $Install 'outpace.exe') --version
```

These commands use Microsoft's [Get-FileHash](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.utility/get-filehash?view=powershell-7.5)
and [Expand-Archive](https://learn.microsoft.com/en-us/powershell/module/microsoft.powershell.archive/expand-archive).
Retain the full package and complete-source archive, including their notices.
These packages do not claim Authenticode signing. Do not disable Windows security
features to make an unexpected or unverified executable run.

Start the daemon in the same PowerShell session:

```powershell
$env:OUTPACE_DATA_DIR = Join-Path $env:LOCALAPPDATA 'Outpace\data'
New-Item -ItemType Directory -Path $env:OUTPACE_DATA_DIR -Force | Out-Null
$env:OUTPACE_CACHE_TYPE = 'memory'
$env:OUTPACE_CACHE_DIR = ''
$env:OUTPACE_BIND = '127.0.0.1:6878'
$env:OUTPACE_RTMP_BIND = '127.0.0.1:1935'
$env:OUTPACE_PEER_LISTEN = '0.0.0.0:8621'
$env:OUTPACE_ENABLE_PORT_MAPPING = '0'
& (Join-Path $Install 'outpace.exe') serve
```

**Windows peer-cache limitation:** durable warm-peer hints currently require
Unix ownership/inode checks. Windows can stream and retain its engine identity,
but that private peer-hint store is not supported there. The explicit empty cache
path above keeps hints in memory and avoids repeated persistence attempts. Do
not combine it with disk-mode media caching. With an older shell that removes
empty variables, use the ordinary default cache path and expect persistence
warnings; a writable directory alone cannot add the missing Windows backend.

`Ctrl-C` stops the foreground process. For optional login startup, save the
environment assignments and invocation in a local `.ps1` file; use an absolute
executable path or `$PSScriptRoot` rather than relying on a previous session's
`$Install`. Create a shortcut to your PowerShell 7.5+ executable with arguments
`-NoProfile -File "C:\path\to\start-outpace.ps1"` in the current user's
`shell:startup` folder. This launches at login, not before login, and does not
provide automatic crash recovery. Follow Microsoft's
[startup-folder instructions](https://support.microsoft.com/en-gb/windows/experience/startup-boot/configure-startup-applications-in-windows).
Remove that shortcut to disable login startup. Outpace does not implement the
Windows Service Control Manager protocol; do not register `outpace.exe` directly
with `sc.exe create` and expect it to behave as a native Windows service.

## Check a running installation

Use another terminal while the daemon is running. On Linux or macOS:

```sh
curl --fail --show-error http://127.0.0.1:6878/healthz
curl --fail --show-error http://127.0.0.1:6878/source.json
curl --fail --show-error http://127.0.0.1:6878/streams
```

On Windows:

```powershell
(Invoke-WebRequest 'http://127.0.0.1:6878/healthz').Content
Invoke-RestMethod 'http://127.0.0.1:6878/source.json'
Invoke-RestMethod 'http://127.0.0.1:6878/streams'
```

`healthz` should return `ok`; `source.json` identifies the running version, target,
source digest and complete-source URL. A healthy listener is not proof of a
working source or player. Continue with the playback examples in the
[native API guide](native-api.md). For a client using `/ace/...` routes, set
`OUTPACE_ACE_COMPAT=1` in that service's environment and restart it; native
`/streams/...` routes do not require that opt-in.

The daemon's `/source` page and `/source.tar.gz` remain available for its matching
source offer. On Windows, use `Invoke-WebRequest -OutFile` to save the archive
without text conversion. Do not share an `identity.seed`, raw peer-hint file, or
private source URL as a diagnostic attachment.

## Upgrade and rollback

1. Download a new version into a separate directory and verify its binary **and**
   complete-source archive against that release's manifest. Read its release
   notes and preserve the current binary, package, configuration and private state
   backup. Never mix a binary from one release with another release's source offer.
2. Inspect `/streams` and arrange an appropriate stop if viewers are active.
   Stop the owning systemd unit, unload the owning LaunchAgent, or stop the Windows
   foreground/login process. Confirm it no longer owns the configured ports.
3. Replace the executable at the service's configured path, or update the service
   path to the verified new version. Retain the previous executable for rollback.
   Keep the same data directory and account; an upgrade must not manufacture a new
   identity or wipe unrelated files. Restart through the same service manager.
4. Check version/source identity, health, logs and actual playback. If validation
   fails, stop the new process, restore the saved executable/configuration and
   restart the old version. Consult release-specific state-migration guidance
   before downgrading after a future state-format change.

To uninstall, disable the relevant startup entry and remove only the executable
and package versions you no longer need. Keep state and backups until you
deliberately choose to retire that node identity. Removing a binary is separate
from deleting private state.

## Source builds

Install the Rust toolchain pinned by `rust-toolchain.toml` and your target's native
linker/build prerequisites. From a Git checkout:

```sh
cargo build --locked --release -p ace-engine --bin outpace
```

The result is `target/release/outpace` or `target/release/outpace.exe`. A cross-build
also needs its target toolchain; use the existing release workflow and
[Linux portability guide](linux-portability.md) as references rather than assuming
`rustup target add` supplies a C linker.

For an offline rebuild of a published artifact, extract its matching complete
source archive and follow its `REBUILD.md`. It carries the exact workspace and
locked registry sources. Modified builds must provide their own matching source
bundle and notices; an earlier release's source archive cannot describe later local
changes. See the repository's [license and source guidance](../README.md).
