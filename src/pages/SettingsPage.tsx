import { useEffect, useState } from 'react';
import { useApp } from '../AppContext';
import { backupService, knowledgeService, settingsService } from '../services';
import type { BackupInspect, Settings } from '../types';
import { AlertCircle, Check, Database, Download, Eye, EyeOff, FolderOpen, LoaderCircle, RotateCcw, Trash2 } from '../components/Icons';
import { Button, Input } from '../components/UI';

type Flash = { kind: 'ok' | 'err'; text: string } | null;
type RestoreDialog =
  | { kind: 'idle' }
  | { kind: 'inspecting'; path: string }
  | { kind: 'confirm'; path: string; report: BackupInspect }
  | { kind: 'restoring'; path: string };

const todayName = () => {
  const d = new Date();
  const m = String(d.getMonth() + 1).padStart(2, '0');
  const day = String(d.getDate()).padStart(2, '0');
  return `interview-kit-backup-${d.getFullYear()}-${m}-${day}.ikbackup`;
};

export function SettingsPage() {
  const [s, setS] = useState<Settings>();
  const [show, setShow] = useState(false);
  const [state, setState] = useState('');
  const [msg, setMsg] = useState('');
  const [flash, setFlash] = useState<Flash>(null);

  const [busy, setBusy] = useState<'backup' | 'restore' | null>(null);
  const [restore, setRestore] = useState<RestoreDialog>({ kind: 'idle' });

  // Clear-the-base (danger zone): irreversible, so it gets its own confirm.
  const { knowledge, refresh } = useApp();
  const [confirmClear, setConfirmClear] = useState(false);
  const [clearing, setClearing] = useState(false);

  useEffect(() => {
    settingsService.get().then(setS);
  }, []);

  if (!s) return null;

  const set = (k: keyof Settings, v: string) => setS({ ...s, [k]: v });

  const test = async () => {
    setState('testing');
    setMsg('');
    const r = await settingsService.testConnection().catch((e) => {
      setState('');
      setMsg(String(e));
      return null;
    });
    if (!r) return;
    setState(r.ok ? 'ok' : 'fail');
    setMsg(r.message);
  };

  const save = async () => {
    await settingsService.save(s).catch((e) => {
      setState('');
      setMsg(String(e));
    });
    setState('saved');
    setTimeout(() => setState(''), 1500);
  };

  const startBackup = async () => {
    setBusy('backup');
    setFlash(null);
    try {
      const dest = await backupService.pickSavePath(todayName());
      if (!dest) {
        setBusy(null);
        return;
      }
      const summary = await backupService.create(dest);
      setFlash({
        kind: 'ok',
        text: `备份完成：${summary.knowledgeCount} 条知识、${summary.domainCount} 个领域，已保存到 ${summary.path}`,
      });
    } catch (e) {
      setFlash({ kind: 'err', text: `备份失败：${String(e)}` });
    } finally {
      setBusy(null);
    }
  };

  const startRestore = async () => {
    setBusy('restore');
    setFlash(null);
    try {
      const source = await backupService.pickOpenPath();
      if (!source) {
        setBusy(null);
        return;
      }
      setRestore({ kind: 'inspecting', path: source });
      const report = await backupService.inspect(source);
      setRestore({ kind: 'confirm', path: source, report });
    } catch (e) {
      setFlash({ kind: 'err', text: `无法读取备份：${String(e)}` });
      setRestore({ kind: 'idle' });
    } finally {
      setBusy(null);
    }
  };

  const cancelRestore = () => setRestore({ kind: 'idle' });

  const doClear = async () => {
    setConfirmClear(false);
    setClearing(true);
    setFlash(null);
    try {
      const removed = await knowledgeService.clear();
      await refresh();
      setFlash({
        kind: 'ok',
        text: `已清空知识库：删除 ${removed} 条知识，以及全部标签、主题与搜索索引。正在重新加载应用…`,
      });
      // Same reset strategy as Restore: a full reload re-fetches every page
      // from the now-empty SQLite and drops stale Import / Chat state.
      setTimeout(() => window.location.reload(), 1200);
    } catch (e) {
      setFlash({ kind: 'err', text: `清空失败：${String(e)}` });
    } finally {
      setClearing(false);
    }
  };

  const confirmRestore = async () => {
    if (restore.kind !== 'confirm') return;
    const path = restore.path;
    setRestore({ kind: 'restoring', path });
    setFlash(null);
    try {
      const r = await backupService.restore(path);
      setFlash({
        kind: 'ok',
        text: `恢复完成：已替换为 ${r.knowledgeCount} 条知识、${r.domainCount} 个领域。正在重新加载应用…`,
      });
      // MVP: simplest reliable reset is a full window reload. This drops any
      // in-memory Chat history, Import session, and stale Settings state, and
      // re-fetches everything from the now-restored SQLite on the next boot.
      setTimeout(() => window.location.reload(), 1200);
    } catch (e) {
      setFlash({ kind: 'err', text: `恢复失败：${String(e)}` });
      setRestore({ kind: 'idle' });
    }
  };

  return (
    <div className="settings-page page-pad">
      <header className="page-header">
        <div>
          <p>设置</p>
          <h1>配置你的 Interview Kit</h1>
          <span>配置用于真实图片提取的模型服务；API Key 仅保存在本地受限文件中，应用内不会回显。</span>
        </div>
        <Button variant="primary" onClick={save}>
          {state === 'saved' ? (
            <>
              <Check /> 已保存
            </>
          ) : (
            <>保存设置</>
          )}
        </Button>
      </header>

      <div className="settings-content">
        <section>
          <div className="settings-heading">
            <h2>模型</h2>
            <p>配置用于图片提取的 OpenAI-compatible 模型服务（Base URL / Key / Vision Model）。</p>
          </div>
          <div className="form-grid">
            <label>
              API Base URL
              <Input value={s.apiBaseUrl} onChange={(e) => set('apiBaseUrl', e.target.value)} />
            </label>
            <label>
              API Key
              <div className="password">
                <Input
                  type={show ? 'text' : 'password'}
                  value={s.apiKey}
                  placeholder="sk-••••••••••••"
                  onChange={(e) => set('apiKey', e.target.value)}
                />
                <button onClick={() => setShow(!show)}>{show ? <EyeOff /> : <Eye />}</button>
              </div>
            </label>
            <label>
              Chat Model
              <Input value={s.chatModel} onChange={(e) => set('chatModel', e.target.value)} />
            </label>
            <label>
              Vision Model
              <Input value={s.visionModel} onChange={(e) => set('visionModel', e.target.value)} />
            </label>
            <div />
            <div className="test-cell">
              <Button onClick={test}>
                {state === 'testing' ? (
                  <>
                    <LoaderCircle className="spin" /> 测试中
                  </>
                ) : state === 'ok' ? (
                  <>
                    <Check /> 连接正常
                  </>
                ) : state === 'fail' ? (
                  <>测试失败</>
                ) : (
                  'Test Connection'
                )}
              </Button>
              {msg && <p className={state === 'fail' ? 'conn-msg err' : 'conn-msg ok'}>{msg}</p>}
            </div>
          </div>
        </section>

        <section>
          <div className="settings-heading">
            <h2>数据</h2>
            <p>管理本地数据库位置与知识库的备份恢复。</p>
          </div>
          <div className="data-location">
            <label>
              Database Location
              <Input value={s.databaseLocation} readOnly title="数据库位置由应用运行时决定" />
            </label>
            <Button disabled>
              <FolderOpen /> 选择位置
            </Button>
          </div>
          <div className="data-actions">
            <button onClick={startBackup} disabled={busy !== null}>
              <Database />
              <span>
                <b>备份知识库</b>
                <small>仅包含你确认后的知识数据；不含任何应用配置</small>
              </span>
              {busy === 'backup' ? <LoaderCircle className="spin" /> : <Download />}
            </button>
            <button onClick={startRestore} disabled={busy !== null}>
              <RotateCcw />
              <span>
                <b>恢复知识库</b>
                <small>从 Interview Kit 备份恢复知识数据；会替换当前知识库</small>
              </span>
              {busy === 'restore' ? <LoaderCircle className="spin" /> : <FolderOpen />}
            </button>
            <button
              className="danger"
              onClick={() => setConfirmClear(true)}
              disabled={busy !== null || clearing || knowledge.length === 0}
            >
              <Trash2 />
              <span>
                <b>清空知识库</b>
                <small>删除全部知识、标签与主题；此操作不可撤销，且不会自动备份</small>
              </span>
              {clearing ? <LoaderCircle className="spin" /> : <AlertCircle />}
            </button>
          </div>
          {flash && <p className={`conn-msg ${flash.kind === 'err' ? 'err' : 'ok'}`} style={{ marginTop: 12 }}>{flash.text}</p>}
        </section>

        <aside>
          <h3>关于本阶段</h3>
          <p>在桌面应用内运行时会启用真实 SQLite 与真实 Vision 图片提取。浏览器模拟（npm run dev 无 Tauri）仍使用 Mock。</p>
          <small>
            Interview Kit · Phase 2 P2
            <br />
            知识库备份与恢复
          </small>
        </aside>
      </div>

      {restore.kind === 'confirm' && (
        <RestoreConfirm
          report={restore.report}
          onCancel={cancelRestore}
          onConfirm={confirmRestore}
        />
      )}

      {restore.kind === 'restoring' && (
        <RestoreProgress />
      )}

      {confirmClear && (
        <ClearConfirm
          count={knowledge.length}
          onCancel={() => setConfirmClear(false)}
          onConfirm={doClear}
        />
      )}
    </div>
  );
}

