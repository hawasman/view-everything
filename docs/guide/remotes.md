# Remotes

A Remote is storage the app indexes over a network protocol without mounting it. Remotes are **read-only**: the app only lists them, never changes or removes their Entries, and they have no Trash.

Add a Remote in Settings. **Test** checks its details before you add it, and you can edit it later. A name is optional; without one the Remote shows its host or bucket.

A Remote syncs when you press Sync now on it (Dashboard or Settings), or on a schedule you choose for it; the default is Manual. The Dashboard's Sync now for every Volume leaves Remotes out. Mounted network shares are a different thing: they are Volumes, and indexing them is opt-in in Settings.

## Kinds

| Kind | You give | Secret | Notes |
| --- | --- | --- | --- |
| SSH | host and folder | your `ssh` keys or ssh-agent | uses the system `ssh` ([ADR-0005](../adr/0005-ssh-remotes-via-system-ssh.md)); nothing is installed on the server |
| S3 | bucket, optional prefix, endpoint, region, access key | secret key | any S3-compatible service ([ADR-0006](../adr/0006-s3-remotes-via-rusty-s3-and-ureq.md)) |
| FTP | host, port, user, folder | password | FTPS (`AUTH TLS`) unless you allow plain FTP |
| WebDAV | folder URL, user | password | Basic authentication; leave the user empty for a server that needs none |
| Azure Blob | storage account, container, optional prefix, optional endpoint | account key or SAS token | |

Secrets are kept in the operating system's keyring, never in the settings file.

## SSH

- Keys and ssh-agent only. A host that accepts only passwords reports an error instead of prompting.
- On the server the app runs GNU `find` and reads its output. Where `find` is not GNU's, it walks the tree over SFTP through the same `ssh` instead.
- Over SFTP, On-disk Size equals Logical Size and creation dates are unknown. The SFTP protocol reports neither.
- **Sync this folder** works inside SSH Remotes. Other kinds of Remote sync whole.

## S3 and Azure Blob

- A bucket or container is listed flat and shown as folders split on `/`.
- S3 has no creation date, so `dc:` never matches there.
- For Azure, a SAS token needs list permission. Fill in Endpoint for Azurite or a sovereign cloud.

## FTP

- Listed with MLSD where the server offers it, otherwise by parsing `LIST`.
- Only listing commands are sent.

## WebDAV

- Read folder by folder with `PROPFIND Depth: 1`, which servers that refuse `Depth: infinity` accept.
- Use `https://` anywhere you don't trust the network. Over `http://`, Basic authentication sends the password without encryption.
