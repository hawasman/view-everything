# Network

The Network Module shows what each network interface is receiving and sending, now or at any second of the last hour. It is read-only and needs no permissions. Linux only for now; on other platforms nothing is recorded and the Module does not appear.

## The Network page

**Network** appears in the rail, under Performance → System, once a sample has a network reading, about two seconds after the window opens. It shows the second the Moment points at:

- **Receiving** and **Sending**: the sum over every interface, in bytes per second.
- **Received, last hour** and **Sent, last hour**: what every interface moved in the recorded seconds of the hour up to the Moment, and the interface that moved the most; "—" while no second of it had a known count.
- **Interfaces**: one row per interface (`eth0`, `wlp3s0`, `tun0`…) with its receive and send rates that second and its totals for the hour. Loopback (`lo`) is left out: it is no link.

An interface that goes away (a VPN that disconnects, a USB tether unplugged) keeps its row for the rest of the hour, marked "gone" with the last second it was seen (at a past Moment, "not there at this Moment"); scrub back to see its rates then. A new interface shows "—" for its first second, since a rate needs two readings; so does a counter that went back (the interface was reset), and the hour's totals leave those seconds out. A counter of a 32-bit kernel that wraps past its limit is still read as the bytes moved.

The page says, in one line, that per-app traffic needs administrator rights and is not available yet. There is no button and nothing asks for them.

## The Timeline

Network is recorded with Performance, in the same samples: once a second, kept for the last hour, in memory only ([When it records](performance.md#when-it-records)). On the Network page the **Timeline dock** shows that hour as bars of the busiest second in each column, received plus sent, scaled to the hour's peak (the caption gives it). Press or drag on it to move the **Moment** there; **Back to Live** returns. Seconds with no sample are gaps, drawn blank.

It reads `/proc/net/dev`, once a second, alongside the Performance counters.
