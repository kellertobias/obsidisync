import { Notice, requestUrl, Vault } from "obsidian";
import { arrayBufferToBase64 } from "./base64";
import {
  HistoryEntry,
  RegisterRequest,
  RegisterResponse,
  ResolveRequest,
  SyncConflict,
  SyncRequest,
  SyncResponse,
  ManifestEntry,
  UploadChunkResponse,
  UploadCompleteResponse,
  UploadInitRequest,
  UploadInitResponse,
  VersionFileResponse
} from "./protocol";
import { getDeviceName } from "./runtime";
import { IosGitSyncSettings } from "./settings";
import { assertGitBranch, assertNamespaceSlug, assertSecureHttpUrl } from "./security";
import { sha256Hex, VaultState } from "./vaultState";

type SaveSettings = () => Promise<void>;
type ConflictNoticeHandler = (conflicts: SyncConflict[]) => void;
type SyncStateListener = (running: boolean) => void;
type SyncBlocker = () => string | null;
const MAIN_BRANCH = "main";

export interface OidcDeviceAuthorization {
  device_code: string;
  user_code: string;
  verification_uri: string;
  verification_uri_complete?: string;
  expires_in: number;
  interval?: number;
}

interface OidcDiscovery {
  device_authorization_endpoint: string;
  token_endpoint: string;
}

interface OidcTokenResponse {
  access_token?: string;
  error?: string;
  error_description?: string;
}

export type ServerAuthConfig =
  | { type: "password"; passwordConfigured: boolean; setupTokenRequired?: boolean }
  | { type: "oidc"; issuer: string; clientId: string; scope: string; audience?: string | null }
  | { type: "token" };

interface PasswordLoginResponse {
  user: string;
  accessToken: string;
}

interface AuthSessionResponse {
  user: string;
  subject: string;
}

export class GitService {
  private running = false;
  private syncStateListeners = new Set<SyncStateListener>();
  private oidcDiscoveryCache: OidcDiscovery | null = null;
  private oidcLoginConfig: Extract<ServerAuthConfig, { type: "oidc" }> | null = null;
  private oidcLoginServerUrl: string | null = null;

  constructor(
    private readonly vault: Vault,
    private settings: IosGitSyncSettings,
    private readonly saveSettings: SaveSettings,
    private readonly onConflictNotice?: ConflictNoticeHandler,
    private readonly syncBlocker?: SyncBlocker
  ) {}

  updateSettings(settings: IosGitSyncSettings): void {
    if (settings.serverUrl !== this.settings.serverUrl) {
      this.oidcDiscoveryCache = null;
      this.oidcLoginConfig = null;
      this.oidcLoginServerUrl = null;
    }
    this.settings = settings;
  }

  currentDeviceName(): string {
    return this.deviceName();
  }

  isSyncRunning(): boolean {
    return this.running;
  }

  onSyncStateChange(listener: SyncStateListener): () => void {
    this.syncStateListeners.add(listener);
    listener(this.running);
    return () => {
      this.syncStateListeners.delete(listener);
    };
  }

  async authConfig(): Promise<ServerAuthConfig> {
    if (!this.settings.serverUrl) throw new Error("Set a sync server URL before logging in");
    assertSecureHttpUrl(this.settings.serverUrl, "Sync server URL");
    const serverUrl = this.settings.serverUrl.replace(/\/+$/, "");
    const response = await requestUrl({
      url: `${serverUrl}/v1/auth/config`,
      method: "GET",
      throw: false
    });
    if (response.status < 200 || response.status >= 300) {
      throw new Error(response.text || `Auth configuration failed: HTTP ${response.status}`);
    }
    return response.json as ServerAuthConfig;
  }

  async loginPassword(username: string, password: string, setup: boolean, setupToken?: string): Promise<void> {
    if (!this.settings.serverUrl) throw new Error("Set a sync server URL before logging in");
    assertSecureHttpUrl(this.settings.serverUrl, "Sync server URL");
    const serverUrl = this.settings.serverUrl.replace(/\/+$/, "");
    const requestBody: { username: string; password: string; setupToken?: string } = { username, password };
    if (setup && setupToken) requestBody.setupToken = setupToken;
    const response = await requestUrl({
      url: `${serverUrl}/v1/auth/password/${setup ? "setup" : "login"}`,
      method: "POST",
      contentType: "application/json",
      body: JSON.stringify(requestBody),
      throw: false
    });
    if (response.status < 200 || response.status >= 300) {
      throw new Error(response.text || `Password login failed: HTTP ${response.status}`);
    }

    const body = response.json as PasswordLoginResponse;
    this.settings.oidcAccessToken = body.accessToken;
    this.settings.userSlug = body.user;
    await this.saveSettings();
  }

