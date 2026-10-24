// crossfade-driver-setup: install, name or remove the Crossfade virtual
// audio driver. Root-enumerated devices have no hardware to trigger PnP, so
// the device node is created here before the driver is bound to it - what
// `devcon install` does, without shipping devcon.
//
//   crossfade-driver-setup install [path\to\CrossfadeAudio.inf]
//   crossfade-driver-setup install-service
//   crossfade-driver-setup name-endpoints
//   crossfade-driver-setup uninstall
//
// `install` also sets up the endpoint naming service (service.cpp), which
// runs this same executable as `crossfade-driver-setup service`.
//
// Needs administrator rights (the manifest requests elevation).

#include <windows.h>
#include <setupapi.h>
#include <newdev.h>
#include <devguid.h>
#include <cfgmgr32.h>
#include <cstdio>
#include <string>
#include <vector>

#include "crossfade.h"
#include "endpoints.h"
#include "service.h"

#pragma comment(lib, "setupapi.lib")
#pragma comment(lib, "newdev.lib")

static void PrintError(const wchar_t* what)
{
    DWORD err = GetLastError();
    wchar_t* msg = nullptr;
    FormatMessageW(FORMAT_MESSAGE_ALLOCATE_BUFFER | FORMAT_MESSAGE_FROM_SYSTEM |
                       FORMAT_MESSAGE_IGNORE_INSERTS,
                   nullptr, err, 0, reinterpret_cast<LPWSTR>(&msg), 0, nullptr);
    fwprintf(stderr, L"%s failed (0x%08lx): %s", what, err, msg ? msg : L"\n");
    LocalFree(msg);
}

// Does the device's hardware id list contain ours?
static bool MatchesHardwareId(HDEVINFO set, SP_DEVINFO_DATA* info)
{
    wchar_t buf[512] = {};
    if (!SetupDiGetDeviceRegistryPropertyW(set, info, SPDRP_HARDWAREID, nullptr,
                                           reinterpret_cast<PBYTE>(buf), sizeof(buf) - 4,
                                           nullptr)) {
        return false;
    }
    for (const wchar_t* p = buf; *p; p += wcslen(p) + 1) {
        if (_wcsicmp(p, CF_HARDWARE_ID) == 0) {
            return true;
        }
    }
    return false;
}

// Call `fn` for every device node (present or not) with our hardware id.
template <typename F>
static int ForEachDevice(F fn)
{
    HDEVINFO set = SetupDiGetClassDevsW(&GUID_DEVCLASS_MEDIA, nullptr, nullptr, 0);
    if (set == INVALID_HANDLE_VALUE) {
        return 0;
    }
    int count = 0;
    SP_DEVINFO_DATA info = {sizeof(info)};
    for (DWORD i = 0; SetupDiEnumDeviceInfo(set, i, &info); i++) {
        if (MatchesHardwareId(set, &info)) {
            fn(set, &info);
            count++;
        }
    }
    SetupDiDestroyDeviceInfoList(set);
    return count;
}

static bool CreateDeviceNode()
{
    HDEVINFO set = SetupDiCreateDeviceInfoList(&GUID_DEVCLASS_MEDIA, nullptr);
    if (set == INVALID_HANDLE_VALUE) {
        PrintError(L"SetupDiCreateDeviceInfoList");
        return false;
    }
    SP_DEVINFO_DATA info = {sizeof(info)};
    bool ok = false;
    // Hardware ids are a REG_MULTI_SZ: double NUL terminated.
    std::wstring ids = std::wstring(CF_HARDWARE_ID) + L'\0' + L'\0';
    if (!SetupDiCreateDeviceInfoW(set, L"MEDIA", &GUID_DEVCLASS_MEDIA, L"Crossfade Virtual Audio",
                                  nullptr, DICD_GENERATE_ID, &info)) {
        PrintError(L"SetupDiCreateDeviceInfo");
    } else if (!SetupDiSetDeviceRegistryPropertyW(
                   set, &info, SPDRP_HARDWAREID, reinterpret_cast<const BYTE*>(ids.c_str()),
                   DWORD(ids.size() * sizeof(wchar_t)))) {
        PrintError(L"SetupDiSetDeviceRegistryProperty");
    } else if (!SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, &info)) {
        PrintError(L"DIF_REGISTERDEVICE");
    } else {
        ok = true;
    }
    SetupDiDestroyDeviceInfoList(set);
    return ok;
}

