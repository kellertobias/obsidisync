# Obsync for Obsidian

This project contains:

- An Obsidian plugin that runs on iOS, iPadOS, and macOS.
- A Rust sync server that performs all native Git operations.

The device does not run Git. The plugin sends changed files to:

```text
/v1/users/{user}/vaults/{vault}
```

The Rust server validates the bearer token, authorizes the `{user}` namespace, commits Git history, optionally rebases and pushes to a configured remote, and returns merged file changes. Tokens can come from OIDC or from the built-in single-user password mode.

## Storage model

- Text/code files are stored and versioned directly in Git.
- Binary files such as images/audio are stored as plain object files under the server vault directory.
- Binary metadata is committed to Git in `.obsidian-git-sync/binary-manifest.json`.
- Changed file contents are uploaded to the server in bounded chunks before sync; the final sync request references staged upload IDs instead of embedding large base64 payloads.
- If no Git remote URL is configured, the server keeps a normal local Git repository under the data directory and never fetches or pushes.
- This keeps Git useful for actual text/code version storage while avoiding binary blobs in Git history.

Server layout:

```text
data/users/{user}/vaults/{vault}/repo
data/users/{user}/vaults/{vault}/binary
data/users/{user}/vaults/{vault}/uploads
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
export OIDC_DEVICE_CLIENT_ID="obsidian-device"
export OIDC_DEVICE_SCOPE="openid profile email"
export OIDC_USER_CLAIM="preferred_username"
export OIDC_JWKS_URL="https://issuer.example.com/keys" # optional; otherwise discovered from OIDC metadata
export OBSIDIAN_GIT_SYNC_DATA_DIR="/srv/obsidian-git-sync"
export OBSIDIAN_GIT_SYNC_LISTEN="127.0.0.1:8787"
export OBSIDIAN_GIT_SYNC_MAX_BODY_BYTES="52428800"
export OBSIDIAN_GIT_SYNC_ALLOWED_REMOTE_HOSTS="github.com,gitlab.com,git.example.com" # required for network remotes
export OBSIDIAN_GIT_SYNC_ALLOWED_ORIGINS="" # default: no browser CORS headers
npm run start:server
```

Single-user password mode without SSO:

```bash
export OBSIDIAN_GIT_SYNC_PASSWORD_USER="alice"
export OBSIDIAN_GIT_SYNC_DATA_DIR="/srv/obsidian-git-sync"
export OBSIDIAN_GIT_SYNC_LISTEN="127.0.0.1:8787"
export OBSIDIAN_GIT_SYNC_ALLOWED_REMOTE_HOSTS="github.com,gitlab.com,git.example.com" # only needed when using network remotes
npm run start:server
```

Then click **Log in** in the Obsidian plugin settings. On the first login, the plugin sets the password through the server; later logins use the same button and store the returned access token automatically. The web `/login` page remains available for browser access to the change feed and manual token recovery. `OBSIDIAN_GIT_SYNC_USER` is accepted as a shorter alias for `OBSIDIAN_GIT_SYNC_PASSWORD_USER`.

After logging in, the page also shows a recent change feed for the user's synced vaults.

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
  -e OIDC_DEVICE_CLIENT_ID="obsidian-device" \
  -e OIDC_USER_CLAIM="preferred_username" \
  -e OBSIDIAN_GIT_SYNC_ALLOWED_REMOTE_HOSTS="github.com,gitlab.com,git.example.com" \
  obsidian-git-sync-server
```

The container listens on `0.0.0.0:8787` and stores server state in `/data`.

Security defaults:

- OIDC issuer/JWKS URLs must use HTTPS, except localhost development URLs.
- Password mode stores an Argon2 password hash and hashed access tokens under `OBSIDIAN_GIT_SYNC_DATA_DIR/auth/password.json`.
- Plugin server/OIDC URLs must use HTTPS, except localhost development URLs.
- Leaving the Git remote URL blank is allowed and selects server-local Git storage in `OBSIDIAN_GIT_SYNC_DATA_DIR`.
- Git remotes must use HTTPS or SSH. Local paths and `file://` remotes are disabled unless `OBSIDIAN_GIT_SYNC_ALLOW_LOCAL_REMOTES=true`.
- HTTPS remote URLs with embedded credentials are rejected so tokens are not written to `state.json`; configure server-side Git credentials or SSH keys instead.
- Network remote hosts are default-deny. List each allowed Git host in `OBSIDIAN_GIT_SYNC_ALLOWED_REMOTE_HOSTS` when using a remote URL.
- CORS headers are not emitted by default. Set `OBSIDIAN_GIT_SYNC_ALLOWED_ORIGINS` to exact origins if you intentionally call the API from a browser app.

