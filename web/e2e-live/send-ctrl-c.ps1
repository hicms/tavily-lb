# Delivers a real CTRL_C_EVENT (or CTRL_BREAK_EVENT) to a console process, the same event as pressing
# Ctrl+C / Ctrl+Break in its window. The target must own its console (spawned with windowsHide /
# CREATE_NO_WINDOW), otherwise the event would hit every process sharing the caller's console.
param([Parameter(Mandatory)][int]$ProcessId, [ValidateSet('c', 'break')][string]$Event = 'c')
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class LiveE2ECtrl {
    delegate bool Handler(uint ctrlType);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool AttachConsole(uint id);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool FreeConsole();
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool SetConsoleCtrlHandler(Handler handler, bool add);
    [DllImport("kernel32.dll", SetLastError = true)] static extern bool GenerateConsoleCtrlEvent(uint ctrlEvent, uint processGroupId);
    // Kept in a static field so the GC never collects the callback.
    static readonly Handler Swallow = _ => true;
    public static string Send(uint pid, uint ctrlEvent) {
        FreeConsole();
        if (!AttachConsole(pid)) return "AttachConsole failed: win32 error " + Marshal.GetLastWin32Error();
        // We are attached to the target console now; survive the event ourselves.
        SetConsoleCtrlHandler(Swallow, true);
        if (!GenerateConsoleCtrlEvent(ctrlEvent, 0)) return "GenerateConsoleCtrlEvent failed: win32 error " + Marshal.GetLastWin32Error();
        System.Threading.Thread.Sleep(300);
        FreeConsole();
        return null;
    }
}
'@
$err = [LiveE2ECtrl]::Send([uint32]$ProcessId, $(if ($Event -eq 'break') { 1 } else { 0 }))
if ($err) { [Console]::Error.WriteLine($err); exit 1 }
