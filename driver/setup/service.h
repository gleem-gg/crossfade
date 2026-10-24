// The endpoint naming service: lets the unprivileged app name the
// "Crossfade Channel N" outputs after its channels (see crossfade.h).
#pragma once

// Entry point when started by the service control manager (`service`).
int RunService();

// Copy this executable to Program Files and register + start the service
// (or update an existing one). Returns false on failure.
bool InstallService();

// Stop and remove the service and its installed executable.
bool UninstallService();
