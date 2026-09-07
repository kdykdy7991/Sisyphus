export type Knowledge={id:string;question:string;answer:string;domain:string;topic:string;tags:string[];followUps:string[];relatedIds:string[];source:string;createdAt:string;updatedAt:string;favorite?:boolean;lastReadAt?:string;similarity?:SimilaritySuggestion};
export type SimilarityRelation='SAME'|'RELATED'|'NONE';
export type SimilaritySuggestion={relation:SimilarityRelation;knowledgeId:string|null;reason:string;matchedQuestion?:string;matchedDomain?:string;matchedTopic?:string};
export type ImportStatus='ready'|'queued'|'extracting'|'completed'|'failed'|'review';
export type ImportImage={id:string;name:string;url:string;status:ImportStatus;drafts:Knowledge[];error?:string};
export type ChatMessage={id:string;role:'user'|'assistant';content:string;citations?:string[]};
export type ChatCitation={knowledgeId:string;question:string};
export type ChatResult={answer:string;citations:ChatCitation[]};
export type Settings={apiBaseUrl:string;apiKey:string;chatModel:string;visionModel:string;databaseLocation:string};
export type ConnectionResult={ok:boolean;message:string};
export type KnowledgeExportSummary={path:string;itemCount:number};

// Backup / Restore — Knowledge data only. Application configuration is not
// part of a backup and is not modified by restore.
export type BackupInspect={formatVersion:number;app:string;createdAt:string;databaseSchemaVersion:number;knowledgeCount:number;domainCount:number};
export type BackupSummary={path:string;formatVersion:number;createdAt:string;knowledgeCount:number;domainCount:number};
export type BackupRestoreResult={knowledgeCount:number;domainCount:number};

// Sync (WebDAV transport). Device configuration only — never part of a
// snapshot or a backup. The backend never returns the password, so the UI
// only ever *sends* it (empty = keep the stored value).
export type WebDavConfig = {
  url: string;
  username: string;
  password: string;
};

// Result of `webdav_test_connection`. Credential-free by construction.
export type WebDavTestResult = {
  success: boolean;
  latencyMs: number;
  message: string;
  errorKind: string;
};

// Summary of `sync_webdav`. Counts come from the shared merge engine.
export type WebDavSyncSummary = {
  remoteExisted: boolean;
  downloadedItemCount: number;
  uploadedItemCount: number;
  stats: MergeStats;
  syncedAt: string;
  retryCount: number;
};

// Sync (local file transport). The `.iksync` file is a full JSON snapshot
// of the local knowledge state, including soft-deleted rows. Sync merges
// incoming data into the local DB; it never replaces. Each device keeps
// its own complete database — WebDAV / file transport are just media.
export type SyncInspectReport={
  path:string;
  formatVersion:number;
  app:string;
  createdAt:string;
  sourceDevice:string;
  itemCount:number;
  activeCount:number;
  deletedCount:number;
  bytesRead:number;
};
export type SyncExportSummary={
  path:string;
  formatVersion:number;
  createdAt:string;
  itemCount:number;
  activeCount:number;
  deletedCount:number;
};
export type MergeStats={
  inserted:number;
  updated:number;
  deleted:number;
  skipped:number;
  conflicts:number;
};
export type SyncImportSummary={
  path:string;
  formatVersion:number;
  snapshotCreatedAt:string;
  snapshotItemCount:number;
  snapshotActiveCount:number;
  snapshotDeletedCount:number;
  bytesRead:number;
  stats:MergeStats;
};
