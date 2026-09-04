import {createContext,useContext,useEffect,useState,type ReactNode} from 'react';import type {ImportImage,Knowledge} from './types';import {knowledgeService} from './services/mockServices';
type Ctx={knowledge:Knowledge[];refresh:()=>Promise<void>;imports:ImportImage[];setImports:React.Dispatch<React.SetStateAction<ImportImage[]>>};
const AppCtx=createContext<Ctx|null>(null);
export function AppProvider({children}:{children:ReactNode}){const[knowledge,setKnowledge]=useState<Knowledge[]>([]);const[imports,setImports]=useState<ImportImage[]>([]);const refresh=async()=>setKnowledge(await knowledgeService.list());useEffect(()=>{refresh()},[]);return <AppCtx.Provider value={{knowledge,refresh,imports,setImports}}>{children}</AppCtx.Provider>}
export const useApp=()=>{const v=useContext(AppCtx);if(!v)throw Error('AppProvider missing');return v};
