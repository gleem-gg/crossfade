// WaveRT miniport and stream.
//
// No hardware: a stream's position is derived from the performance counter
// (the same clock the app mixes on, so the virtual microphones never drift
// against the mix). Capture streams copy pushed audio from the filter's
// ring buffer into the WaveRT buffer up to the current position whenever
// the audio engine asks for the position; render streams just advance
// (their audio is recorded by the app through WASAPI loopback).
//
// Only integer PCM device formats are offered (16/24/32-bit, mono or
// stereo, 48 kHz), so converting the pushed s32 frames needs no floating
// point in kernel mode.

#include "common.h"

static const GUID KSPROPSETID_Crossfade = {STATIC_KSPROPSETID_Crossfade};

// Ring of pushed frames (s32 stereo) per capture filter: 1 s.
#define RING_FRAMES CF_RATE
// After (re)start or a backlog, keep this much queued (10 ms): the app
// pushes every ~5 ms, the audio engine reads every ~10 ms.
#define RING_TARGET (CF_RATE / 100)
// Backlog beyond which the reader skips ahead to RING_TARGET (100 ms).
#define RING_MAX_BACKLOG (CF_RATE / 10)

// ---- Data ranges --------------------------------------------------------------

static KSDATARANGE_AUDIO g_HostRanges[] = {
    {
        {
            sizeof(KSDATARANGE_AUDIO), 0, 0, 0,
            STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
            STATICGUIDOF(KSDATAFORMAT_SUBTYPE_PCM),
            STATICGUIDOF(KSDATAFORMAT_SPECIFIER_WAVEFORMATEX),
        },
        2,      // MaximumChannels
        16,     // MinimumBitsPerSample
        32,     // MaximumBitsPerSample
        CF_RATE, // MinimumSampleFrequency
        CF_RATE, // MaximumSampleFrequency
    },
};

static PKSDATARANGE g_HostRangePtrs[] = {PKSDATARANGE(&g_HostRanges[0])};

static KSDATARANGE g_BridgeRanges[] = {
    {
        sizeof(KSDATARANGE), 0, 0, 0,
        STATICGUIDOF(KSDATAFORMAT_TYPE_AUDIO),
        STATICGUIDOF(KSDATAFORMAT_SUBTYPE_ANALOG),
        STATICGUIDOF(KSDATAFORMAT_SPECIFIER_NONE),
    },
};

static PKSDATARANGE g_BridgeRangePtrs[] = {&g_BridgeRanges[0]};

// ---- Filter descriptors -----------------------------------------------------------

static NTSTATUS PropertyHandlerPush(_In_ PPCPROPERTY_REQUEST Request);

static PCPROPERTY_ITEM g_CaptureFilterProperties[] = {
    {
        &KSPROPSETID_Crossfade,
        KSPROPERTY_CROSSFADE_PUSH,
        KSPROPERTY_TYPE_SET | KSPROPERTY_TYPE_BASICSUPPORT,
        PropertyHandlerPush,
    },
};

DEFINE_PCAUTOMATION_TABLE_PROP(g_CaptureFilterAutomation, g_CaptureFilterProperties);

static PCPIN_DESCRIPTOR g_RenderPins[] = {
    // WAVE_PIN_HOST
    {
        1, 1, 0, nullptr,
        {
            0, nullptr, 0, nullptr,
            ARRAYSIZE(g_HostRangePtrs), g_HostRangePtrs,
            KSPIN_DATAFLOW_IN,
            KSPIN_COMMUNICATION_SINK,
            &KSCATEGORY_AUDIO,
            nullptr,
            0,
        },
    },
    // WAVE_PIN_BRIDGE
    {
        0, 0, 0, nullptr,
        {
            0, nullptr, 0, nullptr,
            ARRAYSIZE(g_BridgeRangePtrs), g_BridgeRangePtrs,
            KSPIN_DATAFLOW_OUT,
            KSPIN_COMMUNICATION_NONE,
            &KSCATEGORY_AUDIO,
            nullptr,
            0,
        },
    },
};

