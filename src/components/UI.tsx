import type {ButtonHTMLAttributes,InputHTMLAttributes,ReactNode,TextareaHTMLAttributes} from 'react';
export function Button({variant='default',className='',...p}:ButtonHTMLAttributes<HTMLButtonElement>&{variant?:'default'|'primary'|'ghost'|'danger'}){return <button className={`btn btn-${variant} ${className}`} {...p}/>}
export function Input(p:InputHTMLAttributes<HTMLInputElement>){return <input className="input" {...p}/>}
export function Textarea(p:TextareaHTMLAttributes<HTMLTextAreaElement>){return <textarea className="textarea" {...p}/>}
export function Tag({children,onRemove}:{children:ReactNode;onRemove?:()=>void}){return <span className="tag">{children}{onRemove&&<button onClick={onRemove}>×</button>}</span>}
export function Empty({icon,heading,text,action}:{icon?:ReactNode;heading:string;text:string;action?:ReactNode}){return <div className="empty">{icon}<h3>{heading}</h3><p>{text}</p>{action}</div>}
export function Spinner({label='正在加载'}:{label?:string}){return <span className="spinner"><i/>{label}</span>}
