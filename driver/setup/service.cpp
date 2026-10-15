#include "service.h"

#include <windows.h>
#include <sddl.h>
#include <shlobj.h>
#include <cstdio>
#include <string>

#include "crossfade.h"
#include "endpoints.h"

#pragma comment(lib, "advapi32.lib")
#pragma comment(lib, "shell32.lib")

// Any local user may ask for names (the app runs unelevated); only SYSTEM
// and administrators may do more. Remote clients are rejected outright.
static const wchar_t* PIPE_SDDL = L"D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;AU)";
// A client that connects but does not send its request in time is dropped,
// so it cannot hold the (single-instance) server hostage.
static const DWORD CLIENT_TIMEOUT_MS = 2000;

static SERVICE_STATUS_HANDLE g_StatusHandle;
static SERVICE_STATUS g_Status;
static HANDLE g_StopEvent;

static void ReportStatus(DWORD state, DWORD exitCode = NO_ERROR)
{
    g_Status.dwServiceType = SERVICE_WIN32_OWN_PROCESS;
    g_Status.dwCurrentState = state;
    g_Status.dwWin32ExitCode = exitCode;
    g_Status.dwControlsAccepted = state == SERVICE_RUNNING ? SERVICE_ACCEPT_STOP : 0;
    g_Status.dwWaitHint = state == SERVICE_RUNNING ? 0 : 3000;
    SetServiceStatus(g_StatusHandle, &g_Status);
}

static DWORD WINAPI ControlHandler(DWORD control, DWORD, LPVOID, LPVOID)
{
    if (control == SERVICE_CONTROL_STOP) {
        ReportStatus(SERVICE_STOP_PENDING);
        SetEvent(g_StopEvent);
        return NO_ERROR;
    }
    return control == SERVICE_CONTROL_INTERROGATE ? NO_ERROR : ERROR_CALL_NOT_IMPLEMENTED;
}

// Turn a client-supplied name into a safe endpoint description: valid UTF-8,
// no control characters, trimmed, at most CF_NAMER_MAX_NAME characters.
static std::wstring CleanName(const char* utf8, int len)
{
    if (len <= 0) {
        return {};
    }
    int wlen = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, utf8, len, nullptr, 0);
    if (wlen <= 0) {
        return {};
    }
    std::wstring raw(wlen, L'\0');
    MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, utf8, len, raw.data(), wlen);
    std::wstring out;
    for (wchar_t c : raw) {
        if (c >= 0x20 && c != 0x7F) {
            out.push_back(c);
        }
    }
    size_t first = out.find_first_not_of(L' ');
    if (first == std::wstring::npos) {
        return {};
    }
    out = out.substr(first, out.find_last_not_of(L' ') - first + 1);
    if (out.size() > CF_NAMER_MAX_NAME) {
        out.resize(CF_NAMER_MAX_NAME);
    }
    return out;
}

// Parse "<slot>\t<name>\n" lines. Unknown slots or malformed lines fail the
// whole request.
static bool ParseRequest(const char* data, DWORD size, std::wstring (&names)[CF_RENDER_SLOTS])
{
    DWORD pos = 0;
    while (pos < size) {
        DWORD end = pos;
        while (end < size && data[end] != '\n') {
            end++;
        }
        DWORD tab = pos;
        while (tab < end && data[tab] != '\t') {
            tab++;
        }
        if (tab == end || tab - pos != 1 || data[pos] < '1' || data[pos] >= '1' + CF_RENDER_SLOTS) {
            return false;
        }
        names[data[pos] - '1'] = CleanName(data + tab + 1, int(end - tab - 1));
        pos = end + 1;
    }
    return true;
}

// Wait for an overlapped operation, giving up on stop or after `timeoutMs`.
static bool WaitIo(HANDLE pipe, OVERLAPPED* ov, DWORD timeoutMs, DWORD* transferred)
{
    HANDLE events[2] = {ov->hEvent, g_StopEvent};
    DWORD r = WaitForMultipleObjects(2, events, FALSE, timeoutMs);
    if (r != WAIT_OBJECT_0) {
        CancelIo(pipe);
        GetOverlappedResult(pipe, ov, transferred, TRUE);
        return false;
    }
    return GetOverlappedResult(pipe, ov, transferred, FALSE) != FALSE;
}