static PCPIN_DESCRIPTOR g_CapturePins[] = {
    // WAVE_PIN_HOST
    {
        1, 1, 0, nullptr,
        {
            0, nullptr, 0, nullptr,
            ARRAYSIZE(g_HostRangePtrs), g_HostRangePtrs,
            KSPIN_DATAFLOW_OUT,
            KSPIN_COMMUNICATION_SINK,
            &KSCATEGORY_AUDIO,
            nullptr,
            0,
        },
    },
    // WAVE_PIN_BRIDGE
    {
        0, 0, 0, nullptr,
        {
            0, nullptr, 0, nullptr,
            ARRAYSIZE(g_BridgeRangePtrs), g_BridgeRangePtrs,
            KSPIN_DATAFLOW_IN,
            KSPIN_COMMUNICATION_NONE,
            &KSCATEGORY_AUDIO,
            nullptr,
            0,
        },
    },
};

// One converter node between host and bridge pin (DAC for render, ADC
// for capture), as the audio endpoint builder expects of a wave filter.
static PCNODE_DESCRIPTOR g_RenderNodes[] = {
    {0, nullptr, &KSNODETYPE_DAC, nullptr},
};

static PCNODE_DESCRIPTOR g_CaptureNodes[] = {
    {0, nullptr, &KSNODETYPE_ADC, nullptr},
};

static PCCONNECTION_DESCRIPTOR g_RenderConnections[] = {
    {PCFILTER_NODE, WAVE_PIN_HOST, 0, 1},   // host -> DAC in
    {0, 0, PCFILTER_NODE, WAVE_PIN_BRIDGE}, // DAC out -> bridge
};

static PCCONNECTION_DESCRIPTOR g_CaptureConnections[] = {
    {PCFILTER_NODE, WAVE_PIN_BRIDGE, 0, 1}, // bridge -> ADC in
    {0, 0, PCFILTER_NODE, WAVE_PIN_HOST},   // ADC out -> host
};

static const GUID g_RenderCategories[] = {
    STATICGUIDOF(KSCATEGORY_AUDIO),
    STATICGUIDOF(KSCATEGORY_RENDER),
    STATICGUIDOF(KSCATEGORY_REALTIME),
};

static const GUID g_CaptureCategories[] = {
    STATICGUIDOF(KSCATEGORY_AUDIO),
    STATICGUIDOF(KSCATEGORY_CAPTURE),
    STATICGUIDOF(KSCATEGORY_REALTIME),
};

static PCFILTER_DESCRIPTOR g_RenderFilter = {
    0,
    nullptr,
    sizeof(PCPIN_DESCRIPTOR),
    ARRAYSIZE(g_RenderPins),
    g_RenderPins,
    sizeof(PCNODE_DESCRIPTOR),
    ARRAYSIZE(g_RenderNodes),
    g_RenderNodes,
    ARRAYSIZE(g_RenderConnections),
    g_RenderConnections,
    ARRAYSIZE(g_RenderCategories),
    g_RenderCategories,
};

static PCFILTER_DESCRIPTOR g_CaptureFilter = {
    0,
    &g_CaptureFilterAutomation,
    sizeof(PCPIN_DESCRIPTOR),
    ARRAYSIZE(g_CapturePins),
    g_CapturePins,
    sizeof(PCNODE_DESCRIPTOR),
    ARRAYSIZE(g_CaptureNodes),
    g_CaptureNodes,
    ARRAYSIZE(g_CaptureConnections),
    g_CaptureConnections,
    ARRAYSIZE(g_CaptureCategories),
    g_CaptureCategories,
};

// ---- Format helpers -------------------------------------------------------------------

struct StreamFormat {
    ULONG Channels;
    ULONG BytesPerSample; // container size: 2, 3 or 4
    ULONG BlockAlign;
};

