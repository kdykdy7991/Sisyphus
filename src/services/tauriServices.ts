import { invoke } from '@tauri-apps/api/core';
import { save as saveDialog, open as openDialog } from '@tauri-apps/plugin-dialog';
import type { BackupInspect, BackupRestoreResult, BackupSummary, ChatMessage, ChatResult, ImportImage, Knowledge, KnowledgeExportSummary, LlmProfiles, Settings, SimilaritySuggestion, SyncExportSummary, SyncImportSummary, SyncInspectReport, WebDavConfig, WebDavSyncSummary, WebDavTestResult } from '../types';
import type { BackupService, ChatService, ImportService, KnowledgeService, SettingsService, SyncService, WebDavService } from './interfaces';

// Phase 2: the Knowledge store lives in SQLite behind Tauri commands.
// Data flows React -> KnowledgeService -> invoke -> Rust command -> rusqlite.

export const knowledgeService: KnowledgeService = {
  async list(): Promise<Knowledge[]> {
    return invoke('knowledge_list');
  },
  async listTopics(): Promise<string[]> {
    return invoke('topic_list');
  },
  async createTopic(name): Promise<void> {
    await invoke('topic_create', { name });
  },
  async deleteTopic(name): Promise<boolean> {
    return invoke<boolean>('topic_delete', { name });
  },
  async get(id): Promise<Knowledge | undefined> {
    const result = await invoke<Knowledge | null>('knowledge_get', { id });
    return result ?? undefined;
  },
  async search(query): Promise<Knowledge[]> {
    return invoke('knowledge_search', { query });
  },
  async save(item): Promise<Knowledge> {
    return invoke('knowledge_save', { item });
  },
  async recent(limit = 10): Promise<Knowledge[]> {
    return invoke('knowledge_recent', { limit });
  },
  // Soft-delete a single knowledge item. Returns true when the row was
  // actually transitioned active → tombstoned; false when the id is unknown
  // or already tombstoned. The UI guards this with an explicit confirm.
  async delete(id): Promise<boolean> {
    return invoke<boolean>('knowledge_delete', { id });
  },
  // Irreversible: wipes knowledge_items plus derived tags/topics/FTS and
  // returns how many items were removed. Guarded by a confirm in Settings.
  async clear(): Promise<number> {
    return invoke('knowledge_clear');
  },
  async exportMarkdown(suggestedName, ids): Promise<KnowledgeExportSummary | null> {
    const destinationPath = await saveDialog({
      title: '导出知识库文档',
      defaultPath: suggestedName,
      filters: [{ name: 'Markdown 文档', extensions: ['md'] }],
    });
    if (typeof destinationPath !== 'string') return null;
    return invoke<KnowledgeExportSummary>('knowledge_export_markdown', { destinationPath, ids });
  },
};

// Real settings: load/save persist to the backend config file (app-data). The
// API key is never returned to the UI (backend blanks it), so the Settings page
// only ever shows a masked placeholder.
export const settingsService: SettingsService = {
  async get(): Promise<Settings> {
    return invoke('settings_get');
  },
  async save(value: Settings): Promise<void> {
    await invoke('settings_save', { config: value });
  },
  async profiles() { return invoke<LlmProfiles>('settings_profiles'); },
  async createProfile(name) { return invoke<string>('settings_profile_create', { name }); },
  async renameProfile(id, name) { await invoke('settings_profile_rename', { id, name }); },
  async switchProfile(id) { return invoke<Settings>('settings_profile_switch', { id }); },
  async deleteProfile(id) { await invoke('settings_profile_delete', { id }); },
  async testConnection() {
    return invoke('settings_test_connection');
  },
};