static void ServeClients()
{
    PSECURITY_DESCRIPTOR sd = nullptr;
    if (!ConvertStringSecurityDescriptorToSecurityDescriptorW(PIPE_SDDL, SDDL_REVISION_1, &sd,
                                                              nullptr)) {
        return;
    }
    SECURITY_ATTRIBUTES sa = {sizeof(sa), sd, FALSE};
    HANDLE ioEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    char buffer[CF_NAMER_MAX_MESSAGE];

    while (WaitForSingleObject(g_StopEvent, 0) != WAIT_OBJECT_0) {
        HANDLE pipe = CreateNamedPipeW(
            CF_NAMER_PIPE, PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED,
            PIPE_TYPE_MESSAGE | PIPE_READMODE_MESSAGE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS, 1,
            64, CF_NAMER_MAX_MESSAGE, 0, &sa);
        if (pipe == INVALID_HANDLE_VALUE) {
            // Another instance owns the name, or resources ran out: back off.
            WaitForSingleObject(g_StopEvent, 5000);
            continue;
        }
        OVERLAPPED ov = {};
        ov.hEvent = ioEvent;
        ResetEvent(ioEvent);
        DWORD n = 0;
        bool connected = ConnectNamedPipe(pipe, &ov) != FALSE;
        if (!connected) {
            DWORD err = GetLastError();
            connected = err == ERROR_PIPE_CONNECTED ||
                        (err == ERROR_IO_PENDING && WaitIo(pipe, &ov, INFINITE, &n));
        }
        if (connected) {
            ResetEvent(ioEvent);
            DWORD size = 0;
            bool read = ReadFile(pipe, buffer, sizeof(buffer), &size, &ov) != FALSE;
            if (!read && GetLastError() == ERROR_IO_PENDING) {
                read = WaitIo(pipe, &ov, CLIENT_TIMEOUT_MS, &size);
            }
            std::wstring names[CF_RENDER_SLOTS];
            bool ok = read && ParseRequest(buffer, size, names);
            if (ok) {
                NameEndpoints(names, 0, false);
            }
            const char* reply = ok ? "ok\n" : "error\n";
            ResetEvent(ioEvent);
            if (!WriteFile(pipe, reply, DWORD(strlen(reply)), &n, &ov) &&
                GetLastError() == ERROR_IO_PENDING) {
                WaitIo(pipe, &ov, CLIENT_TIMEOUT_MS, &n);
            }
            FlushFileBuffers(pipe);
            DisconnectNamedPipe(pipe);
        }
        CloseHandle(pipe);
    }
    CloseHandle(ioEvent);
    LocalFree(sd);
}

static void WINAPI ServiceMain(DWORD, LPWSTR*)
{
    g_StatusHandle = RegisterServiceCtrlHandlerExW(CF_NAMER_SERVICE, ControlHandler, nullptr);
    if (!g_StatusHandle) {
        return;
    }
    g_StopEvent = CreateEventW(nullptr, TRUE, FALSE, nullptr);
    ReportStatus(SERVICE_START_PENDING);
    HRESULT init = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    ReportStatus(SERVICE_RUNNING);
    ServeClients();
    if (SUCCEEDED(init)) {
        CoUninitialize();
    }
    CloseHandle(g_StopEvent);
    ReportStatus(SERVICE_STOPPED);
}

int RunService()
{
    SERVICE_TABLE_ENTRYW table[] = {
        {const_cast<LPWSTR>(CF_NAMER_SERVICE), ServiceMain},
        {nullptr, nullptr},
    };
    return StartServiceCtrlDispatcherW(table) ? 0 : 1;
}

// ---- Installation ---------------------------------------------------------------

// %ProgramFiles%\Gleem Crossfade\crossfade-driver-setup.exe
static std::wstring InstalledPath()
{
    PWSTR programFiles = nullptr;
    std::wstring path;
    if (SUCCEEDED(SHGetKnownFolderPath(FOLDERID_ProgramFiles, 0, nullptr, &programFiles))) {
        path = std::wstring(programFiles) + L"\\Gleem Crossfade";
    }
    CoTaskMemFree(programFiles);
    return path.empty() ? path : path + L"\\crossfade-driver-setup.exe";
}

static void StopService(SC_HANDLE service)
{
    SERVICE_STATUS status;
    if (!ControlService(service, SERVICE_CONTROL_STOP, &status)) {
        return;
    }
    for (int i = 0; i < 50 && QueryServiceStatus(service, &status) &&
                    status.dwCurrentState != SERVICE_STOPPED;
         i++) {
        Sleep(100);
    }
}