static bool ParseFormat(_In_ PKSDATAFORMAT Format, _Out_ StreamFormat* Out)
{
    if (Format->FormatSize < sizeof(KSDATAFORMAT_WAVEFORMATEX) ||
        !IsEqualGUIDAligned(Format->MajorFormat, KSDATAFORMAT_TYPE_AUDIO) ||
        !IsEqualGUIDAligned(Format->Specifier, KSDATAFORMAT_SPECIFIER_WAVEFORMATEX)) {
        return false;
    }
    auto* wfx = &reinterpret_cast<PKSDATAFORMAT_WAVEFORMATEX>(Format)->WaveFormatEx;
    bool pcm = wfx->wFormatTag == WAVE_FORMAT_PCM;
    if (wfx->wFormatTag == WAVE_FORMAT_EXTENSIBLE) {
        if (Format->FormatSize < sizeof(KSDATAFORMAT) + sizeof(WAVEFORMATEXTENSIBLE)) {
            return false;
        }
        auto* ext = reinterpret_cast<PWAVEFORMATEXTENSIBLE>(wfx);
        pcm = IsEqualGUIDAligned(ext->SubFormat, KSDATAFORMAT_SUBTYPE_PCM);
    }
    if (!pcm || wfx->nSamplesPerSec != CF_RATE || wfx->nChannels < 1 || wfx->nChannels > 2) {
        return false;
    }
    if (wfx->wBitsPerSample != 16 && wfx->wBitsPerSample != 24 && wfx->wBitsPerSample != 32) {
        return false;
    }
    Out->Channels = wfx->nChannels;
    Out->BytesPerSample = wfx->wBitsPerSample / 8;
    Out->BlockAlign = Out->Channels * Out->BytesPerSample;
    return wfx->nBlockAlign == Out->BlockAlign;
}

// Write one s32 sample in the stream's container format.
static __forceinline void StoreSample(PBYTE dst, LONG s, ULONG bytes)
{
    switch (bytes) {
    case 2:
        *reinterpret_cast<SHORT*>(dst) = SHORT(s >> 16);
        break;
    case 3:
        dst[0] = BYTE(s >> 8);
        dst[1] = BYTE(s >> 16);
        dst[2] = BYTE(s >> 24);
        break;
    default:
        *reinterpret_cast<LONG*>(dst) = s;
        break;
    }
}

// ---- Miniport ------------------------------------------------------------------------------

class CMiniportWaveRTStream;

class CMiniportWaveRT : public IMiniportWaveRT, public CUnknown {
public:
    DECLARE_STD_UNKNOWN();

    CMiniportWaveRT(PUNKNOWN outer, const CF_ENDPOINT* endpoint)
        : CUnknown(outer), m_Endpoint(endpoint)
    {
        KeInitializeSpinLock(&m_RingLock);
    }

    ~CMiniportWaveRT()
    {
        if (m_Ring) {
            ExFreePoolWithTag(m_Ring, CF_POOLTAG);
        }
    }

    NTSTATUS AllocateRing()
    {
        if (m_Endpoint->Kind != CfKind::Capture) {
            return STATUS_SUCCESS;
        }
        m_Ring = static_cast<LONG*>(ExAllocatePool2(
            POOL_FLAG_NON_PAGED, RING_FRAMES * CF_PUSH_CHANNELS * sizeof(LONG), CF_POOLTAG));
        return m_Ring ? STATUS_SUCCESS : STATUS_INSUFFICIENT_RESOURCES;
    }

