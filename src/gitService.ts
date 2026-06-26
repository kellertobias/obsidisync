import { Notice, requestUrl, Vault } from "obsidian";
import { arrayBufferToBase64 } from "./base64";
import {
  HistoryEntry,
  RegisterRequest,
  RegisterResponse,
  ResolveRequest,
  SyncRequest,
  SyncResponse,
  VersionFileResponse
} from "./protocol";
import { getDeviceName } from "./runtime";
import { IosGitSyncSettings } from "./settings";
import { assertGitBranch, assertNamespaceSlug, assertSecureHttpUrl } from "./security";
import { VaultState } from "./vaultState";

type SaveSettings = () => Promise<void>;

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

export class GitService {
  private running = false;
  private oidcDiscoveryCache: OidcDiscovery | null = null;

  constructor(
    private readonly vault: Vault,
    private settings: IosGitSyncSettings,
    private readonly saveSettings: SaveSettings
  ) {}

  updateSettings(settings: IosGitSyncSettings): void {
    this.settings = settings;
  }

  async sync(): Promise<void> {
    await this.exclusive(async () => {
      this.requireConfigured();
      await this.register();

      const vaultState = new VaultState(this.vault);
      const { manifest, changes } = await vaultState.collectChanges(this.settings.localManifest);
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
        new Notice(`Git sync conflict in ${response.conflicts.length} file(s). Resolve markers, then sync again.`, 15000);
        return;
      }

      this.settings.serverHead = response.serverHead;
      this.settings.localManifest = await vaultState.computeManifest();
      await this.saveSettings();

      new Notice(changes.length > 0 ? `Git sync complete: ${changes.length} local change(s)` : "Git sync complete");
    });
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
      const request: ResolveRequest = {
        clientId: this.settings.clientId,
        deviceName: this.deviceName(),
        files: [{ path, contentBase64: arrayBufferToBase64(buffer) }]
      };
      const response = await this.postJson<SyncResponse>(`${this.vaultPath()}/resolve`, request);
      const vaultState = new VaultState(this.vault);
      await vaultState.applyServerFiles(response.files);
      this.settings.serverHead = response.serverHead;
      this.settings.localManifest = await vaultState.computeManifest();
      await this.saveSettings();
      new Notice(response.status === "conflict" ? "Conflict remains after resolve attempt" : "Conflict resolution pushed");
    });
  }

  async beginOidcDeviceLogin(): Promise<OidcDeviceAuthorization> {
    if (!this.settings.oidcIssuer) throw new Error("Set an OIDC issuer before starting login");
    if (!this.settings.oidcClientId) throw new Error("Set an OIDC client ID before starting login");
    assertSecureHttpUrl(this.settings.oidcIssuer, "OIDC issuer");

    const discovery = await this.oidcDiscovery();
    const params = new URLSearchParams();
    params.set("client_id", this.settings.oidcClientId);
    params.set("scope", this.settings.oidcScope || "openid profile email");
    if (this.settings.oidcAudience) {
      params.set("audience", this.settings.oidcAudience);
      params.set("resource", this.settings.oidcAudience);
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
      params.set("grant_type", "urn:ietf:params:oauth:grant-type:device_code");
      params.set("device_code", deviceCode);
      params.set("client_id", this.settings.oidcClientId);

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
      remoteUrl: this.settings.remoteUrl,
      branch: this.settings.branch,
      authorName: this.settings.authorName,
      authorEmail: this.settings.authorEmail
    };
    const response = await this.postJson<RegisterResponse>(`${this.vaultPath()}/register`, request);
    this.settings.branch = response.branch;
    await this.saveSettings();
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
    const issuer = this.settings.oidcIssuer.replace(/\/+$/, "");
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

  private async exclusive<T>(operation: () => Promise<T>): Promise<T | undefined> {
    if (this.running) {
      new Notice("Git sync is already running");
      return undefined;
    }

    this.running = true;
    try {
      return await operation();
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      new Notice(`Git sync failed: ${message}`, 10000);
      throw error;
    } finally {
      this.running = false;
    }
  }

  private requireConfigured(): void {
    if (!this.settings.serverUrl) throw new Error("Set a sync server URL before syncing");
    if (!this.settings.oidcAccessToken) throw new Error("Set an OIDC access token before syncing");
    if (!this.settings.userSlug) throw new Error("Set a user namespace before syncing");
    if (!this.settings.vaultSlug) throw new Error("Set a vault namespace before syncing");
    if (!this.settings.remoteUrl) throw new Error("Set a Git remote URL before registering this vault");
    if (!this.settings.branch) throw new Error("Set a branch before syncing");
    assertSecureHttpUrl(this.settings.serverUrl, "Sync server URL");
    assertNamespaceSlug(this.settings.userSlug, "User namespace");
    assertNamespaceSlug(this.settings.vaultSlug, "Vault namespace");
    assertGitBranch(this.settings.branch);
    if (/\s/.test(this.settings.oidcAccessToken)) throw new Error("OIDC access token must not contain whitespace");
    if (this.settings.remoteUrl.length > 2048 || /[\s\0]/.test(this.settings.remoteUrl)) {
      throw new Error("Git remote URL is invalid");
    }
  }

  private vaultPath(): string {
    return `/v1/users/${encodeURIComponent(this.settings.userSlug)}/vaults/${encodeURIComponent(this.settings.vaultSlug)}`;
  }

  private deviceName(): string {
    return getDeviceName(this.settings.deviceName);
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => window.setTimeout(resolve, ms));
}
