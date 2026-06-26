# iOS Git Sync for Obsidian

This project contains:

- An Obsidian plugin that runs on desktop, iOS, and iPadOS.
- A Rust sync server that performs all native Git operations.

The device does not run Git. The plugin sends changed files to:

```text
/v1/users/{user}/vaults/{vault}
```

The Rust server validates the OIDC bearer token, authorizes the `{user}` namespace, rebases and pushes Git history, and returns merged file changes.

## Storage model

- Text/code files are stored and versioned directly in Git.
- Binary files such as images/audio are stored as plain object files under the server vault directory.
- Binary metadata is committed to Git in `.obsidian-git-sync/binary-manifest.json`.
- This keeps Git useful for actual text/code version storage while avoiding binary blobs in Git history.

Server layout:

```text
data/users/{user}/vaults/{vault}/repo
data/users/{user}/vaults/{vault}/binary
data/users/{user}/vaults/{vault}/state.json
```

## Build and test

```bash
npm install
npm run build
npm test
```

Plugin output:

```text
main.js
manifest.json
```

Rust server binary:

```text
rust-server/target/release/obsidian-git-sync-server
```

## Run the Rust server

Production OIDC mode:

```bash
export OIDC_ISSUER="https://issuer.example.com"
export OIDC_AUDIENCE="obsidian-git-sync"
export OIDC_USER_CLAIM="preferred_username"
export OIDC_JWKS_URL="https://issuer.example.com/keys" # optional; otherwise discovered from OIDC metadata
export OBSIDIAN_GIT_SYNC_DATA_DIR="/srv/obsidian-git-sync"
export OBSIDIAN_GIT_SYNC_LISTEN="127.0.0.1:8787"
export OBSIDIAN_GIT_SYNC_MAX_BODY_BYTES="52428800"
export OBSIDIAN_GIT_SYNC_ALLOWED_REMOTE_HOSTS="github.com,gitlab.com,git.example.com" # required for network remotes
export OBSIDIAN_GIT_SYNC_ALLOWED_ORIGINS="" # default: no browser CORS headers
npm run start:server
```

Local development token mode:

```bash
export OBSIDIAN_GIT_SYNC_DEV_TOKEN="change-me"
export OBSIDIAN_GIT_SYNC_DEV_USER="alice"
export OBSIDIAN_GIT_SYNC_ALLOW_LOCAL_REMOTES="true" # only for local bare test repos
npm run start:server
```

Use HTTPS in production, usually by placing the server behind a reverse proxy.

Docker:

```bash
docker build -t obsidian-git-sync-server .
docker run --rm \
  -p 8787:8787 \
  -v obsidian-git-sync-data:/data \
  -e OIDC_ISSUER="https://issuer.example.com" \
  -e OIDC_AUDIENCE="obsidian-git-sync" \
  -e OIDC_USER_CLAIM="preferred_username" \
  -e OBSIDIAN_GIT_SYNC_ALLOWED_REMOTE_HOSTS="github.com,gitlab.com,git.example.com" \
  obsidian-git-sync-server
```

The container listens on `0.0.0.0:8787` and stores server state in `/data`.

Security defaults:

- OIDC issuer/JWKS URLs must use HTTPS, except localhost development URLs.
- Plugin server/OIDC URLs must use HTTPS, except localhost development URLs.
- Git remotes must use HTTPS or SSH. Local paths and `file://` remotes are disabled unless `OBSIDIAN_GIT_SYNC_ALLOW_LOCAL_REMOTES=true`.
- HTTPS remote URLs with embedded credentials are rejected so tokens are not written to `state.json`; configure server-side Git credentials or SSH keys instead.
- Network remote hosts are default-deny. List each allowed Git host in `OBSIDIAN_GIT_SYNC_ALLOWED_REMOTE_HOSTS`.
- CORS headers are not emitted by default. Set `OBSIDIAN_GIT_SYNC_ALLOWED_ORIGINS` to exact origins if you intentionally call the API from a browser app.

## Plugin setup

Install:

```text
.obsidian/plugins/ios-git-sync/main.js
.obsidian/plugins/ios-git-sync/manifest.json
```

Configure:

- Sync server URL
- OIDC issuer, client ID, scope, and optional audience
- OIDC access token, or use **Start OIDC device login**
- User namespace, e.g. `alice`
- Vault namespace, e.g. `personal`
- Git remote URL
- Branch
- Author name/email
- Optional device name

The Git remote URL is used only by the server. SSH works if the server's native Git/SSH environment is configured.

## Version UI

The plugin adds:

- **Show current file versions**
- **Resolve current conflict file**
- **Sync now**
- **Start OIDC device login**

The version modal lists commits for the active file, previews selected versions read-only, copies text versions, and can replace the current file with a selected version.

## Conflict handling

The server uses:

- `git rebase origin/{branch}` for remote integration, preserving flat history.
- `git merge-file` for server/client text conflicts before committing.

If a conflict remains, the plugin receives conflict-marker content and writes it into the file. This works on mobile because the user resolves the file inside Obsidian, then runs **Resolve current conflict file** or syncs again.

## API

- `POST /v1/users/{user}/vaults/{vault}/register`
- `POST /v1/users/{user}/vaults/{vault}/sync`
- `GET /v1/users/{user}/vaults/{vault}/history?path=Note.md`
- `GET /v1/users/{user}/vaults/{vault}/file?path=Note.md&hash=<commit>`
- `POST /v1/users/{user}/vaults/{vault}/resolve`

## Limits

- The server sees plaintext vault contents.
- End-to-end encrypted content is out of scope for v1 because it prevents server-side text merges.
- OIDC device login is implemented; automatic refresh-token storage is not implemented yet.
- Binary file version retrieval depends on the server object store retaining the hash referenced by Git metadata.
- The plugin stores the OIDC access token in Obsidian plugin data. Use short-lived access tokens; refresh-token storage is intentionally not implemented.