    // IMiniport
    STDMETHODIMP_(NTSTATUS) GetDescription(_Out_ PPCFILTER_DESCRIPTOR* Description) override
    {
        *Description = m_Endpoint->Kind == CfKind::Render ? &g_RenderFilter : &g_CaptureFilter;
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

    // IMiniportWaveRT
    STDMETHODIMP_(NTSTATUS) Init(_In_ PUNKNOWN, _In_ PRESOURCELIST, _In_ PPORTWAVERT) override
    {
        return AllocateRing();
    }

    STDMETHODIMP_(NTSTATUS) NewStream(
        _Out_ PMINIPORTWAVERTSTREAM* Stream,
        _In_ PPORTWAVERTSTREAM PortStream,
        _In_ ULONG Pin,
        _In_ BOOLEAN Capture,
        _In_ PKSDATAFORMAT DataFormat) override;

    STDMETHODIMP_(NTSTATUS) GetDeviceDescription(_Out_ PDEVICE_DESCRIPTION Description) override
    {
        RtlZeroMemory(Description, sizeof(DEVICE_DESCRIPTION));
        Description->Master = TRUE;
        Description->ScatterGather = TRUE;
        Description->Dma32BitAddresses = TRUE;
        Description->InterfaceType = PCIBus;
        Description->MaximumLength = 0xFFFFFFFF;
        return STATUS_SUCCESS;
    }

    // ---- Ring buffer (capture filters) ----

    // Append pushed s32 stereo frames; the oldest audio is overwritten
    // when the reader is not keeping up.
    void Push(_In_reads_(frames * CF_PUSH_CHANNELS) const LONG* data, ULONG frames)
    {
        if (!m_Ring) {
            return;
        }
        if (frames > RING_FRAMES) {
            data += (frames - RING_FRAMES) * CF_PUSH_CHANNELS;
            frames = RING_FRAMES;
        }
        KIRQL irql;
        KeAcquireSpinLock(&m_RingLock, &irql);
        for (ULONG i = 0; i < frames; i++) {
            ULONG at = (m_RingWrite % RING_FRAMES) * CF_PUSH_CHANNELS;
            m_Ring[at] = data[i * CF_PUSH_CHANNELS];
            m_Ring[at + 1] = data[i * CF_PUSH_CHANNELS + 1];
            m_RingWrite++;
        }
        if (m_RingWrite - m_RingRead > RING_FRAMES) {
            m_RingRead = m_RingWrite - RING_FRAMES;
        }
        KeReleaseSpinLock(&m_RingLock, irql);
    }

    // Drop backlog so a starting stream begins close to "now".
    void Resync()
    {
        KIRQL irql;
        KeAcquireSpinLock(&m_RingLock, &irql);
        if (m_RingWrite - m_RingRead > RING_TARGET) {
            m_RingRead = m_RingWrite - RING_TARGET;
        }
        KeReleaseSpinLock(&m_RingLock, irql);
    }

    // Read `frames` frames into a WaveRT buffer region, converting to the
    // stream format; missing frames (underrun) become silence.
    void Pull(_Out_writes_bytes_(frames * fmt.BlockAlign) PBYTE dst, ULONG frames,
              const StreamFormat& fmt)
    {
        KIRQL irql;
        KeAcquireSpinLock(&m_RingLock, &irql);
        if (m_RingWrite - m_RingRead > RING_MAX_BACKLOG) {
            m_RingRead = m_RingWrite - RING_TARGET;
        }
        for (ULONG i = 0; i < frames; i++, dst += fmt.BlockAlign) {
            LONG l = 0, r = 0;
            if (m_Ring && m_RingRead != m_RingWrite) {
                ULONG at = (m_RingRead % RING_FRAMES) * CF_PUSH_CHANNELS;
                l = m_Ring[at];
                r = m_Ring[at + 1];
                m_RingRead++;
            }
            if (fmt.Channels == 1) {
                // Average without overflowing.
                StoreSample(dst, (l >> 1) + (r >> 1), fmt.BytesPerSample);
            } else {
                StoreSample(dst, l, fmt.BytesPerSample);
                StoreSample(dst + fmt.BytesPerSample, r, fmt.BytesPerSample);
            }
        }
        KeReleaseSpinLock(&m_RingLock, irql);
    }

    const CF_ENDPOINT* Endpoint() const { return m_Endpoint; }

private:
    const CF_ENDPOINT* m_Endpoint;
    KSPIN_LOCK m_RingLock;
    LONG* m_Ring = nullptr;
    // Monotonic frame counters; the ring index is the counter modulo
    // RING_FRAMES.
    ULONGLONG m_RingWrite = 0;
    ULONGLONG m_RingRead = 0;
};

STDMETHODIMP_(NTSTATUS) CMiniportWaveRT::NonDelegatingQueryInterface(_In_ REFIID Interface, _COM_Outptr_ PVOID* Object)
{
    if (IsEqualGUIDAligned(Interface, IID_IUnknown)) {
        *Object = PVOID(PUNKNOWN(PMINIPORTWAVERT(this)));
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniport)) {
        *Object = PVOID(PMINIPORT(this));
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniportWaveRT)) {
        *Object = PVOID(PMINIPORTWAVERT(this));
    } else {
        *Object = nullptr;
        return STATUS_INVALID_PARAMETER;
    }
    PUNKNOWN(*Object)->AddRef();
    return STATUS_SUCCESS;
}