  async loadAuthenticatedUser(): Promise<void> {
    if (!this.settings.oidcAccessToken) throw new Error("Log in before loading the authenticated user");
    const session = await this.getJson<AuthSessionResponse>("/v1/auth/session");
    this.settings.userSlug = session.user;
    await this.saveSettings();
  }

  async sync(): Promise<SyncConflict[]> {
    const blockReason = this.syncBlocker?.();
    if (blockReason) {
      new Notice(blockReason);
      return [];
    }

    const conflicts = await this.exclusive(async () => {
      this.requireConfigured();
      await this.register();

      const vaultState = new VaultState(this.vault);
      const { manifest, changes } = await vaultState.collectChanges(this.settings.localManifest, {
        stageUpload: (path, buffer, entry) => this.uploadBuffer(path, buffer, entry)
      });
      const request: SyncRequest = {
        baseHead: this.settings.serverHead || null,
        clientId: this.settings.clientId,
        deviceName: this.deviceName(),
        changes,
        clientManifest: manifest
      };

      const response = await this.postJson<SyncResponse>(`${this.vaultPath()}/sync`, request);
      await vaultState.applyServerFiles(response.files);

      if (response.status === "conflict") {
        this.showConflictNotice(response.conflicts);
        return response.conflicts;
      }

      this.settings.serverHead = response.serverHead;
      this.settings.lastSyncedAt = new Date().toISOString();
      this.settings.localManifest = await vaultState.computeManifest();
      await this.saveSettings();

      new Notice(changes.length > 0 ? `Git sync complete: ${changes.length} local change(s)` : "Git sync complete");
      return [];
    });
    return conflicts ?? [];
  }

  async history(path?: string): Promise<HistoryEntry[]> {
    this.requireConfigured();
    const suffix = path ? `?path=${encodeURIComponent(path)}` : "";
    return this.getJson<HistoryEntry[]>(`${this.vaultPath()}/history${suffix}`);
  }

  async fileAtVersion(path: string, hash: string): Promise<VersionFileResponse> {
    this.requireConfigured();
    return this.getJson<VersionFileResponse>(`${this.vaultPath()}/file?path=${encodeURIComponent(path)}&hash=${encodeURIComponent(hash)}`);
  }

  async resolveFile(path: string): Promise<void> {
    await this.exclusive(async () => {
      this.requireConfigured();
      const buffer = await this.vault.adapter.readBinary(path);
      const uploadId = await this.uploadBuffer(path, buffer);
      const request: ResolveRequest = {
        clientId: this.settings.clientId,
        deviceName: this.deviceName(),
        files: [{ path, uploadId }]
      };
      const response = await this.postJson<SyncResponse>(`${this.vaultPath()}/resolve`, request);
      const vaultState = new VaultState(this.vault);
      await vaultState.applyServerFiles(response.files);
      this.settings.serverHead = response.serverHead;
      this.settings.lastSyncedAt = new Date().toISOString();
      this.settings.localManifest = await vaultState.computeManifest();
      await this.saveSettings();
      new Notice(response.status === "conflict" ? "Conflict remains after resolve attempt" : "Conflict resolution pushed");
    });
  }

  async resolveTextFile(path: string, content: string): Promise<void> {
    await this.vault.adapter.write(path, content);
    await this.resolveFile(path);
  }

  async beginOidcDeviceLogin(): Promise<OidcDeviceAuthorization> {
    const config = await this.serverOidcLoginConfig();

    const discovery = await this.oidcDiscovery();
    const params = new URLSearchParams();
    params.set("client_id", config.clientId);
    params.set("scope", config.scope || "openid profile email");
    if (config.audience) {
      params.set("audience", config.audience);
      params.set("resource", config.audience);
    }

    const response = await requestUrl({
      url: discovery.device_authorization_endpoint,
      method: "POST",
      contentType: "application/x-www-form-urlencoded",
      body: params.toString(),
      throw: false
    });
    if (response.status < 200 || response.status >= 300) {
      throw new Error(response.text || `OIDC device authorization failed: HTTP ${response.status}`);
    }
    return response.json as OidcDeviceAuthorization;
  }

