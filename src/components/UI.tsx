import type {ButtonHTMLAttributes,InputHTMLAttributes,ReactNode,TextareaHTMLAttributes} from 'react';
export function Button({variant='default',className='',...p}:ButtonHTMLAttributes<HTMLButtonElement>&{variant?:'default'|'primary'|'ghost'|'danger'}){return <button className={`btn btn-${variant} ${className}`} {...p}/>}
export function Input(p:InputHTMLAttributes<HTMLInputElement>){return <input className="input" {...p}/>}
export function Textarea(p:TextareaHTMLAttributes<HTMLTextAreaElement>){return <textarea className="textarea" {...p}/>}
export function Tag({children,onRemove}:{children:ReactNode;onRemove?:()=>void}){return <span className="tag">{children}{onRemove&&<button onClick={onRemove}>×</button>}</span>}
export function Empty({icon,heading,text,action}:{icon?:ReactNode;heading:string;text:string;action?:ReactNode}){return <div className="empty">{icon}<h3>{heading}</h3><p>{text}</p>{action}</div>}

/**
 * Lightweight confirm dialog. Backed by the same `restore-overlay` /
 * `restore-card` style used by Settings (black/white editorial, blurred
 * backdrop) so it visually matches existing destructive confirms. Caller
 * owns `open` state and `onConfirm`/`onCancel`; this component is just the
 * shell + the two buttons. `destructive` flips the primary button to the
 * `danger` variant so the weight matches the action.
 */
export function Confirm({
  open,
  title,
  body,
  confirmText = '确认',
  cancelText = '取消',
  destructive = false,
  onConfirm,
  onCancel,
}: {
  open: boolean;
  title: string;
  body?: ReactNode;
  confirmText?: string;
  cancelText?: string;
  destructive?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}) {
  if (!open) return null;
  return (
    <div className="restore-overlay" role="dialog" aria-modal="true" onClick={onCancel}>
      <div className="restore-card confirm-card" onClick={e => e.stopPropagation()}>
        <h3>{title}</h3>
        {body && <p className="restore-lead">{body}</p>}
        <div className="restore-actions">
          <Button variant="ghost" onClick={onCancel}>
            {cancelText}
          </Button>
          <Button variant={destructive ? 'danger' : 'primary'} onClick={onConfirm}>
            {confirmText}
          </Button>
        </div>
      </div>
    </div>
  );
}
export function Spinner({label='正在加载'}:{label?:string}){return <span className="spinner"><i/>{label}</span>}