bool InstallService()
{
    std::wstring target = InstalledPath();
    if (target.empty()) {
        fwprintf(stderr, L"Could not locate Program Files.\n");
        return false;
    }
    SC_HANDLE scm = OpenSCManagerW(nullptr, nullptr, SC_MANAGER_ALL_ACCESS);
    if (!scm) {
        fwprintf(stderr, L"Could not open the service manager (0x%08lx).\n", GetLastError());
        return false;
    }
    // A running service holds the installed executable open.
    SC_HANDLE service = OpenServiceW(scm, CF_NAMER_SERVICE, SERVICE_ALL_ACCESS);
    if (service) {
        StopService(service);
    }

    wchar_t self[MAX_PATH];
    GetModuleFileNameW(nullptr, self, MAX_PATH);
    std::wstring dir = target.substr(0, target.rfind(L'\\'));
    CreateDirectoryW(dir.c_str(), nullptr);
    if (_wcsicmp(self, target.c_str()) != 0 && !CopyFileW(self, target.c_str(), FALSE)) {
        fwprintf(stderr, L"Could not copy the service to %s (0x%08lx).\n", target.c_str(),
                 GetLastError());
        if (service) CloseServiceHandle(service);
        CloseServiceHandle(scm);
        return false;
    }

    std::wstring command = L"\"" + target + L"\" service";
    if (service) {
        ChangeServiceConfigW(service, SERVICE_NO_CHANGE, SERVICE_AUTO_START, SERVICE_NO_CHANGE,
                             command.c_str(), nullptr, nullptr, nullptr, nullptr, nullptr,
                             nullptr);
    } else {
        service = CreateServiceW(scm, CF_NAMER_SERVICE, L"Gleem Crossfade Device Names",
                                 SERVICE_ALL_ACCESS, SERVICE_WIN32_OWN_PROCESS,
                                 SERVICE_AUTO_START, SERVICE_ERROR_NORMAL, command.c_str(),
                                 nullptr, nullptr, L"AudioEndpointBuilder\0\0", nullptr, nullptr);
        if (!service) {
            fwprintf(stderr, L"Could not create the naming service (0x%08lx).\n", GetLastError());
            CloseServiceHandle(scm);
            return false;
        }
    }
    SERVICE_DESCRIPTIONW description = {const_cast<LPWSTR>(
        L"Names the Crossfade Channel audio outputs after the channels in Gleem Crossfade.")};
    ChangeServiceConfig2W(service, SERVICE_CONFIG_DESCRIPTION, &description);
    // Restart after a crash: twice quickly, then give up until the next boot.
    SC_ACTION actions[3] = {
        {SC_ACTION_RESTART, 5000}, {SC_ACTION_RESTART, 30000}, {SC_ACTION_NONE, 0}};
    SERVICE_FAILURE_ACTIONSW failure = {86400, nullptr, nullptr, 3, actions};
    ChangeServiceConfig2W(service, SERVICE_CONFIG_FAILURE_ACTIONS, &failure);

    bool ok = StartServiceW(service, 0, nullptr) ||
              GetLastError() == ERROR_SERVICE_ALREADY_RUNNING;
    if (ok) {
        wprintf(L"Naming service installed and running.\n");
    } else {
        fwprintf(stderr, L"Could not start the naming service (0x%08lx).\n", GetLastError());
    }
    CloseServiceHandle(service);
    CloseServiceHandle(scm);
    return ok;
}

bool UninstallService()
{
    SC_HANDLE scm = OpenSCManagerW(nullptr, nullptr, SC_MANAGER_ALL_ACCESS);
    if (!scm) {
        return false;
    }
    bool ok = true;
    SC_HANDLE service = OpenServiceW(scm, CF_NAMER_SERVICE, SERVICE_ALL_ACCESS);
    if (service) {
        StopService(service);
        if (DeleteService(service)) {
            wprintf(L"Removed the naming service.\n");
        } else {
            ok = false;
        }
        CloseServiceHandle(service);
    }
    CloseServiceHandle(scm);

    std::wstring target = InstalledPath();
    if (!target.empty() && GetFileAttributesW(target.c_str()) != INVALID_FILE_ATTRIBUTES) {
        // Uninstalling from the installed copy itself: delete it on reboot.
        if (!DeleteFileW(target.c_str())) {
            MoveFileExW(target.c_str(), nullptr, MOVEFILE_DELAY_UNTIL_REBOOT);
        }
        RemoveDirectoryW(target.substr(0, target.rfind(L'\\')).c_str());
    }
    return ok;
}