  async pollOidcDeviceLogin(deviceCode: string, intervalSeconds: number, expiresInSeconds: number): Promise<void> {
    const discovery = await this.oidcDiscovery();
    const startedAt = Date.now();
    let interval = Math.max(intervalSeconds || 5, 1);

    while (Date.now() - startedAt < expiresInSeconds * 1000) {
      await sleep(interval * 1000);

      const params = new URLSearchParams();
      const config = await this.serverOidcLoginConfig();
      params.set("grant_type", "urn:ietf:params:oauth:grant-type:device_code");
      params.set("device_code", deviceCode);
      params.set("client_id", config.clientId);
      if (config.audience) {
        params.set("audience", config.audience);
        params.set("resource", config.audience);
      }

      const response = await requestUrl({
        url: discovery.token_endpoint,
        method: "POST",
        contentType: "application/x-www-form-urlencoded",
        body: params.toString(),
        throw: false
      });
      const body = response.json as OidcTokenResponse;

      if (response.status >= 200 && response.status < 300 && body.access_token) {
        this.settings.oidcAccessToken = body.access_token;
        await this.saveSettings();
        await this.loadAuthenticatedUser();
        new Notice("OIDC login complete");
        return;
      }

      if (body.error === "authorization_pending") continue;
      if (body.error === "slow_down") {
        interval += 5;
        continue;
      }
      if (body.error === "expired_token") {
        throw new Error("OIDC device login expired");
      }

      throw new Error(body.error_description || body.error || response.text || `OIDC token polling failed: HTTP ${response.status}`);
    }

    throw new Error("OIDC device login expired");
  }

  private async register(): Promise<void> {
    const request: RegisterRequest = {
      remoteUrl: this.settings.remoteUrl.trim(),
      branch: MAIN_BRANCH,
      authorName: this.settings.authorName,
      authorEmail: this.settings.authorEmail
    };
    await this.postJson<RegisterResponse>(`${this.vaultPath()}/register`, request);
    this.settings.branch = MAIN_BRANCH;
    await this.saveSettings();
  }

  private async uploadBuffer(path: string, buffer: ArrayBuffer, entry?: ManifestEntry): Promise<string> {
    const sha256 = entry?.sha256 ?? (await sha256Hex(buffer));
    const initRequest: UploadInitRequest = {
      path,
      sha256,
      size: buffer.byteLength
    };
    const init = await this.postJson<UploadInitResponse>(`${this.vaultPath()}/uploads`, initRequest);
    const chunkSize = Math.max(1, Math.min(init.chunkSize || 512 * 1024, 2 * 1024 * 1024));
    let offset = 0;
    while (offset < buffer.byteLength) {
      const chunk = buffer.slice(offset, Math.min(offset + chunkSize, buffer.byteLength));
      const response = await this.postJson<UploadChunkResponse>(`${this.vaultPath()}/uploads/${encodeURIComponent(init.uploadId)}/chunk`, {
        offset,
        contentBase64: arrayBufferToBase64(chunk)
      });
      offset = response.received;
    }

    const complete = await this.postJson<UploadCompleteResponse>(`${this.vaultPath()}/uploads/${encodeURIComponent(init.uploadId)}/complete`, {});
    if (complete.sha256 !== sha256 || complete.size !== buffer.byteLength) {
      throw new Error(`Upload verification failed for ${path}`);
    }
    return complete.uploadId;
  }

  private async postJson<T>(path: string, body: unknown): Promise<T> {
    const serverUrl = this.settings.serverUrl.replace(/\/+$/, "");
    const response = await requestUrl({
      url: `${serverUrl}${path}`,
      method: "POST",
      contentType: "application/json",
      headers: {
        Authorization: `Bearer ${this.settings.oidcAccessToken}`
      },
      body: JSON.stringify(body),
      throw: false
    });

    if (response.status < 200 || response.status >= 300) {
      const text = response.text || `HTTP ${response.status}`;
      throw new Error(text);
    }

    return response.json as T;
  }

  private async getJson<T>(path: string): Promise<T> {
    const serverUrl = this.settings.serverUrl.replace(/\/+$/, "");
    const response = await requestUrl({
      url: `${serverUrl}${path}`,
      method: "GET",
      headers: {
        Authorization: `Bearer ${this.settings.oidcAccessToken}`
      },
      throw: false
    });

    if (response.status < 200 || response.status >= 300) {
      const text = response.text || `HTTP ${response.status}`;
      throw new Error(text);
    }

    return response.json as T;
  }

