// Shared declarations for the Crossfade virtual audio driver.
#pragma once

#include <portcls.h>
// This driver defines its own pool-tagged operators (adapter.cpp).
#define _NEW_DELETE_OPERATORS_
#include <stdunk.h>
#include <ksdebug.h>
#include <ntstrsafe.h>

#include "crossfade.h"

#define CF_POOLTAG 'fdfC'

#define CF_LOG(fmt, ...) \
    DbgPrintEx(DPFLTR_IHVAUDIO_ID, DPFLTR_INFO_LEVEL, "Crossfade: " fmt "\n", __VA_ARGS__)

// Pool-tagged allocation for the CUnknown-based objects (zeroed);
// stdunk.lib provides the matching operator delete.
PVOID operator new(size_t size, POOL_FLAGS flags, ULONG tag);

enum class CfKind { Render, Capture };

// One endpoint = one wave filter + one topology filter.
struct CF_ENDPOINT {
    CfKind Kind;
    PCWSTR WaveName;
    PCWSTR TopoName;
    // Name GUID of the topology bridge pin; the INF maps it to the
    // endpoint description ("Crossfade Stream Mix", …) under
    // MediaCategories.
    GUID PinName;
};

extern const CF_ENDPOINT g_Endpoints[];
extern const ULONG g_EndpointCount;

// Wave filter pins.
#define WAVE_PIN_HOST   0 // the stream pin audiodg connects to
#define WAVE_PIN_BRIDGE 1 // connected to the topology filter
// Topology filter pins.
#define TOPO_PIN_WAVE   0 // connected to the wave filter
#define TOPO_PIN_BRIDGE 1 // the physical connector (speaker/microphone)

NTSTATUS CreateMiniportWaveRT(_Out_ PUNKNOWN* Unknown, _In_ const CF_ENDPOINT* Endpoint);
NTSTATUS CreateMiniportTopology(_Out_ PUNKNOWN* Unknown, _In_ const CF_ENDPOINT* Endpoint);
