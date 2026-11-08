# Reads the ASUS GPU "LevelSense" G-sensor (Bosch BMI321 accelerometer) via GPU Tweak III's ExpanModule.dll.
# Must run in 32-bit PowerShell because ExpanModule.dll is x86:
#   C:\Windows\SysWOW64\WindowsPowerShell\v1.0\powershell.exe -ExecutionPolicy Bypass -File tools\read_gpu_imu.ps1
# Reference/debug only: the guard itself reads the sensor directly via NVAPI (rust/src/sensor),
# without GPU Tweak III. Useful to compare against ASUS's own filtered values.
param(
    [int]$Samples = 20,
    [int]$IntervalMs = 250,
    [int]$GpuId = -1   # -1 = auto-detect
)

if ([IntPtr]::Size -ne 4) { throw "Run this with 32-bit PowerShell (SysWOW64)." }

$dir = 'C:\Program Files (x86)\ASUS\GPUTweakIII'
Set-Location $dir   # so ExpanModule.dll finds its dependencies

Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class Expan {
    [DllImport("kernel32", SetLastError=true, CharSet=CharSet.Unicode)]
    public static extern bool SetDllDirectory(string p);
    [DllImport("ExpanModule.dll", CallingConvention=CallingConvention.Cdecl)]
    public static extern byte Expan_Api_Check_GSensor_Support(int gpuId, out byte supported);
    [DllImport("ExpanModule.dll", CallingConvention=CallingConvention.Cdecl)]
    public static extern int Expan_Api_Get_GSensor_Data(int gpuId, byte[] prev, byte[] result);
}
"@
[Expan]::SetDllDirectory($dir) | Out-Null

function Get-F([byte[]]$b, [int]$o) { [BitConverter]::ToSingle($b, $o) }

if ($GpuId -lt 0) {
    foreach ($id in 0..255) {
        $s = 0
        [Expan]::Expan_Api_Check_GSensor_Support($id, [ref]$s) | Out-Null
        if ($s -ne 0) { $GpuId = $id; break }
    }
    if ($GpuId -lt 0) { throw "No GPU with G-sensor support found." }
}
Write-Host "Using GPU id $GpuId"

# Buffers: floats X,Y,Z at offsets 8/0xC/0x10. 'prev' feeds the module's smoothing filter.
$prev = New-Object byte[] 64
$cur  = New-Object byte[] 64
for ($i = 0; $i -lt $Samples; $i++) {
    [Array]::Clear($cur, 0, $cur.Length)
    $rc = [Expan]::Expan_Api_Get_GSensor_Data($GpuId, $prev, $cur)
    $x = Get-F $cur 8; $y = Get-F $cur 12; $z = Get-F $cur 16
    $mag = [Math]::Sqrt($x*$x + $y*$y + $z*$z)
    # Tilt of each axis relative to gravity, in degrees
    $ax = if ($mag) { [Math]::Asin([Math]::Max(-1.0,[Math]::Min(1.0,$x/$mag))) * 180 / [Math]::PI } else { 0 }
    $ay = if ($mag) { [Math]::Asin([Math]::Max(-1.0,[Math]::Min(1.0,$y/$mag))) * 180 / [Math]::PI } else { 0 }
    $az = if ($mag) { [Math]::Asin([Math]::Max(-1.0,[Math]::Min(1.0,$z/$mag))) * 180 / [Math]::PI } else { 0 }
    "{0,3}  rc={1}  X={2,9:F5}  Y={3,9:F5}  Z={4,9:F5}  |g|={5:F4}  angles(deg) X={6,7:F2} Y={7,7:F2} Z={8,7:F2}" -f $i,$rc,$x,$y,$z,$mag,$ax,$ay,$az
    [Array]::Copy($cur, $prev, $cur.Length)
    Start-Sleep -Milliseconds $IntervalMs
}

