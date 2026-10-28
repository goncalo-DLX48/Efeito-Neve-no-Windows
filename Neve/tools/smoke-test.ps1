# Smoke test do SnowHush: arranca a app, mede memória/CPU, verifica que o
# overlay é click-through e guarda uma captura de ecrã.
#
#   .\tools\smoke-test.ps1            # 8 s, intensidade heavy
#   .\tools\smoke-test.ps1 -Seconds 20 -Intensity steady
#
[CmdletBinding()]
param(
    [int]$Seconds = 8,
    [ValidateSet("off", "light", "steady", "heavy", "variable")]
    [string]$Intensity = "heavy",
    [string]$Screenshot = "$env:TEMP\snowhush-smoke.png"
)

$ErrorActionPreference = "Stop"
Add-Type -AssemblyName System.Drawing

Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class SnowProbe {
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr ctx);
    [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr ctx);
    [DllImport("user32.dll")] public static extern int GetSystemMetrics(int i);
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X; public int Y; }
    [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(POINT p);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetClassName(IntPtr h, StringBuilder s, int n);
}
"@

[void][SnowProbe]::SetProcessDpiAwarenessContext([IntPtr](-4))
[void][SnowProbe]::SetThreadDpiAwarenessContext([IntPtr](-4))

$vw = [SnowProbe]::GetSystemMetrics(0)
$vh = [SnowProbe]::GetSystemMetrics(1)
$vx = [SnowProbe]::GetSystemMetrics(76)
$vy = [SnowProbe]::GetSystemMetrics(77)

$exe = Join-Path (Split-Path -Parent $PSScriptRoot) "target\release\snowhush.exe"
if (-not (Test-Path -LiteralPath $exe)) {
    throw "Executável não encontrado: $exe. Corra 'cargo build --release' primeiro."
}

$env:SNOWHUSH_LOG = "1"
$log = "$env:TEMP\snowhush.log"
Remove-Item -LiteralPath $log -ErrorAction SilentlyContinue

$proc = Start-Process -FilePath $exe -ArgumentList "--intensity", $Intensity -PassThru
Start-Sleep -Seconds $Seconds

if ($proc.HasExited) {
    throw "O processo terminou inesperadamente (exit code $($proc.ExitCode))."
}

$p = Get-Process -Id $proc.Id
$bmp = New-Object System.Drawing.Bitmap $vw, $vh
$g = [System.Drawing.Graphics]::FromImage($bmp)
$g.CopyFromScreen($vx, $vy, 0, 0, $bmp.Size)
$g.Dispose()
$bmp.Save($Screenshot, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()

# O overlay é WS_EX_TRANSPARENT: WindowFromPoint deve devolver a janela de baixo.
$pt = New-Object SnowProbe+POINT
$pt.X = [int]($vw / 2)
$pt.Y = [int]($vh / 3)
$hwnd = [SnowProbe]::WindowFromPoint($pt)
$sb = New-Object System.Text.StringBuilder 256
[void][SnowProbe]::GetClassName($hwnd, $sb, 256)

$fps = (Get-Content -LiteralPath $log -Encoding UTF8 -ErrorAction SilentlyContinue | Select-String "FPS" | Select-Object -Last 1).Line
$errors = (Get-Content -LiteralPath $log -Encoding UTF8 -ErrorAction SilentlyContinue | Select-String "render falhou|erro fatal").Count

$result = [pscustomobject]@{
    Ecra            = "$vw x $vh @ ($vx,$vy)"
    Intensidade     = $Intensity
    "RAM (WS)"      = "$([math]::Round($p.WorkingSet64 / 1MB, 1)) MB"
    "RAM (privada)" = "$([math]::Round($p.PrivateMemorySize64 / 1MB, 1)) MB"
    "CPU (s)"       = [math]::Round($p.CPU, 2)
    "Classe sob o cursor" = $sb.ToString()
    "Click-through" = ($sb.ToString() -ne "SnowHushOverlayWindow")
    "FPS"           = $fps
    "Erros"         = $errors
    "Captura"       = $Screenshot
}
$result | Format-List

Stop-Process -Id $proc.Id -ErrorAction SilentlyContinue