## Plugin setup

Install:

```text
.obsidian/plugins/ios-git-sync/main.js
.obsidian/plugins/ios-git-sync/manifest.json
```

Configure:

- Sync server URL
- Click **Log in**. The plugin fetches public auth settings from the server and stores the access token automatically.
- Vault name, e.g. `personal`. This defaults to the Obsidian vault name.
- Author name/email
- Optional computer name
- Sync on startup and sync interval

Advanced settings:

- Access token, only for static-token development servers or recovery.
- User namespace, normally set automatically from the authenticated server user.
- Git remote URL, optional.
- Branch.

The Git remote URL is used only by the server. Leave it blank to use only the persistent server-local repository at `data/users/{user}/vaults/{vault}/repo`. SSH works if the server's native Git/SSH environment is configured.

### Login flow

The plugin only needs the sync server URL to start login:

1. The plugin calls `GET /v1/auth/config`.
2. In password mode, the plugin shows a username/password form and calls `/v1/auth/password/login` or `/v1/auth/password/setup`.
3. In OIDC mode, the server returns the public device-flow client configuration, and the plugin performs device login with the issuer advertised by the server.
4. The plugin stores the returned access token and calls `GET /v1/auth/session` to set the user namespace.

OIDC provider details are server configuration, not client setup. In OIDC mode the server requires:

- `OIDC_ISSUER`: issuer base URL.
- `OIDC_AUDIENCE`: audience expected in access tokens.
- `OIDC_DEVICE_CLIENT_ID`: public OIDC client configured for device authorization.
- `OIDC_DEVICE_SCOPE`: optional, defaults to `openid profile email`.
- `OIDC_USER_CLAIM`: optional, defaults to `preferred_username`.

## Version UI

The plugin adds:

- **Show current file versions**
- **Resolve current conflict file**
- **Sync now**
- **Log in to Obsync**

The version modal lists commits for the active file, previews selected versions read-only, copies text versions, and can replace the current file with a selected version.

## Conflict handling

The server uses:

- `git rebase origin/{branch}` for remote integration when a remote URL is configured, preserving flat history.
- `git merge-file` for server/client text conflicts before committing.

If a conflict remains, the plugin receives conflict-marker content and writes it into the file. This works on mobile because the user resolves the file inside Obsidian, then runs **Resolve current conflict file** or syncs again.

## API

- `GET /v1/auth/config`
- `GET /v1/auth/session`
- `POST /v1/auth/password/setup`
- `POST /v1/auth/password/login`
- `POST /v1/users/{user}/vaults/{vault}/register`
- `POST /v1/users/{user}/vaults/{vault}/uploads`
- `POST /v1/users/{user}/vaults/{vault}/uploads/{upload}/chunk`
- `POST /v1/users/{user}/vaults/{vault}/uploads/{upload}/complete`
- `POST /v1/users/{user}/vaults/{vault}/sync`
- `GET /v1/users/{user}/feed`
- `GET /v1/users/{user}/vaults/{vault}/history?path=Note.md`
- `GET /v1/users/{user}/vaults/{vault}/file?path=Note.md&hash=<commit>`
- `POST /v1/users/{user}/vaults/{vault}/resolve`

## Limits

- The server sees plaintext vault contents.
- End-to-end encrypted content is out of scope for v1 because it prevents server-side text merges.
- OIDC device login is implemented; automatic refresh-token storage is not implemented yet.
- Binary file version retrieval depends on the server object store retaining the hash referenced by Git metadata.
- The plugin stores the access token in Obsidian plugin data. Use short-lived OIDC access tokens when using OIDC; refresh-token storage is intentionally not implemented. Password mode tokens can be rotated by deleting `auth/password.json` or resetting the password data directory.
