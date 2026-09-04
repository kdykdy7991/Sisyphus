import type {ChatMessage,ImportImage,Knowledge,Settings} from '../types';
export interface KnowledgeService{list():Promise<Knowledge[]>;get(id:string):Promise<Knowledge|undefined>;search(query:string):Promise<Knowledge[]>;save(item:Knowledge):Promise<Knowledge>}
export interface ImportService{extract(images:ImportImage[],mode:'independent'|'continuous',onProgress:(images:ImportImage[])=>void):Promise<ImportImage[]>;confirm(items:Knowledge[]):Promise<void>}
export interface ChatService{ask(question:string,scope:string,history:ChatMessage[]):Promise<ChatMessage>}
export interface SettingsService{get():Promise<Settings>;save(value:Settings):Promise<void>;testConnection():Promise<boolean>}
