// Contract between the Crossfade virtual audio driver and the app
// (src/audio/wasapi/driver.rs). Keep both sides in sync.
#pragma once

// Root-enumerated hardware id the INF and setup tool use.
#define CF_HARDWARE_ID L"ROOT\\CrossfadeAudio"

// The mix format the app pushes: interleaved stereo, signed 32-bit
// integer PCM (full scale), 48 kHz. Integer so the kernel side never
// touches floating point.
#define CF_RATE 48000
#define CF_PUSH_CHANNELS 2

// KSPROPSETID_Crossfade {9C877548-3CF8-4138-88E2-C2D6DFB6B26B}
#define STATIC_KSPROPSETID_Crossfade \
    0x9c877548L, 0x3cf8, 0x4138, 0x88, 0xe2, 0xc2, 0xd6, 0xdf, 0xb6, 0xb2, 0x6b

// SET on a capture wave filter: append frames to its ring buffer. The
// property value is the PCM data (a whole number of frames).
#define KSPROPERTY_CROSSFADE_PUSH 1

// Number of "Crossfade Channel N" render endpoints.
#define CF_RENDER_SLOTS 8

// KS subdevice (reference string) names. The app finds the capture wave
// filters by these, and recognizes our endpoints by the topology filter
// they hang off (case-insensitive).
#define CF_WAVE_STREAM L"CfWaveStream"
#define CF_WAVE_VOD    L"CfWaveVod"
#define CF_TOPO_STREAM L"CfTopoStream"
#define CF_TOPO_VOD    L"CfTopoVod"
// Render slots: CfWaveCh1..8 / CfTopoCh1..8

// Endpoint naming service (driver/setup). Renaming an audio endpoint needs
// administrator rights, so the unprivileged app asks this LocalSystem
// service to name the "Crossfade Channel N" outputs after its channels.
//
// Protocol: the app connects to the pipe, writes one UTF-8 message of
// lines "<slot>\t<name>\n" (slot 1..CF_RENDER_SLOTS; an empty name restores
// the default "Crossfade Channel <slot>"), and reads back "ok\n" or
// "error\n". Messages are at most CF_NAMER_MAX_MESSAGE bytes.
#define CF_NAMER_SERVICE L"CrossfadeEndpointNames"
#define CF_NAMER_PIPE L"\\\\.\\pipe\\GleemCrossfadeEndpointNames"
#define CF_NAMER_MAX_MESSAGE 4096
#define CF_NAMER_MAX_NAME 64