// ---- Stream -------------------------------------------------------------------------------

class CMiniportWaveRTStream : public IMiniportWaveRTStream, public CUnknown {
public:
    DECLARE_STD_UNKNOWN();

    CMiniportWaveRTStream(PUNKNOWN outer) : CUnknown(outer) {}

    ~CMiniportWaveRTStream()
    {
        if (m_Miniport) {
            m_Miniport->Release();
        }
        if (m_PortStream) {
            m_PortStream->Release();
        }
    }

    NTSTATUS Init(CMiniportWaveRT* miniport, PPORTWAVERTSTREAM portStream, bool capture,
                  const StreamFormat& format)
    {
        m_Miniport = miniport;
        m_Miniport->AddRef();
        m_PortStream = portStream;
        m_PortStream->AddRef();
        m_Capture = capture;
        m_Format = format;
        KeInitializeSpinLock(&m_Lock);
        return STATUS_SUCCESS;
    }

    // IMiniportWaveRTStream
    STDMETHODIMP_(NTSTATUS) SetFormat(_In_ PKSDATAFORMAT) override
    {
        return STATUS_NOT_SUPPORTED;
    }

    STDMETHODIMP_(NTSTATUS) SetState(_In_ KSSTATE State) override
    {
        KIRQL irql;
        KeAcquireSpinLock(&m_Lock, &irql);
        if (State == KSSTATE_RUN && m_State != KSSTATE_RUN) {
            m_RunStart = KeQueryPerformanceCounter(nullptr).QuadPart;
            m_RunBaseFrames = m_Frames;
            if (m_Capture) {
                m_Miniport->Resync();
            }
        } else if (State != KSSTATE_RUN && m_State == KSSTATE_RUN) {
            Advance(); // freeze the position where it is
        }
        if (State == KSSTATE_STOP) {
            m_Frames = 0;
            m_RunBaseFrames = 0;
        }
        m_State = State;
        KeReleaseSpinLock(&m_Lock, irql);
        return STATUS_SUCCESS;
    }

    STDMETHODIMP_(NTSTATUS) GetPosition(_Out_ PKSAUDIO_POSITION Position) override
    {
        KIRQL irql;
        KeAcquireSpinLock(&m_Lock, &irql);
        if (m_State == KSSTATE_RUN) {
            Advance();
        }
        ULONGLONG bytes = m_BufferSize ? (m_Frames * m_Format.BlockAlign) % m_BufferSize : 0;
        Position->PlayOffset = bytes;
        Position->WriteOffset = bytes;
        KeReleaseSpinLock(&m_Lock, irql);
        return STATUS_SUCCESS;
    }

