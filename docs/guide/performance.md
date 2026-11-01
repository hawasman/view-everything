# Performance

The Performance Module shows what the machine is doing: CPU use, memory and disk throughput, now or at any second of the last hour. It is read-only and needs no permissions. Linux only for now.

## The System page

**Performance → System** appears in the rail once the first sample is taken, about two seconds after the window opens. It shows the second the Moment points at:

- **CPU**: the whole machine's busy share, then each logical CPU. Where the kernel says which cores are performance cores and which are efficiency cores (Intel hybrid parts list them in `/sys/devices/cpu_core` and `cpu_atom`; Arm big.LITTLE parts give big cores a higher `cpu_capacity`), they are grouped as P and E; otherwise each is just "CPU n".
- **Memory**: used by programs, cache and buffers the kernel can give back, free, what is available without swapping, and swap in use.
- **Disks**: read and write throughput per whole disk (partitions are part of their disk; loop and RAM devices are left out).
- **Sensors**: GPU, temperature and power read "unsupported": the app does not measure them yet, and shows no number rather than a zero.

A reading the app could not take this second shows "—". A file it cannot read at all (an unusual kernel, a container) makes that section "unsupported".

## The Timeline and the Moment

The app keeps one sample per second for the last hour, in memory only; nothing is written to disk. The **Timeline dock** at the foot of the window shows that hour as CPU bars on the Performance page. Press or drag on it to move the **Moment** there: the page then shows that second's sample, and the dock reads "At 14:03:27". **Back to Live** returns to the current second.

The Moment is the same on every page. On Browse it picks the Snapshot nearest that day for [Compare with](snapshots-and-export.md#compare-with); clicking a Snapshot tick moves the Moment to that day, which Performance shows as a gap unless it is within the last hour.

A second with no sample is a gap, drawn blank: the app was not running, it was not recording, or the sampler stalled. A gap is never shown as idle.

## When it records

Recording runs while the window is open, and after it closes only with **keep in tray** on ([Staying current](live-updates-and-background.md#background-tray-and-start-at-login)). A start at login that waits in the tray records only with keep in tray on. Reading the counters once a second costs well under 1 % of one CPU, and an hour of samples takes a few megabytes.
