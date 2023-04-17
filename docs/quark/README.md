# Installation

If you want to try the Quark sandboxer, the QuarkContainer should be installed.
To build and install Quark, please refer to [installing-from-source](https://github.com/QuarkContainer/Quark#installing-from-source).

After installation, we should change the `Sandboxed` config to `true` in the `/etc/quark/config.json`:

```json
{
  "DebugLevel"    : "Error",
  "KernelMemSize" : 24,
  "LogType"       : "Sync",
  "LogLevel"      : "Simple",
  "UringIO"       : true,
  "UringBuf"      : true,
  "UringFixedFile": false,
  "EnableAIO"     : true,
  "PrintException": false,
  "KernelPagetable": false,
  "PerfDebug"     : false,
  "UringStatx"    : false,
  "FileBufWrite"  : true,
  "MmapRead"      : false,
  "AsyncAccept"   : true,
  "EnableRDMA"    : false,
  "RDMAPort"      : 1,
  "PerSandboxLog" : false,
  "ReserveCpuCount": 1,
  "ShimMode"      : false,
  "EnableInotify" : true,
  "ReaddirCache"  : true,
  "HiberODirect"  : true,
  "DisableCgroup" : true,
  "CopyDataWithPf": true,
  "TlbShootdownWait": true,
  "Sandboxed": true
}
```

Make sure the `ShimMode` is false, and `Sandboxed` is true in the config file.