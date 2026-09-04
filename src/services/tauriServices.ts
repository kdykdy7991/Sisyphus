import { invoke } from '@tauri-apps/api/core';
import type { ChatMessage, ChatResult, ImportImage, Knowledge, Settings, SimilaritySuggestion } from '../types';
import type { ChatService, ImportService, KnowledgeService, SettingsService } from './interfaces';

// Phase 2: the Knowledge store lives in SQLite behind Tauri commands.
// Data flows React -> KnowledgeService -> invoke -> Rust command -> rusqlite.

export const knowledgeService: KnowledgeService = {
  async list(): Promise<Knowledge[]> {
    return invoke('knowledge_list');
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