// Real Knowledge Chat: scoped retrieval + Chat model happen in the Rust command
// (knowledge_chat). Here we just map the structured {answer, citations} back to
// the UI's ChatMessage shape (citations as the referenced knowledge ids).
export const chatService: ChatService = {
  async ask(question, scope, history): Promise<ChatMessage> {
    const result = await invoke<ChatResult>('knowledge_chat', {
      question,
      scope,
      history: history.map(({ role, content }) => ({ role, content })),
    });
    return {
      id: `a-${Date.now()}`,
      role: 'assistant',
      content: result.answer,
      citations: result.citations.map(c => c.knowledgeId),
    };
  },
};

// Read an in-memory Blob object URL back into a base64 data URL so it can be
// handed to the Rust `vision_extract` command. The bytes only ever live in the
// frontend's object URL + the request payload — nothing is written to disk.
const blobToDataUrl = (url: string): Promise<string> =>
  fetch(url)
    .then(r => r.blob())
    .then(
      blob =>
        new Promise<string>((resolve, reject) => {
          const reader = new FileReader();
          reader.onload = () => resolve(reader.result as string);
          reader.onerror = () => reject(new Error('无法读取图片内容。'));
          reader.readAsDataURL(blob);
        }),
    );

// Real Vision extraction, one image at a time (batch independent). Each image
// has its own status (queued -> extracting -> completed | failed); a single
// failure never aborts the batch — later images still get extracted and their
// drafts stay usable. Re-running extract on already-completed images (retry) is
// a no-op for them. Image order is never used to merge content.
export const importService: ImportService = {
  async extract(images, onProgress) {
    // Keep already-extracted images as-is (retry), reset the rest to queued.
    let next: ImportImage[] = images.map(im =>
      im.drafts.length > 0 && (im.status === 'completed' || im.status === 'review')
        ? { ...im, status: 'review' as const }
        : { ...im, status: 'queued' as const, error: undefined },
    );
    onProgress([...next]);

    for (let i = 0; i < next.length; i++) {
      if (next[i].status === 'review') continue; // already had drafts
      next[i] = { ...next[i], status: 'extracting' as const, error: undefined };
      onProgress([...next]);
      try {
        const dataUrl = await blobToDataUrl(next[i].url);
        const drafts = await invoke<Knowledge[]>('vision_extract', {
          imageDataUrl: dataUrl,
          source: next[i].name,
        });
        next[i] = { ...next[i], status: 'completed' as const, drafts };
      } catch (err) {
        next[i] = { ...next[i], status: 'failed' as const, error: String(err) };
      }
      onProgress([...next]);
    }

    // Completed images move into the Review stage for user sign-off.
    next = next.map(im => (im.status === 'completed' ? { ...im, status: 'review' as const } : im));
    onProgress([...next]);
    return next;
  },
  async confirm(items: Knowledge[]): Promise<void> {
    for (const item of items) {
      await knowledgeService.save(item);
    }
  },
  async suggestSimilarity(draft: Knowledge): Promise<SimilaritySuggestion | null> {
    return invoke<SimilaritySuggestion | null>('analyze_similarity', { item: draft });
  },
  // SAME -> "update existing": merge the (edited) draft into the existing row,
  // keeping the existing numeric id, source, createdAt, favorite and relatedIds.
  async updateExisting(draft: Knowledge, existingId: string): Promise<void> {
    const existing = await knowledgeService.get(existingId);
    if (!existing) throw new Error('要更新的已有知识不存在。');
    await knowledgeService.save({
      ...existing,
      question: draft.question.trim(),
      answer: draft.answer.trim(),
      domain: draft.domain.trim(),
      topic: draft.topic.trim(),
      tags: draft.tags.map(t => t.trim()).filter(Boolean),
      followUps: draft.followUps,
      updatedAt: new Date().toLocaleString(),
    });
  },
};

