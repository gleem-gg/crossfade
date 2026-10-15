// Naming the driver's audio endpoints (PKEY_Device_DeviceDesc), shared by
// the `name-endpoints` command and the naming service.
#pragma once

#include <windows.h>
#include <string>

#include "crossfade.h"

// The default description of each Crossfade endpoint ("Crossfade Channel 3",
// "Crossfade Stream Mix", ...).
std::wstring DefaultEndpointName(const std::wstring& ref);

// Name the Crossfade endpoints. `channelNames[i]` is the wanted description
// of render slot i+1, or empty for its default; capture endpoints always get
// their default. Waits up to `waitMs` for all endpoints to appear. COM must
// be initialized on the calling thread. Returns the number of Crossfade
// endpoints found; `verbose` prints each rename.
int NameEndpoints(const std::wstring (&channelNames)[CF_RENDER_SLOTS], DWORD waitMs,
                  bool verbose);
