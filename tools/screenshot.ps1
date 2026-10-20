# Captures the running app window, optionally after clicking a point in it.
# Usage: tools\screenshot.ps1 -Out shot.png [-ClickX x -ClickY y]
# Coordinates are physical pixels relative to the window's top-left corner.
# PrintWindow captures the window even when it's behind other windows.
param([Parameter(Mandatory)][string]$Out, [int]$ClickX = -1, [int]$ClickY = -1)
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System; using System.Runtime.InteropServices;
public class Shot { [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r); [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint f); [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h); [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y); [DllImport("user32.dll")] public static extern void mouse_event(uint f, int x, int y, uint d, IntPtr e);
public struct RECT { public int L,T,R,B; } }
"@
[Shot]::SetProcessDPIAware() | Out-Null
$h = (Get-Process idm2 | Select-Object -First 1).MainWindowHandle
$r = New-Object Shot+RECT; [Shot]::GetWindowRect($h, [ref]$r) | Out-Null
if ($ClickX -ge 0) {
    [Shot]::SetForegroundWindow($h) | Out-Null; Start-Sleep -Milliseconds 300
    [Shot]::SetCursorPos($r.L + $ClickX, $r.T + $ClickY) | Out-Null; Start-Sleep -Milliseconds 150
    [Shot]::mouse_event(2, 0, 0, 0, [IntPtr]::Zero); [Shot]::mouse_event(4, 0, 0, 0, [IntPtr]::Zero)
    # Park the cursor off the rows so hover state doesn't leak into the capture.
    [Shot]::SetCursorPos($r.L + 1200, $r.B - 60) | Out-Null; Start-Sleep 3
}
$bmp = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
$g = [System.Drawing.Graphics]::FromImage($bmp); $dc = $g.GetHdc(); [Shot]::PrintWindow($h, $dc, 2) | Out-Null; $g.ReleaseHdc($dc)
$bmp.Save($Out)
