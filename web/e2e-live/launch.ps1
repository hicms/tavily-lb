# Starts tavily-lb the way an interactive terminal would: with Ctrl+C handling enabled.
# Processes inherit the "ignore Ctrl+C" flag from their creator, and automation hosts often set it,
# which would make a real Ctrl+C test meaningless. Prints "CHILDPID <pid>" and exits with the child's code.
param([Parameter(Mandatory)][string]$Exe, [string]$Config = '')
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class LiveE2ELaunch {
    delegate bool Handler(uint ctrlType);
    [DllImport("kernel32.dll")] static extern bool SetConsoleCtrlHandler(Handler handler, bool add);
    static readonly Handler Swallow = _ => true;
    public static void EnableCtrlCForChildren() { SetConsoleCtrlHandler(null, false); }
    // The launcher shares the server's console and must outlive it to report the exit code.
    public static void SurviveConsoleEvents() { SetConsoleCtrlHandler(Swallow, true); }
}
'@
[LiveE2ELaunch]::EnableCtrlCForChildren()
$psi = New-Object System.Diagnostics.ProcessStartInfo -Property @{FileName = $Exe; UseShellExecute = $false}
if ($Config) { $psi.ArgumentList.Add('--config'); $psi.ArgumentList.Add($Config) }
[LiveE2ELaunch]::SurviveConsoleEvents()
$p = [System.Diagnostics.Process]::Start($psi)
[Console]::Out.WriteLine("CHILDPID $($p.Id)")
[Console]::Out.Flush()
$p.WaitForExit()
exit $p.ExitCode