static int Install(const wchar_t* infArg)
{
    wchar_t inf[MAX_PATH];
    if (!GetFullPathNameW(infArg, MAX_PATH, inf, nullptr) ||
        GetFileAttributesW(inf) == INVALID_FILE_ATTRIBUTES) {
        fwprintf(stderr, L"INF not found: %s\n", infArg);
        return 2;
    }
    int existing = ForEachDevice([](HDEVINFO, SP_DEVINFO_DATA*) {});
    if (existing == 0) {
        wprintf(L"Creating the Crossfade Virtual Audio device...\n");
        if (!CreateDeviceNode()) {
            return 1;
        }
    }
    wprintf(L"Installing the driver from %s...\n", inf);
    BOOL reboot = FALSE;
    if (!UpdateDriverForPlugAndPlayDevicesW(nullptr, CF_HARDWARE_ID, inf, INSTALLFLAG_FORCE,
                                            &reboot)) {
        PrintError(L"UpdateDriverForPlugAndPlayDevices");
        return 1;
    }
    if (reboot) {
        wprintf(L"Installed. Restart Windows to finish.\n");
        return 3010;
    }
    // Installing the package succeeds even when Windows then refuses to
    // load the driver; report what the device actually ended up as.
    ULONG problem = 0;
    ForEachDevice([&](HDEVINFO, SP_DEVINFO_DATA* info) {
        ULONG status = 0, code = 0;
        if (CM_Get_DevNode_Status(&status, &code, info->DevInst, 0) == CR_SUCCESS &&
            (status & DN_HAS_PROBLEM)) {
            problem = code;
        }
    });
    if (problem == CM_PROB_UNSIGNED_DRIVER) {
        fwprintf(stderr,
                 L"Installed, but Windows blocked the driver (code 52): it is not signed by\n"
                 L"Microsoft. Development builds only load in test-signing mode:\n"
                 L"  bcdedit /set testsigning on   (elevated, Secure Boot off), then reboot,\n"
                 L"and import CrossfadeAudio.cer into LocalMachine\\Root and\n"
                 L"LocalMachine\\TrustedPublisher. See driver\\README.md.\n");
        return 1;
    }
    if (problem) {
        fwprintf(stderr, L"Installed, but the device reports problem code %lu (see Device Manager).\n",
                 problem);
        return 1;
    }
    wprintf(L"Installed and running.\n");
    // Windows names speaker-type endpoints generically ("Speakers"), which
    // would leave eight identical outputs: give each its default name now;
    // the naming service renames them after the app's channels later.
    std::wstring defaults[CF_RENDER_SLOTS];
    if (NameEndpoints(defaults, 15000, true) < CF_RENDER_SLOTS + 2) {
        fwprintf(stderr, L"Not all endpoints have appeared yet; run "
                         L"`crossfade-driver-setup name-endpoints` later to name them.\n");
    }
    return InstallService() ? 0 : 1;
}

// Is this published oem*.inf our driver package? Both the service it
// installs must be exactly CrossfadeAudio and the driver store must record
// CrossfadeAudio.inf as its original name, so an unrelated INF that merely
// shares a prefix is never removed.
static bool IsOurInf(const wchar_t* path)
{
    wchar_t service[128] = {};
    if (!GetPrivateProfileStringW(L"Crossfade_Device.NT.Services", L"AddService", L"", service,
                                  ARRAYSIZE(service), path)) {
        return false;
    }
    std::wstring name(service);
    name = name.substr(0, name.find(L','));
    size_t first = name.find_first_not_of(L" \t");
    size_t last = name.find_last_not_of(L" \t");
    if (first == std::wstring::npos ||
        _wcsicmp(name.substr(first, last - first + 1).c_str(), L"CrossfadeAudio") != 0) {
        return false;
    }

    DWORD size = 0;
    if (!SetupGetInfInformationW(path, INFINFO_INF_NAME_IS_ABSOLUTE, nullptr, 0, &size)) {
        return false;
    }
    std::vector<BYTE> buffer(size);
    auto* info = reinterpret_cast<PSP_INF_INFORMATION>(buffer.data());
    if (!SetupGetInfInformationW(path, INFINFO_INF_NAME_IS_ABSOLUTE, info, size, nullptr)) {
        return false;
    }
    SP_ORIGINAL_FILE_INFO_W original = {sizeof(original)};
    if (!SetupQueryInfOriginalFileInformationW(info, 0, nullptr, &original)) {
        return false;
    }
    const wchar_t* base = wcsrchr(original.OriginalInfName, L'\\');
    return _wcsicmp(base ? base + 1 : original.OriginalInfName, L"CrossfadeAudio.inf") == 0;
}

