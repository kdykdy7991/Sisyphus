import {createContext,useContext,useEffect,useState,type ReactNode} from 'react';import type {ImportImage,Knowledge} from './types';import {knowledgeService} from './services';
type Ctx={knowledge:Knowledge[];topics:string[];refresh:()=>Promise<void>;imports:ImportImage[];setImports:React.Dispatch<React.SetStateAction<ImportImage[]>>};
const AppCtx=createContext<Ctx|null>(null);
export function AppProvider({children}:{children:ReactNode}){const[knowledge,setKnowledge]=useState<Knowledge[]>([]);const[topics,setTopics]=useState<string[]>([]);const[imports,setImports]=useState<ImportImage[]>([]);const refresh=async()=>{const[items,names]=await Promise.all([knowledgeService.list(),knowledgeService.listTopics()]);setKnowledge(items);setTopics(names)};useEffect(()=>{refresh()},[]);return <AppCtx.Provider value={{knowledge,topics,refresh,imports,setImports}}>{children}</AppCtx.Provider>}
export const useApp=()=>{const v=useContext(AppCtx);if(!v)throw Error('AppProvider missing');return v};
