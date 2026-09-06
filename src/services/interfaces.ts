import type {BackupInspect,BackupRestoreResult,BackupSummary,ChatMessage,ConnectionResult,ImportImage,Knowledge,Settings,SimilaritySuggestion,SyncExportSummary,SyncImportSummary,SyncInspectReport,WebDavConfig,WebDavSyncSummary,WebDavTestResult} from '../types';
export interface KnowledgeService{list():Promise<Knowledge[]>;get(id:string):Promise<Knowledge|undefined>;search(query:string):Promise<Knowledge[]>;save(item:Knowledge):Promise<Knowledge>;recent(limit?:number):Promise<Knowledge[]>;delete(id:string):Promise<boolean>;clear():Promise<number>}
export interface ImportService{extract(images:ImportImage[],onProgress:(images:ImportImage[])=>void):Promise<ImportImage[]>;confirm(items:Knowledge[]):Promise<void>;suggestSimilarity(draft:Knowledge):Promise<SimilaritySuggestion|null>;updateExisting(draft:Knowledge,existingId:string):Promise<void>}
export interface ChatService{ask(question:string,scope:string,history:ChatMessage[]):Promise<ChatMessage>}
export interface SettingsService{get():Promise<Settings>;save(value:Settings):Promise<void>;testConnection():Promise<ConnectionResult>}
// Backup / Restore: pickSavePath() returns the user-chosen destination
// (or null when they cancelled); pickOpenPath() returns the selected
// backup file (or null). The actual file IO is done by the Rust side.
export interface BackupService{
  pickSavePath(suggestedName:string):Promise<string|null>;
  pickOpenPath():Promise<string|null>;
  create(destinationPath:string):Promise<BackupSummary>;
  inspect(sourcePath:string):Promise<BackupInspect>;
  restore(sourcePath:string):Promise<BackupRestoreResult>;
}
// Sync (local file transport). The `.iksync` file is a full JSON
// snapshot; the file picker filters on the `iksync` extension. Imports
// merge into the local DB (never replace); re-importing the same file
// is idempotent.
export interface SyncService{
  pickSavePath(suggestedName:string):Promise<string|null>;
  pickOpenPath():Promise<string|null>;
  export(destinationPath:string):Promise<SyncExportSummary>;
  inspect(sourcePath:string):Promise<SyncInspectReport>;
  import(sourcePath:string):Promise<SyncImportSummary>;
}

// Sync (WebDAV transport). Credentials are device config and are never read
// back: `getConfig` returns an empty password, and `saveConfig` accepts an
// empty password to mean "keep the stored one". `testConnection` probes the
// configured endpoint with the saved config (so `sync` calls it after saving).
export interface WebDavService{
  getConfig():Promise<WebDavConfig>;
  saveConfig(config:WebDavConfig):Promise<void>;
  testConnection():Promise<WebDavTestResult>;
  sync():Promise<WebDavSyncSummary>;
}

// Diagnostic log access. `openDir` asks the OS to reveal the log folder in
// Finder/Explorer; `getDir` returns the absolute path as text so the UI
// can show it when the OS file manager fails to launch (sandboxed envs).
export interface LogsService{
  openDir():Promise<void>;
  getDir():Promise<string>;
}
