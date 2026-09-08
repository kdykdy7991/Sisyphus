import { useEffect, useState } from 'react';
import { useApp } from '../AppContext';
import { backupService, knowledgeService, logsService, settingsService, webdavService } from '../services';
import type { BackupInspect, LlmProfiles, Settings, WebDavConfig } from '../types';
import { AlertCircle, Check, Database, Download, Eye, EyeOff, FileText, FolderOpen, LoaderCircle, RotateCcw, Settings as SettingsIcon, Trash2, X } from '../components/Icons';
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
  const [profiles, setProfiles] = useState<LlmProfiles>();
  const [newProfileName, setNewProfileName] = useState('');
  const [profileName, setProfileName] = useState('');
  const [profileManagerOpen, setProfileManagerOpen] = useState(false);

  const [busy, setBusy] = useState<'backup' | 'restore' | null>(null);
  const [restore, setRestore] = useState<RestoreDialog>({ kind: 'idle' });

  // Clear-the-base (danger zone): irreversible, so it gets its own confirm.
  const { knowledge, refresh } = useApp();
  const [confirmClear, setConfirmClear] = useState(false);
  const [clearing, setClearing] = useState(false);

  // WebDAV sync transport (device config; password is never echoed back).
  const [wd, setWd] = useState<WebDavConfig>({ url: '', username: '', password: '' });
  const [wdShow, setWdShow] = useState(false);
  const [wdBusy, setWdBusy] = useState<'save' | 'test' | 'sync' | null>(null);
  const [wdFlash, setWdFlash] = useState<Flash>(null);

  // Diagnostic log directory (set once on mount under Tauri; mock returns a
  // placeholder string). Surfaced as text under the "open log folder"
  // button so users can copy the path if the OS file manager fails to pop.
  const [logDirPath, setLogDirPath] = useState('');
  useEffect(() => {
    logsService.getDir().then(setLogDirPath).catch(() => setLogDirPath(''));
  }, []);

  useEffect(() => {
    settingsService.get().then(setS);
    settingsService.profiles().then(value => {
      setProfiles(value);
      setProfileName(value.profiles.find(profile => profile.id === value.activeId)?.name ?? '');
    });
    webdavService.getConfig().then(setWd);
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
  const switchProfile = async (id: string) => {
    setS(await settingsService.switchProfile(id));
    const next = await settingsService.profiles();
    setProfiles(next);
    setProfileName(next.profiles.find(profile => profile.id === next.activeId)?.name ?? '');
    setState('');
    setMsg('');
  };
  const createProfile = async () => {
    if (!newProfileName.trim()) return;
    await settingsService.createProfile(newProfileName.trim());
    setNewProfileName('');
    setProfiles(await settingsService.profiles());
  };
  const deleteProfile = async (id: string, name: string) => {
    if (!profiles || profiles.profiles.length <= 1 || id === profiles.activeId) return;
    if (window.confirm(`删除配置“${name}”？`)) {
      await settingsService.deleteProfile(id);
      setProfiles(await settingsService.profiles());
    }
  };
  const saveProfile = async () => {
    if (!profiles || !profileName.trim()) return;
    if (s.visionMaxTokens > s.visionContextTokens) {
      setState('fail');
      setMsg('Vision 最大输出 Token 不能超过模型上下文大小。');
      return;
    }
    try {
      await settingsService.save(s);
      await settingsService.renameProfile(profiles.activeId, profileName.trim());
      setProfiles(await settingsService.profiles());
      setState('saved');
      setTimeout(() => setState(''), 1500);
      setProfileManagerOpen(false);
    } catch (error) {
      setState('fail');
      setMsg(String(error));
    }
  };

  // Best-effort: ask the OS to open the log directory. If it fails (rare,
  // usually a sandboxed env) we still have the path string rendered below
  // the button so the user can copy it manually.
  const openLogsDir = async () => {
    setFlash(null);
    try {
      await logsService.openDir();
    } catch (e) {
      setFlash({ kind: 'err', text: `无法打开日志目录：${String(e)}` });
    }
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
        text: `备份完成：${summary.knowledgeCount} 条知识，已保存到 ${summary.path}`,
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
        text: `恢复完成：已替换为 ${r.knowledgeCount} 条知识。正在重新加载应用…`,
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

  const setWdField = (k: keyof WebDavConfig, v: string) => setWd({ ...wd, [k]: v });

  const saveWd = async () => {
    setWdBusy('save');
    setWdFlash(null);
    try {
      await webdavService.saveConfig(wd);
      setWdFlash({ kind: 'ok', text: '已保存 WebDAV 配置。' });
    } catch (e) {
      setWdFlash({ kind: 'err', text: `保存失败：${String(e)}` });
    } finally {
      setWdBusy(null);
    }
  };

  // Probe with the *saved* config, so a freshly typed password is persisted
  // first. The backend reads `webdav.json`, not the form.
  const testWd = async () => {
    setWdBusy('test');
    setWdFlash(null);
    try {
      await webdavService.saveConfig(wd);
      const r = await webdavService.testConnection();
      setWdFlash({
        kind: r.success ? 'ok' : 'err',
        text: r.success
          ? `连接成功（${r.latencyMs}ms）：${r.message}`
          : `连接失败（${r.errorKind || '未知错误'}）：${r.message}`,
      });
    } catch (e) {
      setWdFlash({ kind: 'err', text: `测试失败：${String(e)}` });
    } finally {
      setWdBusy(null);
    }
  };

  const syncWd = async () => {
    setWdBusy('sync');
    setWdFlash(null);
    try {
      // Persist first so freshly typed credentials are used by the sync.
      await webdavService.saveConfig(wd);
      const r = await webdavService.sync();
      const parts: string[] = [];
      if (r.remoteExisted) {
        parts.push(`已从远端合并 ${r.downloadedItemCount} 条`);
      } else {
        parts.push(`远端为空，已上传本机 ${r.uploadedItemCount} 条（首次同步）`);
      }
      parts.push(
        `新增 ${r.stats.inserted} · 更新 ${r.stats.updated} · 删除 ${r.stats.deleted} · 跳过 ${r.stats.skipped}`,
      );
      if (r.retryCount > 0) parts.push(`遇到 ${r.retryCount} 次并发冲突已自动解决`);
      setWdFlash({ kind: 'ok', text: `同步完成：${parts.join('，')}。` });
      // Pull the merged knowledge into the live UI so other pages reflect it.
      await refresh();
    } catch (e) {
      setWdFlash({ kind: 'err', text: `同步失败：${String(e)}` });
    } finally {
      setWdBusy(null);
    }
  };

  return (
    <div className="settings-page page-pad">
      <header className="page-header settings-top">
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
          {profiles && <div className="model-switcher"><label>当前配置<select className="input" value={profiles.activeId} onChange={e=>void switchProfile(e.target.value)}>{profiles.profiles.map(p=><option key={p.id} value={p.id}>{p.name}</option>)}</select></label><Button onClick={()=>setProfileManagerOpen(true)}><SettingsIcon /> 管理配置</Button></div>}
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
                <small>从 Sisyphus 备份恢复知识数据；会替换当前知识库</small>
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

        <section>
          <div className="settings-heading">
            <h2>诊断日志</h2>
            <p>模型提取失败、Chat 报错等诊断信息会写入本地日志文件，文件名按天滚动。如果遇到无法解释的失败，可以打开日志目录把日志发给开发者。</p>
          </div>
          <div className="data-actions">
            <button onClick={openLogsDir} disabled={busy !== null}>
              <FileText />
              <span>
                <b>打开日志目录</b>
                <small>在文件管理器中显示今日的 app-YYYY-MM-DD.log</small>
              </span>
              <FolderOpen />
            </button>
          </div>
          {logDirPath && (
            <p className="conn-msg ok" style={{ marginTop: 12, fontFamily: 'monospace', fontSize: 12 }}>
              {logDirPath}
            </p>
          )}
        </section>

        <section>
          <div className="settings-heading">
            <h2>多设备同步</h2>
            <p>通过 WebDAV 在多台设备间同步知识库。WebDAV 只是同步介质——每台设备都拥有完整本地知识库，同步采用「完整快照 + 合并」，互不覆盖。账号密码仅保存在本机，不会进入同步文件或备份。</p>
          </div>
          <div className="form-grid">
            <label style={{ gridColumn: '1 / -1' }}>
              WebDAV 地址
              <Input
                value={wd.url}
                placeholder="https://dav.example.com/remote.php/dav/files/用户名/Sisyphus"
                onChange={(e) => setWdField('url', e.target.value)}
              />
            </label>
            <label>
              用户名
              <Input value={wd.username} onChange={(e) => setWdField('username', e.target.value)} />
            </label>
            <label>
              密码 / 应用令牌
              <div className="password">
                <Input
                  type={wdShow ? 'text' : 'password'}
                  value={wd.password}
                  placeholder="留空表示保持不变"
                  onChange={(e) => setWdField('password', e.target.value)}
                />
                <button onClick={() => setWdShow(!wdShow)}>{wdShow ? <EyeOff /> : <Eye />}</button>
              </div>
            </label>
            <div className="test-cell">
              <div className="wd-actions">
                <Button onClick={saveWd} disabled={wdBusy !== null}>保存配置</Button>
                <Button onClick={testWd} disabled={wdBusy !== null}>
                  {wdBusy === 'test' ? (
                    <>
                      <LoaderCircle className="spin" /> 测试中
                    </>
                  ) : (
                    '测试连接'
                  )}
                </Button>
                <Button variant="primary" onClick={syncWd} disabled={wdBusy !== null}>
                  {wdBusy === 'sync' ? (
                    <>
                      <LoaderCircle className="spin" /> 同步中
                    </>
                  ) : (
                    '立即同步'
                  )}
                </Button>
              </div>
              {wdFlash && (
                <p className={`conn-msg ${wdFlash.kind === 'err' ? 'err' : 'ok'}`} style={{ marginTop: 12 }}>
                  {wdFlash.text}
                </p>
              )}
            </div>
          </div>
        </section>

        <aside>
          <h3>关于本阶段</h3>
          <p>在桌面应用内运行时会启用真实 SQLite 与真实 Vision 图片提取。浏览器模拟（npm run dev 无 Tauri）仍使用 Mock。</p>
          <small>
            Sisyphus · Phase 2 P2
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

      {profileManagerOpen && profiles && (
        <div className="restore-overlay" role="dialog" aria-modal="true" aria-labelledby="profile-manager-title" onClick={() => setProfileManagerOpen(false)}>
          <div className="restore-card profile-manager" onClick={e => e.stopPropagation()}>
            <div className="profile-manager-head">
              <div><h3 id="profile-manager-title">管理模型配置</h3><p>编辑当前配置，或管理已保存的配置。</p></div>
              <button className="icon-button" aria-label="关闭" onClick={() => setProfileManagerOpen(false)}><X /></button>
            </div>
            <div className="profile-list">
              {profiles.profiles.map(profile => <div key={profile.id} className={profile.id === profiles.activeId ? 'active' : ''}>
                <button onClick={() => void switchProfile(profile.id)}><span>{profile.name}</span>{profile.id === profiles.activeId && <small>当前</small>}</button>
                <button aria-label={`删除 ${profile.name}`} title={profile.id === profiles.activeId ? '当前配置不能删除' : `删除 ${profile.name}`} disabled={profile.id === profiles.activeId || profiles.profiles.length <= 1} onClick={() => void deleteProfile(profile.id, profile.name)}><Trash2 /></button>
              </div>)}
            </div>
            <div className="profile-create"><Input value={newProfileName} maxLength={30} placeholder="新配置名称" onChange={e=>setNewProfileName(e.target.value)} onKeyDown={e=>{if(e.key==='Enter')void createProfile();}}/><Button onClick={()=>void createProfile()} disabled={!newProfileName.trim()}>新增配置</Button></div>
            <div className="profile-form">
              <label className="profile-name-field">配置名称<Input value={profileName} maxLength={30} placeholder="例如 OpenAI · GPT-5" onChange={e => setProfileName(e.target.value)} /></label>
              <label>API Base URL<Input value={s.apiBaseUrl} onChange={(e) => set('apiBaseUrl', e.target.value)} /></label>
              <label>API Key<div className="password"><Input type={show ? 'text' : 'password'} value={s.apiKey} placeholder="sk-••••••••••••" onChange={(e) => set('apiKey', e.target.value)} /><button onClick={() => setShow(!show)}>{show ? <EyeOff /> : <Eye />}</button></div></label>
              <label>Chat Model<Input value={s.chatModel} onChange={(e) => set('chatModel', e.target.value)} /></label>
              <label>Vision Model<Input value={s.visionModel} onChange={(e) => set('visionModel', e.target.value)} /></label>
              <label>模型上下文大小<Input type="number" min={1} step={1024} value={s.visionContextTokens} onChange={e => setS({...s, visionContextTokens: Math.max(1, Number(e.target.value) || 1)})} /><small className="field-hint">模型支持的总上下文 Token；128K = 131072。</small></label>
              <label>Vision 最大输出 Token<Input type="number" min={1} step={1024} value={s.visionMaxTokens} onChange={e => setS({...s, visionMaxTokens: Math.max(1, Number(e.target.value) || 1)})} /><small className="field-hint">32K = 32768；具体上限取决于模型服务。</small></label>
              <div className="thinking-control"><label className="thinking-toggle"><input type="checkbox" checked={s.visionThinkingEnabled} onChange={e => setS({...s, visionThinkingEnabled: e.target.checked})}/><span><b>启用模型思考</b><small>关闭时向兼容接口发送 reasoning_effort: none。</small></span></label><label>思考强度<select className="input" disabled={!s.visionThinkingEnabled} value={s.visionReasoningEffort} onChange={e => setS({...s, visionReasoningEffort: e.target.value as Settings['visionReasoningEffort']})}><option value="low">低</option><option value="medium">中</option><option value="high">高</option></select></label></div>
            </div>
            {msg && <p className={state === 'fail' ? 'conn-msg err' : 'conn-msg ok'}>{msg}</p>}
            <div className="restore-actions"><Button onClick={test}>{state === 'testing' ? <><LoaderCircle className="spin" /> 测试中</> : state === 'ok' ? <><Check /> 连接正常</> : state === 'fail' ? '测试失败' : '测试连接'}</Button><Button variant="primary" disabled={!profileName.trim()} onClick={()=>void saveProfile()}>保存配置</Button></div>
          </div>
        </div>
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
