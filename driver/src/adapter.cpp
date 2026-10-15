// Crossfade virtual audio driver: adapter setup.
//
// A root-enumerated PortCls adapter without hardware. Each endpoint is a
// WaveRT filter plus a topology filter, joined by a physical connection:
//
//   capture:  [topo: mic bridge -> wave pin] ==> [wave: bridge -> host]
//   render:   [wave: host -> bridge] ==> [topo: wave pin -> speaker bridge]
//
// The two capture endpoints ("Crossfade Stream Mix", "Crossfade VOD Mix")
// are fed by the app through a private KS property; the render endpoints
// ("Crossfade Channel 1..8") discard their audio and are recorded by the
// app through WASAPI loopback.

// Instantiate the PortCls interface/class GUIDs (selectany) here.
#include <initguid.h>
#include "common.h"

// Endpoint descriptions live in the INF (MediaCategories); these GUIDs
// must match it.
const CF_ENDPOINT g_Endpoints[] = {
    {CfKind::Capture, CF_WAVE_STREAM, CF_TOPO_STREAM,
     {0xc3964751, 0x0836, 0x44a2, {0x9e, 0xe3, 0x05, 0xe0, 0x4e, 0x42, 0x1f, 0x00}}},
    {CfKind::Capture, CF_WAVE_VOD, CF_TOPO_VOD,
     {0xc3964751, 0x0836, 0x44a2, {0x9e, 0xe3, 0x05, 0xe0, 0x4e, 0x42, 0x1f, 0x01}}},
    {CfKind::Render, L"CfWaveCh1", L"CfTopoCh1",
     {0xc3964751, 0x0836, 0x44a2, {0x9e, 0xe3, 0x05, 0xe0, 0x4e, 0x42, 0x1f, 0x02}}},
    {CfKind::Render, L"CfWaveCh2", L"CfTopoCh2",
     {0xc3964751, 0x0836, 0x44a2, {0x9e, 0xe3, 0x05, 0xe0, 0x4e, 0x42, 0x1f, 0x03}}},
    {CfKind::Render, L"CfWaveCh3", L"CfTopoCh3",
     {0xc3964751, 0x0836, 0x44a2, {0x9e, 0xe3, 0x05, 0xe0, 0x4e, 0x42, 0x1f, 0x04}}},
    {CfKind::Render, L"CfWaveCh4", L"CfTopoCh4",
     {0xc3964751, 0x0836, 0x44a2, {0x9e, 0xe3, 0x05, 0xe0, 0x4e, 0x42, 0x1f, 0x05}}},
    {CfKind::Render, L"CfWaveCh5", L"CfTopoCh5",
     {0xc3964751, 0x0836, 0x44a2, {0x9e, 0xe3, 0x05, 0xe0, 0x4e, 0x42, 0x1f, 0x06}}},
    {CfKind::Render, L"CfWaveCh6", L"CfTopoCh6",
     {0xc3964751, 0x0836, 0x44a2, {0x9e, 0xe3, 0x05, 0xe0, 0x4e, 0x42, 0x1f, 0x07}}},
    {CfKind::Render, L"CfWaveCh7", L"CfTopoCh7",
     {0xc3964751, 0x0836, 0x44a2, {0x9e, 0xe3, 0x05, 0xe0, 0x4e, 0x42, 0x1f, 0x08}}},
    {CfKind::Render, L"CfWaveCh8", L"CfTopoCh8",
     {0xc3964751, 0x0836, 0x44a2, {0x9e, 0xe3, 0x05, 0xe0, 0x4e, 0x42, 0x1f, 0x09}}},
};
const ULONG g_EndpointCount = ARRAYSIZE(g_Endpoints);

// ---- Allocation ----------------------------------------------------------

PVOID operator new(size_t size, POOL_FLAGS flags, ULONG tag)
{
    // ExAllocatePool2 zero-initializes.
    return ExAllocatePool2(flags, size, tag);
}

// stdunk.lib provides the unsized operator delete; the sized one (used by
// the compiler-generated deleting destructors) forwards to it.
void __cdecl operator delete(PVOID p, size_t)
{
    operator delete(p);
}

// ---- Subdevices ------------------------------------------------------------

