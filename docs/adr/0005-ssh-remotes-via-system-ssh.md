# SSH Remotes via the system `ssh` binary and remote `find`

SSH Remotes are indexed by running the system `ssh` binary with `find <root> -xdev -printf ...` on the server and streaming the result, falling back to SFTP (`ssh -s sftp`) when GNU `find -printf` is unavailable. Nothing is installed on the server. Using the system binary inherits `~/.ssh/config`, ssh-agent, ProxyJump, known_hosts and hardware keys; an embedded library (`russh`) was rejected because it would need all of those reimplemented. Remote `find` was chosen over SFTP-first because SFTP needs a round-trip per directory.
