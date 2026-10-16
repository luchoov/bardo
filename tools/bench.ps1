# Measures bardo and the official Discord client side by side while you use them normally.
#
#   powershell -ExecutionPolicy Bypass -File tools\bench.ps1              # until Ctrl+C
#   powershell -ExecutionPolicy Bypass -File tools\bench.ps1 -Minutes 60
#   powershell -ExecutionPolicy Bypass -File tools\bench.ps1 -Summary bench.csv
#
# Every few seconds it adds up each app's processes (Discord runs several) and writes a row per app
# to a CSV: private memory, the "Memory" column of Task Manager, and CPU as a share of the whole
# machine. When it stops it prints a summary; -Summary prints one for an existing file.

param(
    [double]$Minutes = 0,
    [int]$Interval = 5,
    [string]$Out = "bench-$(Get-Date -Format 'yyyyMMdd-HHmm').csv",
    [string]$Summary
)

$ErrorActionPreference = "Stop"
$apps = @("bardo", "Discord")
$cores = [Environment]::ProcessorCount

function Show-Summary([string]$file) {
    $rows = Import-Csv $file
    $minutes = if ($rows.Count -gt 1) {
        ([datetime]$rows[-1].time - [datetime]$rows[0].time).TotalMinutes
    } else { 0 }
    "{0}: {1:N0} minutes, {2} logical processors" -f $file, $minutes, $cores
    ""
    "| | Memory, median | Memory, max | CPU, average | CPU, max | Processes |"
    "|---|---|---|---|---|---|"
    foreach ($app in $apps) {
        $samples = @($rows | Where-Object { $_.app -eq $app -and [int]$_.processes -gt 0 })
        if ($samples.Count -eq 0) { continue }
        $memory = @($samples | ForEach-Object { [double]$_.memory_mb } | Sort-Object)
        $cpu = @($samples | ForEach-Object { [double]$_.cpu_percent })
        $processes = ($samples | ForEach-Object { [int]$_.processes } | Measure-Object -Maximum).Maximum
        "| {0} | {1:N0} MB | {2:N0} MB | {3:N1} % | {4:N1} % | {5} |" -f $app, $memory[[int]($memory.Count / 2)],
            $memory[-1], ($cpu | Measure-Object -Average).Average, ($cpu | Measure-Object -Maximum).Maximum, $processes
    }
}

if ($Summary) {
    Show-Summary $Summary
    return
}

# CPU time per process id at the previous sample, per app.
$previous = @{}
foreach ($app in $apps) {
    $previous[$app] = @{}
    Get-Process -Name $app -ErrorAction SilentlyContinue |
        ForEach-Object { $previous[$app][$_.Id] = $_.TotalProcessorTime.TotalSeconds }
}
$invariant = [Globalization.CultureInfo]::InvariantCulture
$clock = [Diagnostics.Stopwatch]::StartNew()
$last = $clock.Elapsed
"time,app,processes,memory_mb,cpu_percent" | Set-Content $Out
Write-Output "Writing to $Out every $Interval s. Ctrl+C to stop."

try {
    while ($Minutes -eq 0 -or $clock.Elapsed.TotalMinutes -lt $Minutes) {
        Start-Sleep -Seconds $Interval
        $now = $clock.Elapsed
        $wall = ($now - $last).TotalSeconds
        $last = $now
        # Working Set - Private, as in Task Manager.
        $private = @{}
        Get-CimInstance Win32_PerfRawData_PerfProc_Process -Filter "Name LIKE 'bardo%' OR Name LIKE 'Discord%'" |
            ForEach-Object { $private[[int]$_.IDProcess] = [double]$_.WorkingSetPrivate }
        foreach ($app in $apps) {
            $processes = @(Get-Process -Name $app -ErrorAction SilentlyContinue)
            $memory = 0.0
            $cpu = 0.0
            $times = @{}
            foreach ($process in $processes) {
                $memory += $private[$process.Id]
                $total = $process.TotalProcessorTime.TotalSeconds
                $times[$process.Id] = $total
                # Only processes seen last time count, so a new one does not show its whole life at once.
                if ($previous[$app].ContainsKey($process.Id)) { $cpu += $total - $previous[$app][$process.Id] }
            }
            $previous[$app] = $times
            $row = [string]::Format($invariant, "{0},{1},{2},{3:F1},{4:F2}", (Get-Date -Format "s"), $app,
                $processes.Count, ($memory / 1MB), (100 * $cpu / ($wall * $cores)))
            $row | Add-Content $Out
        }
    }
} finally {
    ""
    Show-Summary $Out
}
