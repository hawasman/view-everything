# Network

The Network Module shows what each network interface is receiving and sending, and which connections are open and which process holds each, now or at any second of the last hour. It is read-only and needs no permissions. Linux only for now; on other platforms nothing is recorded and the Module does not appear.

## The Network page

**Network** appears in the rail, under Performance → System, once a sample has a network reading, about two seconds after the window opens. It shows the second the Moment points at:

- **Receiving** and **Sending**: the sum over every interface, in bytes per second.
- **Received, last hour** and **Sent, last hour**: what every interface moved in the recorded seconds of the hour up to the Moment, and the interface that moved the most; "—" while no second of it had a known count.
- **Interfaces**: one row per interface (`eth0`, `wlp3s0`, `tun0`…) with its receive and send rates that second and its totals for the hour. Loopback (`lo`) is left out: it is no link.

An interface that goes away (a VPN that disconnects, a USB tether unplugged) keeps its row for the rest of the hour, marked "gone" with the last second it was seen (at a past Moment, "not there at this Moment"); scrub back to see its rates then. A new interface shows "—" for its first second, since a rate needs two readings; so does a counter that went back (the interface was reset), and the hour's totals leave those seconds out. A counter of a 32-bit kernel that wraps past its limit is still read as the bytes moved.

## Traffic per process

**Traffic per process** lists each of your own processes holding a TCP connection, with its PID, what it receives and sends per second at the Moment, and what it moved in the last hour; the most bytes first. Its title carries the line **TCP only, your processes**: that is all it can read without administrator rights. UDP, other users' processes (and the system's) and connections too short to be seen between two readings are not counted. Nothing asks for administrator rights.

- The kernel counts each TCP connection's bytes, received and sent (acknowledged by the other end), and tells any user, the way `ss -ti` shows them. Every two seconds, with the connections, the app reads those counts and finds which of your processes holds each connection; a process's rate is what its connections moved since the reading before, over the seconds between.
- A connection that closes counts no more; it never makes a rate go back. A new one counts every byte it moved since it opened. A connection handed to another process (a server passing it to a worker) counts for the process holding it now, from then on.
- A process that cannot be read, or that ended between the two readings, counts for no one.
- Click a process to open it in the Inspector.
- Live lists every process holding a TCP connection, idle ones too; each past second keeps the 200 that moved the most bytes then, so scrubbing the Timeline dock to a past Moment shows that second's traffic, with the rates of the two-second reading it falls in.

## Connections

**Connections** lists every open TCP and UDP socket, IPv4 and IPv6: protocol, local and remote address, state and the process holding it, with its PID. Open conversations come first, then ones opening and closing, then listening ports and UDP sockets waiting for anyone ("open"), then closed ones the kernel is finishing ("time wait").

- A socket held by a process the app cannot read (another user's, or the system's) says **unknown**: it belongs to some process, the app is just not allowed to see which. A closed connection the kernel is finishing belongs to no process and says **none, closed**.
- A listening port or an unconnected UDP socket has no remote end: its Remote reads "none".
- Click a connection to open its process in the Inspector (command line, user, start time, parent, CPU time); **Show on Performance** there opens Performance → System with that process selected in the table. Its row stays in view as the table re-sorts, until you scroll the table yourself.
- The list shows about a dozen rows at a time and scrolls; only the rows in view are drawn, so thousands of connections cost no more than a few.

The Overview's "Talking on the network" answer counts the distinct connections the hour's kept seconds hold and names the process that moved the most bytes over TCP in the hour (where traffic per process cannot be read, the process holding the most open connections with a remote end now); clicking it opens Network with that process inspected and its first connection at the top of the list.

Connections are read every two seconds, with the process table. Live lists every one; each past second keeps the 200 most worth keeping by state (open conversations first): a UDP socket's bytes, and those of other users' connections, cannot be read without administrator rights, so bytes cannot rank them all.

## The Timeline

Network is recorded with Performance, in the same samples: once a second, kept for the last hour, in memory only ([When it records](performance.md#when-it-records)). On the Network page the **Timeline dock** shows that hour as bars of the busiest second in each column, received plus sent, scaled to the hour's peak (the caption gives it). Press or drag on it to move the **Moment** there; **Back to Live** returns. Seconds with no sample are gaps, drawn blank.

It reads `/proc/net/dev` once a second, alongside the Performance counters, and `/proc/net/tcp`, `tcp6`, `udp` and `udp6` every two seconds, with each TCP socket's byte counts from the kernel's socket diagnostics (`sock_diag`, as `ss -ti` does); it finds each socket's process through the `/proc/<pid>/fd` of your own processes only.
