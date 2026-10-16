# Takes the README screenshots and animation from the demo (made-up servers and people, nothing
# from an account). Needs a debug build (cargo build -p bardo), and Python with Pillow for the GIF.
#
#   powershell -ExecutionPolicy Bypass -File tools\screenshots.ps1
#
# Each scene starts bardo with BARDO_DEMO=<scene>, sizes the window, waits for it to settle and saves
# its client area to docs\images\<name>.png. The tour is recorded frame by frame into tour.gif.
# A real bardo can stay open meanwhile.

param(
    [string]$Exe = "target\debug\bardo.exe",
    [string]$Out = "docs\images",
    # Logical pixels.
    [int]$Width = 380,
    [int]$Height = 640,
    # Screenshots are drawn at this scale, for sharp images on high-DPI screens; the GIF at 1.
    [double]$Scale = 2
)

$ErrorActionPreference = "Stop"

Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
using System.Text;

public static class Shot {
    public delegate bool EnumProc(IntPtr hwnd, IntPtr param);
    [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc proc, IntPtr param);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr hwnd, out uint pid);
    [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr hwnd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern int GetWindowText(IntPtr hwnd, StringBuilder text, int max);
    [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr context);
    [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hwnd);
    [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hwnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
    [DllImport("user32.dll")] public static extern bool AdjustWindowRectExForDpi(ref RECT rect, uint style, bool menu, uint exStyle, uint dpi);
    [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr hwnd, int index);
    [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);

    [StructLayout(LayoutKind.Sequential)]
    public struct RECT { public int Left, Top, Right, Bottom; }

    public static IntPtr Find(uint pid) {
        IntPtr found = IntPtr.Zero;
        EnumWindows((hwnd, _) => {
            uint owner;
            GetWindowThreadProcessId(hwnd, out owner);
            var title = new StringBuilder(64);
            GetWindowText(hwnd, title, 64);
            if (owner == pid && IsWindowVisible(hwnd) && title.ToString() == "bardo") { found = hwnd; return false; }
            return true;
        }, IntPtr.Zero);
        return found;
    }

    // Sizes the client area to width x height physical pixels.
    public static void Size(IntPtr hwnd, int width, int height) {
        var rect = new RECT { Left = 0, Top = 0, Right = width, Bottom = height };
        AdjustWindowRectExForDpi(ref rect, (uint)GetWindowLong(hwnd, -16), false, (uint)GetWindowLong(hwnd, -20), GetDpiForWindow(hwnd));
        // SWP_NOZORDER | SWP_NOACTIVATE
        SetWindowPos(hwnd, IntPtr.Zero, 80, 80, rect.Right - rect.Left, rect.Bottom - rect.Top, 0x0004 | 0x0010);
    }

    public static System.Drawing.Bitmap Capture(IntPtr hwnd) {
        RECT rect;
        GetClientRect(hwnd, out rect);
        var bitmap = new System.Drawing.Bitmap(rect.Right, rect.Bottom);
        using (var graphics = System.Drawing.Graphics.FromImage(bitmap)) {
            IntPtr hdc = graphics.GetHdc();
            // PW_CLIENTONLY | PW_RENDERFULLCONTENT: the GPU-drawn content, without the title bar.
            PrintWindow(hwnd, hdc, 1 | 2);
            graphics.ReleaseHdc(hdc);
        }
        return bitmap;
    }
}
"@ -ReferencedAssemblies System.Drawing

# Per-monitor DPI aware, so sizes and captures are in real pixels.
[void][Shot]::SetProcessDpiAwarenessContext([IntPtr]-4)

$scenes = [ordered]@{
    "main"     = "1"
    "call"     = "call"
    "settings" = "settings"
    "sign-in"  = "login"
}

$exePath = (Resolve-Path $Exe).Path
New-Item -ItemType Directory -Force $Out | Out-Null
$outPath = (Resolve-Path $Out).Path

# Starts a scene and returns its process and window, sized.
function Open-Scene([string]$scene, [double]$scale) {
    $env:BARDO_DEMO = $scene
    $env:SLINT_SCALE_FACTOR = "$scale"
    # Minimized applies to the debug build's console, not to bardo's window.
    $process = Start-Process -FilePath $exePath -PassThru -WindowStyle Minimized
    $hwnd = [IntPtr]::Zero
    for ($i = 0; $i -lt 100 -and $hwnd -eq [IntPtr]::Zero; $i++) {
        Start-Sleep -Milliseconds 100
        $hwnd = [Shot]::Find([uint32]$process.Id)
    }
    if ($hwnd -eq [IntPtr]::Zero) {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        throw "no window for $scene"
    }
    [Shot]::Size($hwnd, [int]($Width * $scale), [int]($Height * $scale))
    [void][Shot]::SetForegroundWindow($hwnd)
    return $process, $hwnd
}

try {
    foreach ($name in $scenes.Keys) {
        $process, $hwnd = Open-Scene $scenes[$name] $Scale
        try {
            # Let it lay out, load fonts and finish any animation.
            Start-Sleep -Milliseconds 1500
            $bitmap = [Shot]::Capture($hwnd)
            $file = Join-Path $outPath "$name.png"
            $bitmap.Save($file, [System.Drawing.Imaging.ImageFormat]::Png)
            $bitmap.Dispose()
            Write-Output $file
        } finally {
            Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
        }
    }

    # The tour (app/src/demo.rs) lasts about 10 s and starts as soon as the window opens. It is
    # recorded a frame every 100 ms or so, each frame timed by the clock.
    $frames = Join-Path ([IO.Path]::GetTempPath()) "bardo-tour"
    Remove-Item -Recurse -Force $frames -ErrorAction SilentlyContinue
    New-Item -ItemType Directory $frames | Out-Null
    $process, $hwnd = Open-Scene "tour" 1
    try {
        $clock = [Diagnostics.Stopwatch]::StartNew()
        $times = @()
        for ($i = 0; $clock.ElapsedMilliseconds -lt 11000; $i++) {
            $bitmap = [Shot]::Capture($hwnd)
            $bitmap.Save((Join-Path $frames ("{0:d4}.png" -f $i)), [System.Drawing.Imaging.ImageFormat]::Png)
            $bitmap.Dispose()
            $times += $clock.ElapsedMilliseconds
            Start-Sleep -Milliseconds 60
        }
        $times -join "`n" | Set-Content (Join-Path $frames "times.txt")
    } finally {
        Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue
    }
    python (Join-Path $PSScriptRoot "make_gif.py") $frames (Join-Path $outPath "tour.gif")
    Remove-Item -Recurse -Force $frames
} finally {
    Remove-Item Env:BARDO_DEMO -ErrorAction SilentlyContinue
    Remove-Item Env:SLINT_SCALE_FACTOR -ErrorAction SilentlyContinue
}
