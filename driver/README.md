# Crossfade virtual audio driver (Windows)

A small PortCls/WaveRT kernel driver that gives Crossfade on Windows the
virtual devices PipeWire provides on Linux:

| Endpoint | Kind | Purpose |
| --- | --- | --- |
| Crossfade Stream Mix | microphone | the Stream Mix, for OBS/Discord |
| Crossfade VOD Mix | microphone | the optional VOD Mix |
| Crossfade Channel 1 … 8 | speakers | one per mixer channel; apps play into them |

The microphones are fed by the app through a private KS property
(`KSPROPSETID_Crossfade`, see `inc/crossfade.h`); the speaker endpoints discard
their audio, which the app records through ordinary WASAPI loopback. All
endpoints are clocked by the performance counter, the same clock the app's
mix engine runs on, so the microphones never drift against the mix.

## Building

Requirements: Visual Studio 2022 or its Build Tools with the C++ workload,
the Spectre-mitigated libraries, and the Windows Driver Kit (WDK) component.

```powershell
.\driver\build.ps1                 # Release x64
.\driver\build.ps1 -Platform ARM64 # needs the ARM64 C++ tools
```

The package — `CrossfadeAudio.sys`, `.inf`, `.cat`, `crossfade-driver-setup.exe`
and the test certificate `CrossfadeAudio.cer` — lands in
`driver\build\<Platform>\<Configuration>\CrossfadeAudio\`.

Every build stamps a distinct `DriverVer` (`0.9.<yyMM>.<minute of the
month>`, UTC). Windows' driver store identifies a package by its INF, so
reinstalling a rebuild with an identical INF reports success but keeps the
old binary running.

Check the INF separately (the WDK's in-build check fails to load with Build
Tools only):

```powershell
& "${env:ProgramFiles(x86)}\Windows Kits\10\Tools\10.0.26100.0\x64\infverif.exe" /v driver\build\x64\Release\CrossfadeAudio\CrossfadeAudio.inf
```

## Installing a development build

Development builds are signed with the WDK test certificate, which Windows
only accepts in test-signing mode. On a **test machine** (Secure Boot must
be off for test signing):

1. Enable test signing from an elevated prompt and reboot:
   `bcdedit /set testsigning on`
2. Trust the test certificate (elevated):
   ```powershell
   Import-Certificate .\CrossfadeAudio.cer -CertStoreLocation Cert:\LocalMachine\Root
   Import-Certificate .\CrossfadeAudio.cer -CertStoreLocation Cert:\LocalMachine\TrustedPublisher
   ```
3. Install: `.\crossfade-driver-setup.exe install` (asks for elevation;
   creates the `ROOT\CrossfadeAudio` device, binds the driver, names the
   endpoints and sets up the naming service, below).

Remove everything again with `.\crossfade-driver-setup.exe uninstall`.

### Endpoint names

Windows names speaker-type endpoints generically, so all eight outputs
would appear as "Speakers". The setup tool names them "Crossfade Channel
1…8" after installing; `crossfade-driver-setup name-endpoints` resets them
to those names.

Renaming an endpoint needs administrator rights, so the app cannot name the
outputs after its channels itself. The **naming service**
("Gleem Crossfade Device Names", `CrossfadeEndpointNames`) does it for the
app: a LocalSystem service, installed to
`%ProgramFiles%\Gleem Crossfade\crossfade-driver-setup.exe` and run as
`… service`, that listens on `\\.\pipe\GleemCrossfadeEndpointNames`. The
app sends the wanted name of every slot ("Crossfade: Game"; empty = the
default) whenever Windows' names differ from its channels. The service
only ever renames the driver's own render endpoints (recognized by their
KS filter, not their name), cleans and caps names at 64 characters, and
drops clients that stall. `install-service` adds it to an existing install;
`uninstall` removes it. Windows' Sound settings may show new names only
after being reopened.

To run the app without the driver (monitor mix only — the virtual
microphones and channel devices will be missing), set
`CROSSFADE_ALLOW_NO_DRIVER=1`.

## Shipping

End users cannot enable test signing. A release driver must be signed by
Microsoft: register for the Windows Hardware Developer Program (needs an EV
code-signing certificate), then submit the package for **attestation
signing** through Partner Center. Open points before submitting:

- `infverif /w` (strict "Windows Driver" mode) rejects the HKLM
  `MediaCategories` entries that name the endpoints; attestation of a
  desktop driver accepts them, but a componentized/universal INF would need
  a different naming mechanism.
- ARM64 builds need the ARM64 C++ build tools.

## Layout

- `inc/crossfade.h` — the contract shared with `src/audio/wasapi/driver.rs`
- `src/adapter.cpp` — DriverEntry, device start, subdevice registration
- `src/mintopo.cpp` — topology miniport (bridge pins name the endpoints)
- `src/minwave.cpp` — WaveRT miniport/stream, capture ring buffer, push property
- `CrossfadeAudio.inf` — endpoints' KS interfaces and names
- `setup/` — `crossfade-driver-setup.exe`: device creation and driver
  install (`setup.cpp`), endpoint naming (`endpoints.cpp`), the naming
  service (`service.cpp`)
