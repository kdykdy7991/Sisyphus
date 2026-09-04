export type Knowledge={id:string;question:string;answer:string;domain:string;topic:string;tags:string[];followUps:string[];relatedIds:string[];source:string;createdAt:string;updatedAt:string;favorite?:boolean};
export type ImportStatus='ready'|'queued'|'extracting'|'completed'|'review';
export type ImportImage={id:string;name:string;url:string;status:ImportStatus;drafts:Knowledge[]};
export type ChatMessage={id:string;role:'user'|'assistant';content:string;citations?:string[]};
export type Settings={apiBaseUrl:string;apiKey:string;chatModel:string;visionModel:string;databaseLocation:string};
