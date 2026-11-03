# Performance

The Performance Module shows what the machine is doing: CPU use, memory and disk throughput, now or at any second of the last hour. It is read-only and needs no permissions. Linux only for now; on other platforms nothing is recorded and the Module does not appear.

## The System page

**Performance → System** appears in the rail once the first sample is taken, about two seconds after the window opens. It shows the second the Moment points at:

- **CPU**: the whole machine's busy share, then each logical CPU. Where the kernel says which cores are performance cores and which are efficiency cores (Intel hybrid parts list them in `/sys/devices/cpu_core` and `cpu_atom`; Arm big.LITTLE parts give big cores a higher `cpu_capacity`), they are grouped as P and E; otherwise each is just "CPU n".
- **Memory**: used by programs, cache and buffers the kernel can give back, free, what is available without swapping, and swap in use.
- **Disks**: read and write throughput per whole disk (partitions are part of their disk; loop and RAM devices are left out).
- **Sensors**: GPU, temperature and power read "unsupported": the app does not measure them yet, and shows no number rather than a zero.
- **Processes**: every running process with its CPU share (of the whole machine), resident memory and, while the page is open, its storage read and write per second. Sort by any column with the arrows in its header.

A reading the app could not take this second shows "—". A file it cannot read at all (an unusual kernel, a container) makes that section "unsupported".

## Processes

A process is known by its PID and the time it started, so a PID the system hands to a new process never carries the old one's history. The table is read every two seconds; the seconds between show the last reading. A process that started since the previous reading shows "—" for CPU until the next.

I/O is read only while the Performance page is open, one file per process; before that, and for processes of other users (which the kernel does not let the app read), Read and Write show "—".

Click a process to open it in the **Inspector**: its command line, the user it runs as, when it started, its parent, the CPU time it has used since it started, and its readings at the Moment. A detail that cannot be read (a kernel thread has no command line; a process that has exited cannot be asked) shows "—", and an exited process is tagged **Exited**.

Live, the table lists every process. Each second of the Timeline keeps the 200 that matter most: the busiest by CPU and the largest by memory, in turn. At a past Moment the table shows those, and says so.

## The Timeline and the Moment

The app keeps one sample per second for the last hour, in memory only; nothing is written to disk. The **Timeline dock** at the foot of the window shows that hour as CPU bars on the Performance page. Press or drag on it to move the **Moment** there: the page then shows that second's sample, and the dock reads "At 14:03:27". **Back to Live** returns to the current second.

The Moment is the same on every page. On Browse it picks the Snapshot nearest that day for [Compare with](snapshots-and-export.md#compare-with); clicking a Snapshot tick moves the Moment to that day, which Performance shows as a gap unless it is within the last hour.

A second with no sample is a gap, drawn blank: the app was not running, it was not recording, or the sampler stalled. A gap is never shown as idle.

## When it records

Recording runs while the window is open, and after it closes only with **keep in tray** on ([Staying current](live-updates-and-background.md#background-tray-and-start-at-login)). A start at login that waits in the tray records only with keep in tray on. On a 12-core desktop, reading the counters once a second measured 0.02 % of one CPU, and reading the table of 670 processes every two seconds about 0.4 % (0.5 % with I/O, while the page is open). An hour of samples takes about 40 MB, almost all of it the 200 processes kept every two seconds. The whole app, idle with a 5.7-million-Entry Index, measured 0.09 % of one CPU waiting in the tray while recording, and 0.5 % with the window open on this page, before the process table was added.