  private async oidcDiscovery(): Promise<OidcDiscovery> {
    if (this.oidcDiscoveryCache) return this.oidcDiscoveryCache;
    const issuer = (await this.serverOidcLoginConfig()).issuer.replace(/\/+$/, "");
    const response = await requestUrl({
      url: `${issuer}/.well-known/openid-configuration`,
      method: "GET",
      throw: false
    });
    if (response.status < 200 || response.status >= 300) {
      throw new Error(response.text || `OIDC discovery failed: HTTP ${response.status}`);
    }
    const discovery = response.json as OidcDiscovery;
    if (!discovery.device_authorization_endpoint || !discovery.token_endpoint) {
      throw new Error("OIDC issuer does not advertise device authorization and token endpoints");
    }
    assertSecureHttpUrl(discovery.device_authorization_endpoint, "OIDC device authorization endpoint");
    assertSecureHttpUrl(discovery.token_endpoint, "OIDC token endpoint");
    this.oidcDiscoveryCache = discovery;
    return discovery;
  }

  private async serverOidcLoginConfig(): Promise<Extract<ServerAuthConfig, { type: "oidc" }>> {
    const serverUrl = this.settings.serverUrl.replace(/\/+$/, "");
    if (this.oidcLoginConfig && this.oidcLoginServerUrl === serverUrl) return this.oidcLoginConfig;
    const config = await this.authConfig();
    if (config.type !== "oidc") {
      throw new Error("This server is not configured for OIDC device login");
    }
    assertSecureHttpUrl(config.issuer, "OIDC issuer");
    this.oidcDiscoveryCache = null;
    this.oidcLoginConfig = config;
    this.oidcLoginServerUrl = serverUrl;
    return config;
  }

  private async exclusive<T>(operation: () => Promise<T>): Promise<T | undefined> {
    if (this.running) {
      new Notice("Git sync is already running");
      return undefined;
    }

    this.running = true;
    this.emitSyncState();
    try {
      return await operation();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      new Notice(`Git sync failed: ${message}`, 10000);
      throw error;
    } finally {
      this.running = false;
      this.emitSyncState();
    }
  }

  private requireConfigured(): void {
    if (!this.settings.serverUrl) throw new Error("Set a sync server URL before syncing");
    if (!this.settings.oidcAccessToken) throw new Error("Set an access token before syncing");
    if (!this.settings.userSlug) throw new Error("Set a user namespace before syncing");
    if (!this.settings.vaultSlug) throw new Error("Set a vault namespace before syncing");
    this.settings.branch = MAIN_BRANCH;
    assertSecureHttpUrl(this.settings.serverUrl, "Sync server URL");
    assertNamespaceSlug(this.settings.userSlug, "User namespace");
    assertNamespaceSlug(this.settings.vaultSlug, "Vault namespace");
    assertGitBranch(MAIN_BRANCH);
    if (/\s/.test(this.settings.oidcAccessToken)) throw new Error("Access token must not contain whitespace");
    if (this.settings.remoteUrl && (this.settings.remoteUrl.length > 2048 || /[\s\0]/.test(this.settings.remoteUrl))) {
      throw new Error("Git remote URL is invalid");
    }
  }

  private vaultPath(): string {
    return `/v1/users/${encodeURIComponent(this.settings.userSlug)}/vaults/${encodeURIComponent(this.settings.vaultSlug)}`;
  }

  private deviceName(): string {
    return getDeviceName(this.settings.deviceName);
  }

  private showConflictNotice(conflicts: SyncConflict[]): void {
    const notice = new Notice(`Git sync conflict in ${conflicts.length} file(s). Click to choose a resolution.`, 15000);
    if (!this.onConflictNotice) return;

    notice.messageEl.style.cursor = "pointer";
    notice.messageEl.title = "Open conflict resolver";
    notice.messageEl.tabIndex = 0;

    const openResolver = () => {
      notice.hide();
      this.onConflictNotice?.(conflicts);
    };
    notice.messageEl.onclick = openResolver;
    notice.messageEl.onkeydown = (event) => {
      if (event.key === "Enter" || event.key === " ") {
        event.preventDefault();
        openResolver();
      }
    };
  }

  private emitSyncState(): void {
    for (const listener of this.syncStateListeners) {
      listener(this.running);
    }
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}
