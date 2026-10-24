// Topology miniport: two pins and a straight connection — no volume or
// mute nodes, so Windows applies software volume on the endpoint.
//
// The bridge pin's category decides the endpoint form factor (speaker /
// microphone); its Name GUID decides the endpoint description.

#include "common.h"

static KSDATARANGE g_BridgeRanges[] = {
    {
        sizeof(KSDATARANGE), 0, 0, 0,
        STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
        STATICGUIDOF(KSDATAFORMAT_SUBTYPE_ANALOG),
        STATICGUIDOF(KSDATAFORMAT_SPECIFIER_NONE),
    },
};

static PKSDATARANGE g_BridgeRangePtrs[] = {&g_BridgeRanges[0]};

static const PCCONNECTION_DESCRIPTOR g_RenderConnections[] = {
    {PCFILTER_NODE, TOPO_PIN_WAVE, PCFILTER_NODE, TOPO_PIN_BRIDGE},
};

static const PCCONNECTION_DESCRIPTOR g_CaptureConnections[] = {
    {PCFILTER_NODE, TOPO_PIN_BRIDGE, PCFILTER_NODE, TOPO_PIN_WAVE},
};

static const GUID g_TopoCategories[] = {
    STATICGUIDOF(KSCATEGORY_AUDIO),
    STATICGUIDOF(KSCATEGORY_TOPOLOGY),
};

class CMiniportTopology : public IMiniportTopology, public CUnknown {
public:
    DECLARE_STD_UNKNOWN();

    CMiniportTopology(PUNKNOWN outer, const CF_ENDPOINT* endpoint)
        : CUnknown(outer), m_Endpoint(endpoint)
    {
        BuildDescriptor();
    }

    ~CMiniportTopology() = default;

    // IMiniport
    STDMETHODIMP_(NTSTATUS) GetDescription(_Out_ PPCFILTER_DESCRIPTOR* Description) override
    {
        *Description = &m_Filter;
        return STATUS_SUCCESS;
    }

    STDMETHODIMP_(NTSTATUS) DataRangeIntersection(
        _In_ ULONG, _In_ PKSDATARANGE, _In_ PKSDATARANGE, _In_ ULONG,
        _Out_writes_bytes_to_opt_(OutputBufferLength, *ResultantFormatLength) PVOID,
        _Out_ PULONG ResultantFormatLength) override
    {
        *ResultantFormatLength = 0;
        return STATUS_NOT_IMPLEMENTED; // PortCls' default handler
    }

    // IMiniportTopology
    STDMETHODIMP_(NTSTATUS) Init(_In_ PUNKNOWN, _In_ PRESOURCELIST, _In_ PPORTTOPOLOGY) override
    {
        return STATUS_SUCCESS;
    }

private:
    void BuildDescriptor()
    {
        const bool render = m_Endpoint->Kind == CfKind::Render;

        // Pin facing the wave filter: carries audio "into" the topology
        // for render, out of it for capture.
        KSPIN_DESCRIPTOR& wave = m_Pins[TOPO_PIN_WAVE].KsPinDescriptor;
        m_Pins[TOPO_PIN_WAVE].MaxGlobalInstanceCount = 0;
        m_Pins[TOPO_PIN_WAVE].MaxFilterInstanceCount = 0;
        wave.DataRangesCount = ARRAYSIZE(g_BridgeRangePtrs);
        wave.DataRanges = g_BridgeRangePtrs;
        wave.DataFlow = render ? KSPIN_DATAFLOW_IN : KSPIN_DATAFLOW_OUT;
        wave.Communication = KSPIN_COMMUNICATION_NONE;
        wave.Category = &KSCATEGORY_AUDIO;

        // The physical connector: what the endpoint is.
        KSPIN_DESCRIPTOR& bridge = m_Pins[TOPO_PIN_BRIDGE].KsPinDescriptor;
        bridge.DataRangesCount = ARRAYSIZE(g_BridgeRangePtrs);
        bridge.DataRanges = g_BridgeRangePtrs;
        bridge.DataFlow = render ? KSPIN_DATAFLOW_OUT : KSPIN_DATAFLOW_IN;
        bridge.Communication = KSPIN_COMMUNICATION_NONE;
        bridge.Category = render ? &KSNODETYPE_SPEAKER : &KSNODETYPE_MICROPHONE;
        bridge.Name = &m_Endpoint->PinName;

        m_Filter.Version = 0;
        m_Filter.AutomationTable = nullptr;
        m_Filter.PinSize = sizeof(PCPIN_DESCRIPTOR);
        m_Filter.PinCount = ARRAYSIZE(m_Pins);
        m_Filter.Pins = m_Pins;
        m_Filter.NodeSize = sizeof(PCNODE_DESCRIPTOR);
        m_Filter.NodeCount = 0;
        m_Filter.Nodes = nullptr;
        m_Filter.ConnectionCount = 1;
        m_Filter.Connections = render ? g_RenderConnections : g_CaptureConnections;
        m_Filter.CategoryCount = ARRAYSIZE(g_TopoCategories);
        m_Filter.Categories = g_TopoCategories;
    }

    const CF_ENDPOINT* m_Endpoint;
    PCPIN_DESCRIPTOR m_Pins[2] = {};
    PCFILTER_DESCRIPTOR m_Filter = {};
};

STDMETHODIMP_(NTSTATUS) CMiniportTopology::NonDelegatingQueryInterface(_In_ REFIID Interface, _COM_Outptr_ PVOID* Object)
{
    if (IsEqualGUIDAligned(Interface, IID_IUnknown)) {
        *Object = PVOID(PUNKNOWN(PMINIPORTTOPOLOGY(this)));
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniport)) {
        *Object = PVOID(PMINIPORT(this));
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniportTopology)) {
        *Object = PVOID(PMINIPORTTOPOLOGY(this));
    } else {
        *Object = nullptr;
        return STATUS_INVALID_PARAMETER;
    }
    PUNKNOWN(*Object)->AddRef();
    return STATUS_SUCCESS;
}

NTSTATUS CreateMiniportTopology(_Out_ PUNKNOWN* Unknown, _In_ const CF_ENDPOINT* Endpoint)
{
    auto* obj = new (POOL_FLAG_NON_PAGED, CF_POOLTAG) CMiniportTopology(nullptr, Endpoint);
    if (!obj) {
        *Unknown = nullptr;
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    *Unknown = PUNKNOWN(PMINIPORTTOPOLOGY(obj));
    (*Unknown)->AddRef();
    return STATUS_SUCCESS;
}
