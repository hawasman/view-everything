# S3 Remotes via `rusty-s3`, blocking `ureq` and the OS keyring

S3 Remotes list the bucket with ListObjectsV2 (1000 keys a page). Requests are presigned by `rusty-s3` (no I/O, pure-Rust signing) and sent by blocking `ureq` with rustls; the full async AWS SDK was rejected because it brings a tokio runtime and a large dependency tree for one read-only call, and the app has no async runtime in its scan path. Custom endpoints (R2, MinIO, B2, Wasabi) are the same code with an endpoint and a path-style option.

Credentials follow the standard chain, read by a small `~/.aws` ini parser: keys typed into the dialog, a chosen profile, `AWS_*` variables, then `AWS_PROFILE`/`default`. Typed secrets are stored with the `keyring` crate (Secret Service, Keychain, Credential Manager) under service `view-everything` and the Remote's identity; settings.toml holds only the access key ID. Not supported: role-assuming and SSO profiles, and servers with a private CA (rustls uses the bundled web roots).
