// Resolver: pick the live Knowledge/Import/Settings backend based on the runtime.
// - Under Tauri: real SQLite (knowledge), real config (settings), real Vision (import).
// - Under plain vite dev: Mock in-memory backend.
// Chat remains Mock in Phase 2 (out of scope).
import { isTauri } from './env';
import * as mock from './mockServices';
import * as tauri from './tauriServices';
import type { BackupService, ChatService, ImportService, KnowledgeService, SettingsService, SyncService, WebDavService } from './interfaces';

export const knowledgeService: KnowledgeService = isTauri ? tauri.knowledgeService : mock.knowledgeService;
export const importService: ImportService = isTauri ? tauri.importService : mock.importService;
export const chatService: ChatService = isTauri ? tauri.chatService : mock.chatService;
export const settingsService: SettingsService = isTauri ? tauri.settingsService : mock.settingsService;
export const backupService: BackupService = isTauri ? tauri.backupService : mock.backupService;
export const syncService: SyncService = isTauri ? tauri.syncService : mock.syncService;
export const webdavService: WebDavService = isTauri ? tauri.webdavService : mock.webdavService;