// Backup / Restore — see `src-tauri/src/backup.rs` for the file format and
// the security model (no API Key, no transient chat / upload data, atomic
// file replace with a safety-snapshot rollback path).
export const backupService: BackupService = {
  async pickSavePath(suggestedName: string): Promise<string | null> {
    const result = await saveDialog({
      title: '保存 Sisyphus 备份',
      defaultPath: suggestedName,
      filters: [{ name: 'Sisyphus Backup', extensions: ['ikbackup'] }],
    });
    return typeof result === 'string' ? result : null;
  },
  async pickOpenPath(): Promise<string | null> {
    const result = await openDialog({
      title: '选择 Sisyphus 备份',
      multiple: false,
      directory: false,
      filters: [{ name: 'Sisyphus Backup', extensions: ['ikbackup'] }],
    });
    if (Array.isArray(result)) return result[0] ?? null;
    return typeof result === 'string' ? result : null;
  },
  async create(destinationPath: string): Promise<BackupSummary> {
    return invoke<BackupSummary>('backup_create', { destinationPath });
  },
  async inspect(sourcePath: string): Promise<BackupInspect> {
    return invoke<BackupInspect>('backup_inspect', { sourcePath });
  },
  async restore(sourcePath: string): Promise<BackupRestoreResult> {
    return invoke<BackupRestoreResult>('backup_restore', { sourcePath });
  },
};

// Sync (local file transport) — see `src-tauri/src/sync.rs`. The
// `.iksync` file is a JSON snapshot of the current local knowledge
// state (active rows + tombstones). Imports merge into the local DB;
// re-importing the same file is a no-op (everything is Skip). The Rust
// side handles parse / validate / plan / apply; the UI only routes
// paths and surfaces the returned stats.
export const syncService: SyncService = {
  async pickSavePath(suggestedName: string): Promise<string | null> {
    const result = await saveDialog({
      title: '导出 Sisyphus 同步数据',
      defaultPath: suggestedName,
      filters: [{ name: 'Sisyphus Sync', extensions: ['iksync'] }],
    });
    return typeof result === 'string' ? result : null;
  },
  async pickOpenPath(): Promise<string | null> {
    const result = await openDialog({
      title: '选择 Sisyphus 同步数据',
      multiple: false,
      directory: false,
      filters: [{ name: 'Sisyphus Sync', extensions: ['iksync'] }],
    });
    if (Array.isArray(result)) return result[0] ?? null;
    return typeof result === 'string' ? result : null;
  },
  async export(destinationPath: string): Promise<SyncExportSummary> {
    return invoke<SyncExportSummary>('sync_export_local', { destinationPath });
  },
  async inspect(sourcePath: string): Promise<SyncInspectReport> {
    return invoke<SyncInspectReport>('sync_inspect', { sourcePath });
  },
  async import(sourcePath: string): Promise<SyncImportSummary> {
    return invoke<SyncImportSummary>('sync_import_local', { sourcePath });
  },
};

// Sync (WebDAV transport). The password is never returned by the backend
// (`webdav_config_get` blanks it), so the UI only ever sends it; an empty
// password on `saveConfig` means "keep the stored one". `testConnection`
// probes the endpoint using the *saved* config, so the flow is
// save-then-test to validate freshly typed values.
export const webdavService: WebDavService = {
  async getConfig(): Promise<WebDavConfig> {
    return invoke<WebDavConfig>('webdav_config_get');
  },
  async saveConfig(config: WebDavConfig): Promise<void> {
    await invoke('webdav_config_save', { config });
  },
  async testConnection(): Promise<WebDavTestResult> {
    return invoke<WebDavTestResult>('webdav_test_connection');
  },
  async sync(): Promise<WebDavSyncSummary> {
    return invoke<WebDavSyncSummary>('sync_webdav');
  },
};

// Diagnostic log access. The Rust side writes every LLM failure to
// `app_data_dir/logs/app-YYYY-MM-DD.log`; these commands let the Settings
// page open that folder in Finder/Explorer or display the path as text.
export const logsService = {
  async openDir(): Promise<void> {
    await invoke('log_open_dir');
  },
  async getDir(): Promise<string> {
    return invoke<string>('log_get_dir');
  },
};
