// Resolver: pick the live Knowledge/Import/Settings backend based on the runtime.
// - Under Tauri: real SQLite (knowledge), real config (settings), real Vision (import).
// - Under plain vite dev: Mock in-memory backend.
// Chat remains Mock in Phase 2 (out of scope).
import { isTauri } from './env';
import * as mock from './mockServices';
import * as tauri from './tauriServices';
import type { ChatService, ImportService, KnowledgeService, SettingsService } from './interfaces';

export const knowledgeService: KnowledgeService = isTauri ? tauri.knowledgeService : mock.knowledgeService;
export const importService: ImportService = isTauri ? tauri.importService : mock.importService;
export const chatService: ChatService = isTauri ? tauri.chatService : mock.chatService;
export const settingsService: SettingsService = isTauri ? tauri.settingsService : mock.settingsService;