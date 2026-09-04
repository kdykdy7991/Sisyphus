import type {ChatMessage,ConnectionResult,ImportImage,Knowledge,Settings,SimilaritySuggestion} from '../types';
export interface KnowledgeService{list():Promise<Knowledge[]>;get(id:string):Promise<Knowledge|undefined>;search(query:string):Promise<Knowledge[]>;save(item:Knowledge):Promise<Knowledge>;recent(limit?:number):Promise<Knowledge[]>}
export interface ImportService{extract(images:ImportImage[],onProgress:(images:ImportImage[])=>void):Promise<ImportImage[]>;confirm(items:Knowledge[]):Promise<void>;suggestSimilarity(draft:Knowledge):Promise<SimilaritySuggestion|null>;updateExisting(draft:Knowledge,existingId:string):Promise<void>}
export interface ChatService{ask(question:string,scope:string,history:ChatMessage[]):Promise<ChatMessage>}
export interface SettingsService{get():Promise<Settings>;save(value:Settings):Promise<void>;testConnection():Promise<ConnectionResult>}
