# One shell for every Module: a rail, one Query box, an Inspector and a Timeline dock

The PRD adds four Modules (Search, Performance, Apps, Network) and three cross-cutting pages (Agents, Permissions, Plugins) to an app whose frame was built for one. By v0.2.1 that frame had filled up: one pill component served as button, tab, toggle and pager; Sync now existed in seven places and Export in three; the Explorer header carried five controls over nested tabs; Tasks and Locations were two popovers anchored by hand; the status bar existed on one page only; nine type sizes and no spacing tokens. Four radically different information architectures were drawn and compared (`designs/`): one Query as the only navigation, a Module rail with shared panes, an answer-first single column, and a DAW-style timeline. The rail with shared panes was chosen for its bones, with the answer-first first screen and a Query grammar kept as the next step.

## The shell

- **A rail of Modules** on the left (240px), labels not icons: Overview; Files with Browse, Cleaner and Duplicates nested; Settings at the foot. A Module joins by adding a rail entry and its pages; nothing else moves. The rail's Work block shows Tasks (the first running one's progress) and Locations (count, offline, syncing) as one button that opens the Work page, where each Task and Location opens in the Inspector; there are no popovers.
- **One Query box** in the top bar, on every page. `/` focuses it from anywhere; typing on a page that has no Query opens Browse. Its scope chip names the Module it searches (Files · Index today). When Performance and Network arrive, the scope follows the Module, and the same box over the desktop is the Search launcher.
- **An Inspector** on the right (300px) that shows what is selected anywhere: an Entry, a Location or a Task today; an Owner, a process, an app, a connection or an agent later. One shape: kind, title, facts, actions (destructive last). It follows the selection and Esc clears it; leaving a page clears it. The Explorer's Selection panel became its Entry case.
- **A Timeline dock** at the bottom that every Module reads. Today it holds Snapshots: on Browse a days lane with a tick per Snapshot drives Compare with; elsewhere it reports the Moment (Live or a date). Performance's per-second history and Network's per-app history will scrub on the same dock (PRD §6.4).
- **A status strip** on every page: Sync phase, Last Synced, Offline Volumes, unreadable folders, problems, Entries, Trash.

## The look

The `achromatic-ledger` app style from the taste study: no colour on chrome, one near-black primary per screen, grey secondaries, 1px edges and 8px corners, five type sizes (24/16/30/14/12), one gap. Teal is the one data hue (used bars, progress, the Snapshot tick). No serif, no gradients, no shadows at rest. A Done Task is grey; colour only on what needs a look.

## Consequences

- Every page starts with a title row and sits in the Shell's gutter; pages stop drawing their own outer frame.
- Module pages that have no data are not drawn: the rail shows what exists, so Performance, Apps, Network, Agents and Plugins appear when their data does.
- Not done yet, by decision: the Query grammar with `kind:`/`is:`/`at:`/`since:` facets that would make the rail a rendering of the Query (Design 1); the severity-ranked three-question Overview (Design 3) waits for Performance and Network data; the Cleaner and Duplicates pages keep their v0.2 insides until the Files rework.
