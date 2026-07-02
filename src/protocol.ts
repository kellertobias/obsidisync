export type SyncStatus = "ok" | "conflict";

export interface ManifestEntry {
  path: string;
  sha256: string;
  mtime: number;
  size: number;
}

export type ClientChange =
  | {
      path: string;
      op: "upsert";
      contentBase64?: string;
      uploadId?: string;
      sha256?: string;
      mtime?: number;
    }
  | {
      path: string;
      op: "delete";
    };

export type ServerFileChange =
  | {
      path: string;
      op: "upsert";
      contentBase64: string;
      sha256: string;
    }
  | {
      path: string;
      op: "delete";
    };

export interface RegisterRequest {
  remoteUrl: string;
  branch: string;
  authorName: string;
  authorEmail: string;
}

export interface RegisterResponse {
  user: string;
  vault: string;
  serverHead: string | null;
  branch: string;
}

export interface SyncRequest {
  baseHead: string | null;
  clientId: string;
  deviceName: string;
  changes: ClientChange[];
  clientManifest: ManifestEntry[];
}

export interface SyncConflict {
  path: string;
  reason: string;
}

export interface SyncResponse {
  status: SyncStatus;
  serverHead: string | null;
  files: ServerFileChange[];
  conflicts: SyncConflict[];
}

export interface UploadInitRequest {
  path: string;
  sha256: string;
  size: number;
}

export interface UploadInitResponse {
  uploadId: string;
  chunkSize: number;
}

export interface UploadChunkRequest {
  offset: number;
  contentBase64: string;
}

export interface UploadChunkResponse {
  uploadId: string;
  received: number;
}

export interface UploadCompleteResponse {
  uploadId: string;
  size: number;
  sha256: string;
}

export interface ResolveRequest {
  clientId: string;
  deviceName: string;
  files: Array<{
    path: string;
    contentBase64?: string;
    uploadId?: string;
  }>;
}

export interface HistoryEntry {
  hash: string;
  date: string;
  author: string;
  subject: string;
  versionNumber: number;
  name?: string | null;
  squashedIntoHash?: string | null;
  deviceName?: string | null;
}

export interface DeviceEntry {
  clientId: string;
  deviceName: string;
  lastSyncedHead: string;
  lastSyncedAt: string;
}

export interface DeviceVersionEntry {
  clientId: string;
  deviceName: string;
  hash: string | null;
  versionNumber: number | null;
}

export interface VersionMetadataRequest {
  path: string;
  hash: string;
  name?: string | null;
  clearName?: boolean;
  squashedIntoHash?: string | null;
}

export interface VersionFileResponse {
  path: string;
  hash: string;
  contentBase64: string;
  sha256: string;
  readOnly: boolean;
}

export interface ServerInfoResponse {
  name: string;
  version: string;
  apiVersion: number;
  minClientApiVersion: number;
}