static int Uninstall()
{
    bool failed = !UninstallService();
    bool reboot = false;
    int n = ForEachDevice([&](HDEVINFO set, SP_DEVINFO_DATA* info) {
        BOOL needReboot = FALSE;
        if (!DiUninstallDevice(nullptr, set, info, 0, &needReboot)) {
            PrintError(L"DiUninstallDevice");
            failed = true;
        }
        reboot |= needReboot != FALSE;
    });
    if (n == 0) {
        wprintf(L"The Crossfade Virtual Audio device is not installed.\n");
    }

    // Remove the driver package(s) from the driver store as well.
    std::vector<std::wstring> oems;
    WIN32_FIND_DATAW fd;
    wchar_t pattern[MAX_PATH];
    GetWindowsDirectoryW(pattern, MAX_PATH);
    wcscat_s(pattern, L"\\INF\\oem*.inf");
    HANDLE h = FindFirstFileW(pattern, &fd);
    if (h != INVALID_HANDLE_VALUE) {
        do {
            wchar_t path[MAX_PATH];
            GetWindowsDirectoryW(path, MAX_PATH);
            wcscat_s(path, L"\\INF\\");
            wcscat_s(path, fd.cFileName);
            if (IsOurInf(path)) {
                oems.push_back(fd.cFileName);
            }
        } while (FindNextFileW(h, &fd));
        FindClose(h);
    }
    for (const auto& oem : oems) {
        if (SetupUninstallOEMInfW(oem.c_str(), SUOI_FORCEDELETE, nullptr)) {
            wprintf(L"Removed driver package %s.\n", oem.c_str());
        } else {
            PrintError(L"SetupUninstallOEMInf");
            failed = true;
        }
    }
    if (reboot) {
        wprintf(L"Restart Windows to finish.\n");
    }
    return failed ? 1 : (reboot ? 3010 : 0);
}

int wmain(int argc, wchar_t** argv)
{
    if (argc >= 2 && _wcsicmp(argv[1], L"service") == 0) {
        return RunService();
    }
    // Everything below talks to the MMDevice API.
    CoInitializeEx(nullptr, COINIT_APARTMENTTHREADED);
    if (argc >= 2 && _wcsicmp(argv[1], L"install") == 0) {
        // Default: the INF next to this executable.
        wchar_t inf[MAX_PATH];
        if (argc >= 3) {
            wcscpy_s(inf, argv[2]);
        } else {
            GetModuleFileNameW(nullptr, inf, MAX_PATH);
            wchar_t* slash = wcsrchr(inf, L'\\');
            if (slash) {
                *(slash + 1) = 0;
            }
            wcscat_s(inf, L"CrossfadeAudio.inf");
        }
        return Install(inf);
    }
    if (argc >= 2 && _wcsicmp(argv[1], L"install-service") == 0) {
        // Add (or update) just the naming service on an existing install.
        return InstallService() ? 0 : 1;
    }
    if (argc >= 2 && _wcsicmp(argv[1], L"name-endpoints") == 0) {
        // Reset to the defaults; a running app renames them again.
        std::wstring defaults[CF_RENDER_SLOTS];
        int found = NameEndpoints(defaults, 0, true);
        wprintf(L"%d Crossfade endpoints found.\n", found);
        return found > 0 ? 0 : 1;
    }
    if (argc >= 2 && _wcsicmp(argv[1], L"uninstall") == 0) {
        return Uninstall();
    }
    fwprintf(stderr,
             L"usage: crossfade-driver-setup install [CrossfadeAudio.inf]\n"
             L"       crossfade-driver-setup install-service\n"
             L"       crossfade-driver-setup name-endpoints\n"
             L"       crossfade-driver-setup uninstall\n");
    return 2;
}
