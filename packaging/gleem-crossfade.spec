Name:           gleem-crossfade
Version:        0.8.0
Release:        1%{?dist}
Summary:        Dual-mix virtual audio mixer for PipeWire
License:        MIT
URL:            https://github.com/gleem-gg/crossfade
Source0:        %{url}/archive/v%{version}/%{name}-%{version}.tar.gz
# Created with `cargo vendor` (see .copr/Makefile in the repository)
Source1:        %{name}-%{version}-vendor.tar.xz

BuildRequires:  cargo
BuildRequires:  rust >= 1.85
BuildRequires:  gcc
BuildRequires:  gtk4-devel >= 4.18
BuildRequires:  libadwaita-devel >= 1.8
BuildRequires:  pulseaudio-libs-devel
# MIDI controller support (ALSA sequencer)
BuildRequires:  alsa-lib-devel
BuildRequires:  desktop-file-utils

# The PulseAudio compatibility layer of PipeWire is the API Crossfade talks to
Requires:       pipewire-pulseaudio
# pw-cli / pw-link, used to wire and control effect chains
Recommends:     pipewire-utils
# LV2 effect browser (chains still load without it)
Recommends:     lilv
# VST2/VST3 effect hosting
Suggests:       Carla

# The app was called "openwave" before 0.8.0
Obsoletes:      openwave < 0.8.0
Provides:       openwave = %{version}-%{release}

%description
Gleem Crossfade is a dual-mix virtual audio mixer for Linux. Every input
channel has two independent faders: a Monitor Mix (what you hear) and a
Stream Mix, exposed as a virtual microphone "Crossfade Stream Mix" for
OBS, Discord, or any other application. Channels can capture microphones,
application playback streams, or act as virtual output devices, with
optional per-channel LV2 and VST2/VST3 effect chains, MIDI controller
support with MIDI learn, and a D-Bus control API for scripting.

%prep
%autosetup
tar -xf %{SOURCE1}
mkdir -p .cargo
cat > .cargo/config.toml <<'EOF'
[source.crates-io]
replace-with = "vendored-sources"

[source.vendored-sources]
directory = "vendor"

[net]
offline = true
EOF

%build
cargo build --release --locked

%install
install -Dm755 target/release/gleem-crossfade %{buildroot}%{_bindir}/gleem-crossfade
install -Dm644 data/gg.gleem.Crossfade.desktop \
    %{buildroot}%{_datadir}/applications/gg.gleem.Crossfade.desktop
install -Dm644 data/icons/hicolor/scalable/apps/gg.gleem.Crossfade.svg \
    %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/gg.gleem.Crossfade.svg

%check
desktop-file-validate %{buildroot}%{_datadir}/applications/gg.gleem.Crossfade.desktop

%files
%license LICENSE
%doc README.md
%{_bindir}/gleem-crossfade
%{_datadir}/applications/gg.gleem.Crossfade.desktop
%{_datadir}/icons/hicolor/scalable/apps/gg.gleem.Crossfade.svg

%changelog
* Wed Jul 29 2026 René Preuß <hello@ghostzero.de> - 0.8.0-1
- Renamed to Gleem Crossfade: new binary gleem-crossfade, app id
  gg.gleem.Crossfade, D-Bus API gg.gleem.Crossfade.Mixer1, virtual
  microphones "Crossfade Stream Mix" / "Crossfade VOD Mix"
- Existing OpenWave configuration is migrated automatically on first start

* Wed Jul 22 2026 René Preuß <hello@ghostzero.de> - 0.7.0-1
- MIDI controller support: MIDI learn on every fader and mute, binding
  profiles switchable from pads, LED feedback, fader pickup, hotplug
- D-Bus control API (de.ghostzero.OpenWave.Mixer1) for scripting,
  hotkeys and external controllers
- New build dependency: alsa-lib-devel

* Tue Jul 21 2026 René Preuß <hello@ghostzero.de> - 0.6.0-1
- Optional VOD Mix: third bus and "Virtual VOD Mix" microphone for a
  VOD-safe second recording track

* Mon Jul 20 2026 René Preuß <hello@ghostzero.de> - 0.5.0-1
- Initial package