// Create a port, bind the miniport to it and register it under `name`.
static NTSTATUS InstallSubdevice(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_opt_ PIRP Irp,
    _In_ PCWSTR Name,
    _In_ REFGUID PortClassId,
    _In_ PUNKNOWN Miniport,
    _In_opt_ PRESOURCELIST ResourceList,
    _Out_ PUNKNOWN* OutPort)
{
    *OutPort = nullptr;
    PPORT port = nullptr;
    NTSTATUS status = PcNewPort(&port, PortClassId);
    if (!NT_SUCCESS(status)) {
        return status;
    }
    status = port->Init(DeviceObject, Irp, Miniport, nullptr, ResourceList);
    if (NT_SUCCESS(status)) {
        status = PcRegisterSubdevice(DeviceObject, const_cast<PWSTR>(Name), port);
    }
    if (NT_SUCCESS(status)) {
        *OutPort = port; // keeps the reference
    } else {
        CF_LOG("could not install subdevice %ws: 0x%08x", Name, status);
        port->Release();
    }
    return status;
}

static NTSTATUS InstallEndpoint(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_opt_ PIRP Irp,
    _In_opt_ PRESOURCELIST ResourceList,
    _In_ const CF_ENDPOINT* Endpoint)
{
    PUNKNOWN topoMiniport = nullptr;
    PUNKNOWN waveMiniport = nullptr;
    PUNKNOWN topoPort = nullptr;
    PUNKNOWN wavePort = nullptr;

    NTSTATUS status = CreateMiniportTopology(&topoMiniport, Endpoint);
    if (NT_SUCCESS(status)) {
        status = InstallSubdevice(DeviceObject, Irp, Endpoint->TopoName, CLSID_PortTopology,
                                  topoMiniport, ResourceList, &topoPort);
    }
    if (NT_SUCCESS(status)) {
        status = CreateMiniportWaveRT(&waveMiniport, Endpoint);
    }
    if (NT_SUCCESS(status)) {
        status = InstallSubdevice(DeviceObject, Irp, Endpoint->WaveName, CLSID_PortWaveRT,
                                  waveMiniport, ResourceList, &wavePort);
    }
    if (NT_SUCCESS(status)) {
        if (Endpoint->Kind == CfKind::Render) {
            status = PcRegisterPhysicalConnection(DeviceObject, wavePort, WAVE_PIN_BRIDGE,
                                                  topoPort, TOPO_PIN_WAVE);
        } else {
            status = PcRegisterPhysicalConnection(DeviceObject, topoPort, TOPO_PIN_WAVE,
                                                  wavePort, WAVE_PIN_BRIDGE);
        }
    }

    // The ports hold their own references to the miniports, and PortCls
    // holds the registered ports.
    if (topoMiniport) topoMiniport->Release();
    if (waveMiniport) waveMiniport->Release();
    if (topoPort) topoPort->Release();
    if (wavePort) wavePort->Release();
    return status;
}

_Function_class_(PCPFNSTARTDEVICE)
static NTSTATUS StartDevice(
    _In_ PDEVICE_OBJECT DeviceObject,
    _In_ PIRP Irp,
    _In_ PRESOURCELIST ResourceList)
{
    PAGED_CODE();
    for (ULONG i = 0; i < g_EndpointCount; i++) {
        NTSTATUS status = InstallEndpoint(DeviceObject, Irp, ResourceList, &g_Endpoints[i]);
        if (!NT_SUCCESS(status)) {
            return status;
        }
    }
    CF_LOG("started with %lu endpoints", g_EndpointCount);
    return STATUS_SUCCESS;
}

// ---- Driver entry ------------------------------------------------------------

_Function_class_(DRIVER_ADD_DEVICE)
static NTSTATUS AddDevice(_In_ PDRIVER_OBJECT DriverObject, _In_ PDEVICE_OBJECT PhysicalDeviceObject)
{
    PAGED_CODE();
    // Two subdevices (wave + topology) per endpoint.
    return PcAddAdapterDevice(DriverObject, PhysicalDeviceObject, StartDevice,
                              g_EndpointCount * 2, 0);
}

_Function_class_(DRIVER_UNLOAD)
static void DriverUnload(_In_ PDRIVER_OBJECT DriverObject)
{
    UNREFERENCED_PARAMETER(DriverObject);
}

extern "C" DRIVER_INITIALIZE DriverEntry;

extern "C" NTSTATUS DriverEntry(_In_ PDRIVER_OBJECT DriverObject, _In_ PUNICODE_STRING RegistryPath)
{
    NTSTATUS status = PcInitializeAdapterDriver(DriverObject, RegistryPath,
                                                (PDRIVER_ADD_DEVICE)AddDevice);
    if (NT_SUCCESS(status)) {
        DriverObject->DriverUnload = DriverUnload;
    }
    return status;
}