function ClearConfirm({
  count,
  onCancel,
  onConfirm,
}: {
  count: number;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="restore-overlay" role="dialog" aria-modal="true">
      <div className="restore-card">
        <h3>清空知识库</h3>
        <p className="restore-lead">
          将永久删除当前知识库中的 <b>{count}</b> 条知识，连同全部标签、主题与搜索索引。
          <br />
          此操作<b>不可撤销</b>，且不会自动创建备份。如果需要保留，请先使用「备份知识库」。
        </p>
        <div className="restore-actions">
          <Button onClick={onCancel}>取消</Button>
          <Button variant="danger" onClick={onConfirm}>
            <Trash2 /> 确认清空
          </Button>
        </div>
      </div>
    </div>
  );
}

function RestoreConfirm({
  report,
  onCancel,
  onConfirm,
}: {
  report: BackupInspect;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  return (
    <div className="restore-overlay" role="dialog" aria-modal="true">
      <div className="restore-card">
        <h3>恢复知识库</h3>
        <p className="restore-lead">
          恢复会替换当前知识库数据。
          <br />
          当前数据会在恢复过程中创建临时安全快照。如果恢复失败，将自动回滚到恢复前的状态。
          <br />
          备份中不包含任何应用配置；模型与 API Key 等设置保持恢复前的状态。
        </p>
        <dl className="restore-meta">
          <div>
            <dt>应用</dt>
            <dd>{report.app}</dd>
          </div>
          <div>
            <dt>创建时间</dt>
            <dd>{report.createdAt}</dd>
          </div>
          <div>
            <dt>知识条目</dt>
            <dd>{report.knowledgeCount}</dd>
          </div>
          <div>
            <dt>知识领域</dt>
            <dd>{report.domainCount}</dd>
          </div>
          <div>
            <dt>数据库格式</dt>
            <dd>v{report.databaseSchemaVersion}</dd>
          </div>
        </dl>
        <div className="restore-actions">
          <Button onClick={onCancel}>取消</Button>
          <Button variant="primary" onClick={onConfirm}>
            <RotateCcw /> 恢复
          </Button>
        </div>
      </div>
    </div>
  );
}

function RestoreProgress() {
  return (
    <div className="restore-overlay" role="dialog" aria-modal="true">
      <div className="restore-card">
        <h3>正在恢复…</h3>
        <p className="restore-lead">
          正在替换本地知识库并重建索引。
          <br />
          完成后应用会自动重新加载。
        </p>
        <p style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
          <LoaderCircle className="spin" /> 请勿关闭应用
        </p>
      </div>
    </div>
  );
}
