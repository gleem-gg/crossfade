#include "endpoints.h"

#include <mmdeviceapi.h>
#include <devicetopology.h>
#include <functiondiscoverykeys_devpkey.h>
#include <propvarutil.h>
#include <cstdio>

#pragma comment(lib, "ole32.lib")
#pragma comment(lib, "propsys.lib")

// The KS reference string an endpoint's topology filter was registered
// under ("stream", "vod", "ch3"), lowercased; empty if it is not ours.
// Endpoints are recognized by this, never by their (renameable) name.
static std::wstring EndpointRef(const wchar_t* filterPath)
{
    std::wstring p = filterPath;
    for (auto& c : p) {
        c = towlower(c);
    }
    const std::wstring marker = L"\\cftopo";
    size_t at = p.rfind(marker);
    return at == std::wstring::npos ? std::wstring() : p.substr(at + marker.size());
}

// Render slot (1-based) of a reference string, or 0.
static int SlotOf(const std::wstring& ref)
{
    if (ref.size() == 3 && ref[0] == L'c' && ref[1] == L'h' && ref[2] >= L'1' &&
        ref[2] < L'1' + CF_RENDER_SLOTS) {
        return ref[2] - L'0';
    }
    return 0;
}

std::wstring DefaultEndpointName(const std::wstring& ref)
{
    if (ref == L"stream") {
        return L"Crossfade Stream Mix";
    }
    if (ref == L"vod") {
        return L"Crossfade VOD Mix";
    }
    if (int slot = SlotOf(ref)) {
        return L"Crossfade Channel " + std::to_wstring(slot);
    }
    return {};
}

// Set one endpoint's description unless it already has it.
static void NameEndpoint(IMMDevice* device, const std::wstring& name, bool verbose)
{
    IPropertyStore* store = nullptr;
    if (FAILED(device->OpenPropertyStore(STGM_READWRITE, &store))) {
        fwprintf(stderr, L"Could not open endpoint \"%s\" for writing.\n", name.c_str());
        return;
    }
    PROPVARIANT current;
    PropVariantInit(&current);
    bool same = SUCCEEDED(store->GetValue(PKEY_Device_DeviceDesc, &current)) &&
                current.vt == VT_LPWSTR && name == current.pwszVal;
    PropVariantClear(&current);
    PROPVARIANT value;
    if (!same && SUCCEEDED(InitPropVariantFromString(name.c_str(), &value))) {
        if (SUCCEEDED(store->SetValue(PKEY_Device_DeviceDesc, value)) &&
            SUCCEEDED(store->Commit())) {
            if (verbose) {
                wprintf(L"Named endpoint \"%s\".\n", name.c_str());
            }
        } else {
            fwprintf(stderr, L"Could not name endpoint \"%s\".\n", name.c_str());
        }
        PropVariantClear(&value);
    }
    store->Release();
}

int NameEndpoints(const std::wstring (&channelNames)[CF_RENDER_SLOTS], DWORD waitMs,
                  bool verbose)
{
    const int expected = CF_RENDER_SLOTS + 2;
    IMMDeviceEnumerator* enumerator = nullptr;
    if (FAILED(CoCreateInstance(__uuidof(MMDeviceEnumerator), nullptr, CLSCTX_ALL,
                                IID_PPV_ARGS(&enumerator)))) {
        fwprintf(stderr, L"Could not open the audio device list.\n");
        return 0;
    }
    int found = 0;
    ULONGLONG deadline = GetTickCount64() + waitMs;
    for (;;) {
        found = 0;
        IMMDeviceCollection* devices = nullptr;
        UINT count = 0;
        if (SUCCEEDED(enumerator->EnumAudioEndpoints(eAll, DEVICE_STATE_ACTIVE, &devices))) {
            devices->GetCount(&count);
        }
        for (UINT i = 0; i < count; i++) {
            IMMDevice* device = nullptr;
            if (FAILED(devices->Item(i, &device))) {
                continue;
            }
            IDeviceTopology* topology = nullptr;
            IConnector* connector = nullptr;
            LPWSTR filter = nullptr;
            std::wstring ref;
            if (SUCCEEDED(device->Activate(__uuidof(IDeviceTopology), CLSCTX_ALL, nullptr,
                                           reinterpret_cast<void**>(&topology))) &&
                SUCCEEDED(topology->GetConnector(0, &connector)) &&
                SUCCEEDED(connector->GetDeviceIdConnectedTo(&filter))) {
                ref = EndpointRef(filter);
            }
            CoTaskMemFree(filter);
            if (connector) connector->Release();
            if (topology) topology->Release();
            std::wstring name = DefaultEndpointName(ref);
            if (!name.empty()) {
                found++;
                int slot = SlotOf(ref);
                if (slot && !channelNames[slot - 1].empty()) {
                    name = channelNames[slot - 1];
                }
                NameEndpoint(device, name, verbose);
            }
            device->Release();
        }
        if (devices) devices->Release();
        if (found >= expected || GetTickCount64() >= deadline) {
            break;
        }
        Sleep(500);
    }
    enumerator->Release();
    return found;
}