    STDMETHODIMP_(NTSTATUS) AllocateAudioBuffer(
        _In_ ULONG RequestedSize,
        _Out_ PMDL* AudioBufferMdl,
        _Out_ ULONG* ActualSize,
        _Out_ ULONG* OffsetFromFirstPage,
        _Out_ MEMORY_CACHING_TYPE* CacheType) override
    {
        ULONG size = RequestedSize - RequestedSize % m_Format.BlockAlign;
        if (size == 0) {
            return STATUS_INVALID_PARAMETER;
        }
        PHYSICAL_ADDRESS high;
        high.QuadPart = MAXLONGLONG;
        PMDL mdl = m_PortStream->AllocatePagesForMdl(high, size);
        if (!mdl) {
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        // The allocation may come back short; the buffer must cover all of
        // `size`, which Advance() writes up to.
        if (MmGetMdlByteCount(mdl) < size) {
            m_PortStream->FreePagesFromMdl(mdl);
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        PBYTE buffer = static_cast<PBYTE>(m_PortStream->MapAllocatedPages(mdl, MmCached));
        if (!buffer) {
            m_PortStream->FreePagesFromMdl(mdl);
            return STATUS_INSUFFICIENT_RESOURCES;
        }
        RtlZeroMemory(buffer, size);
        KIRQL irql;
        KeAcquireSpinLock(&m_Lock, &irql);
        m_Buffer = buffer;
        m_BufferSize = size;
        KeReleaseSpinLock(&m_Lock, irql);
        *AudioBufferMdl = mdl;
        *ActualSize = size;
        *OffsetFromFirstPage = 0;
        *CacheType = MmCached;
        return STATUS_SUCCESS;
    }

    STDMETHODIMP_(VOID) FreeAudioBuffer(_In_opt_ PMDL Mdl, _In_ ULONG) override
    {
        PBYTE buffer;
        KIRQL irql;
        KeAcquireSpinLock(&m_Lock, &irql);
        buffer = m_Buffer;
        m_Buffer = nullptr;
        m_BufferSize = 0;
        KeReleaseSpinLock(&m_Lock, irql);
        if (Mdl) {
            if (buffer) {
                m_PortStream->UnmapAllocatedPages(buffer, Mdl);
            }
            m_PortStream->FreePagesFromMdl(Mdl);
        }
    }

    STDMETHODIMP_(VOID) GetHWLatency(_Out_ PKSRTAUDIO_HWLATENCY Latency) override
    {
        Latency->FifoSize = 0;
        Latency->ChipsetDelay = 0;
        Latency->CodecDelay = 0;
    }

    STDMETHODIMP_(NTSTATUS) GetPositionRegister(_Out_ PKSRTAUDIO_HWREGISTER) override
    {
        return STATUS_NOT_SUPPORTED;
    }

    STDMETHODIMP_(NTSTATUS) GetClockRegister(_Out_ PKSRTAUDIO_HWREGISTER) override
    {
        return STATUS_NOT_SUPPORTED;
    }

private:
    // Move the position to "now" (caller holds m_Lock, state is RUN); for
    // capture, fill the buffer region that was just "recorded".
    void Advance()
    {
        LARGE_INTEGER freq;
        LONGLONG now = KeQueryPerformanceCounter(&freq).QuadPart;
        ULONGLONG ticks = ULONGLONG(now - m_RunStart);
        ULONGLONG elapsed = (ticks / freq.QuadPart) * CF_RATE +
                            (ticks % freq.QuadPart) * CF_RATE / freq.QuadPart;
        ULONGLONG target = m_RunBaseFrames + elapsed;
        if (target <= m_Frames) {
            return;
        }
        ULONGLONG count = target - m_Frames;
        if (m_Capture && m_Buffer && m_BufferSize) {
            ULONG bufferFrames = m_BufferSize / m_Format.BlockAlign;
            // After a long stall only the last buffer's worth matters.
            ULONGLONG skip = count > bufferFrames ? count - bufferFrames : 0;
            ULONG pos = ULONG((m_Frames + skip) % bufferFrames);
            ULONG remaining = ULONG(count - skip);
            while (remaining) {
                ULONG chunk = min(remaining, bufferFrames - pos);
                m_Miniport->Pull(m_Buffer + pos * m_Format.BlockAlign, chunk, m_Format);
                pos = (pos + chunk) % bufferFrames;
                remaining -= chunk;
            }
        }
        m_Frames = target;
    }

    CMiniportWaveRT* m_Miniport = nullptr;
    PPORTWAVERTSTREAM m_PortStream = nullptr;
    bool m_Capture = false;
    StreamFormat m_Format = {};
    KSPIN_LOCK m_Lock;
    KSSTATE m_State = KSSTATE_STOP;
    PBYTE m_Buffer = nullptr;
    ULONG m_BufferSize = 0;
    // Frames the stream has advanced since it was last stopped.
    ULONGLONG m_Frames = 0;
    // m_Frames when the stream last entered RUN, and the QPC at that time.
    ULONGLONG m_RunBaseFrames = 0;
    LONGLONG m_RunStart = 0;
};

STDMETHODIMP_(NTSTATUS) CMiniportWaveRTStream::NonDelegatingQueryInterface(_In_ REFIID Interface, _COM_Outptr_ PVOID* Object)
{
    if (IsEqualGUIDAligned(Interface, IID_IUnknown)) {
        *Object = PVOID(PUNKNOWN(PMINIPORTWAVERTSTREAM(this)));
    } else if (IsEqualGUIDAligned(Interface, IID_IMiniportWaveRTStream)) {
        *Object = PVOID(PMINIPORTWAVERTSTREAM(this));
    } else {
        *Object = nullptr;
        return STATUS_INVALID_PARAMETER;
    }
    PUNKNOWN(*Object)->AddRef();
    return STATUS_SUCCESS;
}

STDMETHODIMP_(NTSTATUS) CMiniportWaveRT::NewStream(
    _Out_ PMINIPORTWAVERTSTREAM* Stream,
    _In_ PPORTWAVERTSTREAM PortStream,
    _In_ ULONG Pin,
    _In_ BOOLEAN Capture,
    _In_ PKSDATAFORMAT DataFormat)
{
    *Stream = nullptr;
    const bool capture = m_Endpoint->Kind == CfKind::Capture;
    if (Pin != WAVE_PIN_HOST || bool(Capture) != capture) {
        return STATUS_INVALID_PARAMETER;
    }
    StreamFormat format;
    if (!ParseFormat(DataFormat, &format)) {
        return STATUS_NO_MATCH;
    }
    auto* stream = new (POOL_FLAG_NON_PAGED, CF_POOLTAG) CMiniportWaveRTStream(nullptr);
    if (!stream) {
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    stream->AddRef();
    NTSTATUS status = stream->Init(this, PortStream, capture, format);
    if (!NT_SUCCESS(status)) {
        stream->Release();
        return status;
    }
    *Stream = PMINIPORTWAVERTSTREAM(stream);
    return STATUS_SUCCESS;
}

// ---- Private property ------------------------------------------------------------------------

static NTSTATUS PropertyHandlerPush(_In_ PPCPROPERTY_REQUEST Request)
{
    if (Request->Verb & KSPROPERTY_TYPE_BASICSUPPORT) {
        if (Request->ValueSize >= sizeof(ULONG)) {
            *static_cast<PULONG>(Request->Value) = KSPROPERTY_TYPE_SET | KSPROPERTY_TYPE_BASICSUPPORT;
            Request->ValueSize = sizeof(ULONG);
            return STATUS_SUCCESS;
        }
        Request->ValueSize = sizeof(ULONG);
        return STATUS_BUFFER_OVERFLOW;
    }
    if (!(Request->Verb & KSPROPERTY_TYPE_SET)) {
        return STATUS_INVALID_DEVICE_REQUEST;
    }
    const ULONG frameBytes = CF_PUSH_CHANNELS * sizeof(LONG);
    if (!Request->Value || Request->ValueSize == 0 || Request->ValueSize % frameBytes) {
        return STATUS_INVALID_BUFFER_SIZE;
    }
    // MajorTarget is the miniport's IMiniportWaveRT, its first base.
    auto* miniport = reinterpret_cast<CMiniportWaveRT*>(Request->MajorTarget);
    miniport->Push(static_cast<const LONG*>(Request->Value), Request->ValueSize / frameBytes);
    return STATUS_SUCCESS;
}

NTSTATUS CreateMiniportWaveRT(_Out_ PUNKNOWN* Unknown, _In_ const CF_ENDPOINT* Endpoint)
{
    auto* obj = new (POOL_FLAG_NON_PAGED, CF_POOLTAG) CMiniportWaveRT(nullptr, Endpoint);
    if (!obj) {
        *Unknown = nullptr;
        return STATUS_INSUFFICIENT_RESOURCES;
    }
    *Unknown = PUNKNOWN(PMINIPORTWAVERT(obj));
    (*Unknown)->AddRef();
    return STATUS_SUCCESS;
}
