import { useEffect, useMemo, useRef, useState } from 'react';
import { ArrowRight, Copy, HardDrive, LoaderCircle, ShieldCheck, Trash2 } from 'lucide-react';
import { cancelDiskCleanupScan, executeDiskCleanup, getDiskCleanupScan, listCleanupDrives, onDiskProgress, startDiskCleanupScan, type CleanupDrive, type DiskEntry, type DiskGroup, type DiskPlan, type DiskProgress, type DiskScanSession } from '../diskCleanerApi';
import type { CacheResult } from '../cacheCleanerApi';
import { formatBytes } from '../format';
import './diskCleaner.css';
type Stage = 'initial' | 'scanning' | 'preview' | 'confirm' | 'executing' | 'error';
interface Props { active: boolean; onBusy: (busy: boolean, executing: boolean) => void; registerCancel: (cancel: (() => void) | null) => void; onComplete: (result: CacheResult) => void }
function fileDate(value: string | number | null) { if (value == null) return '日期未知'; const date = new Date(typeof value === 'number' ? value * 1000 : value); return Number.isNaN(date.getTime()) ? '日期未知' : date.toLocaleDateString('zh-CN'); }
export function DiskCleaner({ active, onBusy, registerCancel, onComplete }: Props) {
  const [drives, setDrives] = useState<CleanupDrive[]>([]);
  const [driveIds, setDriveIds] = useState<Set<string>>(new Set());
  const [driveLoading, setDriveLoading] = useState(true);
  const [stage, setStage] = useState<Stage>('initial');
  const [plan, setPlan] = useState<DiskPlan | null>(null);
  const [progress, setProgress] = useState<DiskProgress | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [pages, setPages] = useState<Record<string, number>>({});
  const [open, setOpen] = useState<Set<string>>(new Set(['temp']));
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [cancelling, setCancelling] = useState(false);
  const [copyNotice, setCopyNotice] = useState('');
  const alive = useRef(true); const operation = useRef(false); const scanId = useRef(''); const cancelRequested = useRef(false);
  const generation = useRef(0); const ignoredSession = useRef(''); const acceptedPlan = useRef(''); const settledScan = useRef('');
  const shouldPoll = useRef(true); const starting = useRef(false); const startFailure = useRef('');
  const refreshSession = useRef<() => Promise<void>>(async () => {});
  const container = useRef<HTMLDivElement>(null);
  const entries = useMemo(() => plan?.groups.flatMap(group => group.entries) ?? [], [plan]);
  const chosen = useMemo(() => entries.filter(entry => selected.has(entry.id)), [entries, selected]);
  const bytes = chosen.reduce((sum, entry) => sum + entry.size, 0);
  const busy = stage === 'scanning' || stage === 'executing';
  const applySession = (session: DiskScanSession | null) => {
    if (!session) {
      shouldPoll.current = false;
      if (operation.current || startFailure.current) {
        operation.current = false; setCancelling(false); setStage('error');
        setError(startFailure.current || '检查会话已结束，请重新检查。'); startFailure.current = '';
      }
      return;
    }
    if (session.scan_id === ignoredSession.current) { shouldPoll.current = false; return; }
    if (session.status === 'running' && settledScan.current === session.scan_id) return;
    scanId.current = session.scan_id;
    setDriveIds(new Set(session.drive_ids)); setProgress(session.progress);
    startFailure.current = ''; setNotice('');
    if (session.status === 'running') { shouldPoll.current = true; operation.current = true; setStage('scanning'); return; }
    shouldPoll.current = false;
    settledScan.current = session.scan_id; operation.current = false; setCancelling(false);
    if (session.status === 'completed' && session.preview) {
      if (acceptedPlan.current !== session.preview.plan_id) { acceptedPlan.current = session.preview.plan_id; setPlan(session.preview); setSelected(new Set()); setStage('preview'); setError(''); setNotice(''); }
    } else if (session.status === 'cancelled') { setPlan(null); setStage('initial'); setNotice('检查已取消，没有更改任何文件。'); }
    else { setPlan(null); setError(session.error || '检查未能完成，请重试。'); setStage('error'); }
  };
  useEffect(() => {
    alive.current = true;
    shouldPoll.current = true;
    const epoch = ++generation.current;
    void listCleanupDrives().then(next => { if (alive.current && epoch === generation.current) { setDrives(next); if (!scanId.current) setDriveIds(new Set(next.map(drive => drive.id))); } }).catch(err => { if(alive.current) setError(String(err)); }).finally(() => { if(alive.current) setDriveLoading(false); });
    let polling = false; let unlisten: (() => void) | undefined;
    const refresh = async () => {
      if (polling || !alive.current || !shouldPoll.current || starting.current) return;
      polling = true; const requestEpoch = generation.current;
      try { const session = await getDiskCleanupScan(); if (alive.current && requestEpoch === generation.current) applySession(session); }
      catch { if (alive.current && requestEpoch === generation.current) setNotice('暂时无法更新检查进度，正在自动重试…'); }
      finally { polling = false; }
    };
    refreshSession.current = refresh;
    void onDiskProgress(next => { if(alive.current && scanId.current === next.scan_id) setProgress(next); }).then(stop => { if(alive.current && epoch === generation.current) unlisten = stop; else stop(); }).catch(() => { /* 会话轮询仍会更新进度。 */ });
    void refresh(); const timer = window.setInterval(() => void refresh(), 1000);
    return () => { alive.current = false; generation.current++; window.clearInterval(timer); unlisten?.(); refreshSession.current = async () => {}; };
  }, []);
  useEffect(() => { if(active) { if(stage !== 'initial') container.current?.closest<HTMLElement>('.cc-dialog')?.focus(); container.current?.querySelector('.dc-body')?.scrollTo({top:0}); } }, [stage, active]);
  useEffect(() => { onBusy(busy, stage === 'executing'); }, [busy, stage, onBusy]);
  useEffect(() => { registerCancel(() => { cancelRequested.current = true; if(scanId.current) void cancelDiskCleanupScan(scanId.current); }); return () => registerCancel(null); }, [registerCancel]);
  const scan = async () => {
    if(operation.current || !driveIds.size) return;
    const epoch = ++generation.current; ignoredSession.current = ''; acceptedPlan.current = ''; settledScan.current = '';
    shouldPoll.current = true; starting.current = true; startFailure.current = '';
    operation.current = true; cancelRequested.current = false; const id = crypto.randomUUID(); scanId.current = id;
    setStage('scanning'); onBusy(true, false); setProgress(null); setError(''); setNotice(''); setPlan(null); setSelected(new Set()); setPages({}); setCancelling(false);
    try {
      const next = await startDiskCleanupScan([...driveIds], id);
      if(!alive.current || epoch !== generation.current) return;
      starting.current = false;
      applySession(next);
      if(cancelRequested.current) await cancelDiskCleanupScan(next.scan_id);
    } catch(err) {
      if(alive.current && epoch === generation.current) {
        starting.current = false; shouldPoll.current = true; startFailure.current = String(err);
        setNotice('正在重新连接后台检查，请稍候…');
        await refreshSession.current();
      }
    }
  };
  const cancel = async () => { if(!scanId.current || cancelling) return; cancelRequested.current = true; setCancelling(true); try { await cancelDiskCleanupScan(scanId.current); } catch(err) { if(alive.current) { setError(String(err)); setCancelling(false); } } };
  const execute = async () => {
    if(operation.current || stage !== 'confirm' || !plan || !chosen.length || chosen.length > 10000) return;
    operation.current = true; setStage('executing'); onBusy(true, true);
    generation.current++; ignoredSession.current = scanId.current; shouldPoll.current = false;
    try { const result = await executeDiskCleanup(plan.plan_id, chosen.map(entry => entry.id)); if(alive.current) { setPlan(null); setSelected(new Set()); setStage('initial'); onBusy(false, false); onComplete(result); } }
    catch(err) { if(alive.current) { setError(String(err)); setPlan(null); setSelected(new Set()); setStage('error'); } }
    finally { operation.current = false; if(alive.current) onBusy(false, false); }
  };
  const changeDrives = () => {
    generation.current++; ignoredSession.current = scanId.current; shouldPoll.current = false;
    setPlan(null); setSelected(new Set()); setPages({}); setNotice(''); setError(''); setProgress(null); setCancelling(false); setOpen(new Set(['temp'])); setStage('initial');
    container.current?.closest<HTMLElement>('.cc-dialog')?.focus();
  };
  const toggle = (id: string) => setSelected(current => { const next = new Set(current); if(next.has(id)) next.delete(id); else next.add(id); return next; });
  const copyFiles = async (files: DiskEntry[]) => {
    const text = files.map(entry => `名称：${entry.title}\n完整路径：${entry.path}\n大小：${formatBytes(entry.size)}（${entry.size.toLocaleString()} 字节）\n修改日期：${entry.modified == null ? '日期未知' : new Date(typeof entry.modified === 'number' ? entry.modified * 1000 : entry.modified).toLocaleString('zh-CN')}`).join('\n\n');
    try { await navigator.clipboard.writeText(text); if(alive.current) setCopyNotice(`已复制 ${files.length} 个文件的信息。`); }
    catch { if(alive.current) setCopyNotice('复制失败，请检查剪贴板权限后重试。'); }
  };
  const renderGroup = (group: DiskGroup) => {
    const page = pages[group.id] ?? 0; const count = Math.ceil(group.entries.length / 50); const visible = group.entries.slice(page * 50, (page + 1) * 50);
    const isOpen = open.has(group.id);
    const selectedEntries = group.entries.filter(entry => selected.has(entry.id));
    const allSelected = group.entries.length > 0 && selectedEntries.length === group.entries.length;
    return <section className="dc-group" key={group.id}>
      <button className="dc-group-toggle" aria-expanded={isOpen} onClick={() => setOpen(current => {const next = new Set(current); if(next.has(group.id)) next.delete(group.id); else next.add(group.id); return next;})}><span className="dc-chevron">{isOpen ? '−' : '+'}</span><span><strong>{group.title}</strong><small>{group.file_count.toLocaleString()} 个待检查文件 · 已选 {group.entries.filter(entry => selected.has(entry.id)).length} 个</small></span><b>{formatBytes(group.bytes)}</b></button>
      {isOpen && <div className="dc-group-content"><p className="dc-consequence">{group.description || '请确认这些文件不再需要后，再勾选清理。'}</p>
        {group.entries.length === 0 ? <p className="cc-hint">没有找到此类文件。</p> : <>
        {stage === 'preview' && <div className="dc-bulk-actions"><button className="cc-text" aria-pressed={allSelected} title="选择或取消此分类的全部文件，包含其他页" onClick={() => setSelected(current => { const next = new Set(current); const all = group.entries.every(entry => next.has(entry.id)); group.entries.forEach(entry => { if(all) next.delete(entry.id); else next.add(entry.id); }); return next; })}>{allSelected ? '取消全选' : '全选'}</button><button className="cc-text dc-copy-selected" disabled={!selectedEntries.length} title="复制此分类内选中的文件信息，包含其他页" onClick={() => void copyFiles(selectedEntries)}><Copy size={13} aria-hidden="true"/>复制选中的文件信息</button></div>}
        <div className="dc-files">{visible.map(entry => <div className={`dc-file${selected.has(entry.id) ? ' dc-selected' : ''}`} key={entry.id}><label className="dc-file-select"><input type="checkbox" aria-label={`选择 ${entry.title}`} checked={selected.has(entry.id)} disabled={stage !== 'preview'} onChange={() => toggle(entry.id)} /><span className="dc-file-copy"><strong>{entry.title}</strong><span title={entry.path}>{entry.path}</span></span><span className="dc-file-meta"><b>{formatBytes(entry.size)}</b><time>{fileDate(entry.modified)}</time></span></label><button className="dc-copy-button" title="复制名称、完整路径、大小和修改日期" aria-label={`复制 ${entry.title} 的文件信息`} onClick={() => void copyFiles([entry])}><Copy size={15} aria-hidden="true"/></button></div>)}</div>
        {count > 1 && <div className="dc-pagination"><span>每页 50 个 · 第 {page + 1} / {count} 页</span><button className="cc-button" disabled={page === 0} onClick={() => setPages(current => ({...current,[group.id]:page-1}))}>上一页</button><button className="cc-button" disabled={page + 1 >= count} onClick={() => setPages(current => ({...current,[group.id]:page+1}))}>下一页</button></div>}</>}
      </div>}
    </section>;
  };
  return <div className="dc-panel" hidden={!active} ref={container}>
    <div className="cc-body dc-body">
      {copyNotice && <p className="cc-check-notice dc-copy-notice" role="status" aria-live="polite">{copyNotice}</p>}
      {(stage === 'initial' || stage === 'scanning' || stage === 'error') && <>
        <div className="dc-intro"><span className="dc-drive-stamp"><HardDrive size={28}/></span><div><h3>把每个磁盘，都仔细看看</h3><p>从所选磁盘查找临时文件、旧记录、安装包和大文件。<br/>只检查，不自动删除；找到的文件由你逐个确认。</p></div></div>
        <div className="dc-drive-heading"><strong>检查哪些磁盘？</strong><span>已选 {driveIds.size} 个</span></div>
        <div className="dc-drives">{drives.map(drive => <label className={`dc-drive${driveIds.has(drive.id) ? ' dc-drive-selected' : ''}`} key={drive.id}><input type="checkbox" aria-label={drive.label} checked={driveIds.has(drive.id)} disabled={busy} onChange={() => setDriveIds(current => {const next = new Set(current); if(next.has(drive.id)) next.delete(drive.id); else next.add(drive.id); return next;})}/><HardDrive size={22}/><span><strong>{drive.label}</strong><small>可用 {formatBytes(drive.available_bytes)} / 共 {formatBytes(drive.total_bytes)}</small></span></label>)}</div>
        {driveLoading && <p role="status" className="cc-hint">正在读取本地磁盘…</p>}{!driveLoading && !drives.length && <p className="cc-check-notice">暂未找到可检查的本地磁盘，请关闭面板后重试。</p>}
        {stage === 'scanning' ? <div className="dc-progress" role="status"><LoaderCircle className="cc-spin" size={22}/><div><strong>{cancelling ? '正在停止检查…' : `正在检查 ${progress?.drive_id || [...driveIds][0]}`}</strong><p>已查看 {(progress?.scanned_files ?? 0).toLocaleString()} 个文件 · {formatBytes(progress?.scanned_bytes ?? 0)}</p><small>文件较多时需要一些时间，可以随时取消。</small></div></div> : <div className="dc-pledge"><ShieldCheck size={17}/><span>文件留在本机 · 全部由你选择 · 移入回收站可恢复</span></div>}
        {notice && <p className="cc-check-notice" role="status">{notice}</p>}
      </>}
      {plan && <>
        {chosen.length > 10000 && <p className="cc-check-notice" role="alert">一次最多清理 10,000 个文件，请减少选择后分次处理。</p>}
        <div className="dc-summary"><div><span>找到待检查文件</span><strong>{formatBytes(plan.groups.reduce((sum,group) => sum + group.bytes,0))}</strong></div><p>这些文件<b>不一定是垃圾</b>。<br/>所有文件默认不选，请根据名称和位置决定是否保留。</p></div>
        <p className="cc-hint">已查看 {plan.stats.scanned_files.toLocaleString()} 个文件 · {formatBytes(plan.stats.scanned_bytes)}。{plan.stats.limited ? '文件较多，当前仅展示部分候选；清理后可再次检查。' : ''}{plan.stats.denied_directories > 0 ? '部分位置无法访问，已跳过。' : ''}</p>
        {(stage === 'confirm' || stage === 'executing') && <div className="cc-confirm" role="status"><Trash2 size={22}/><div><strong>{stage === 'executing' ? '正在移入回收站，请保持窗口打开…' : `确认将 ${chosen.length} 个文件移入回收站？`}</strong><p>共 {formatBytes(bytes)}。回收站内仍占空间，需要时可恢复。</p><p>其中{chosen.some(entry => entry.category === 'large') ? '包含大文件，可能是个人资料；' : ''}{chosen.some(entry => entry.category === 'installers') ? '包含安装包；' : ''}全部需要你确认不再使用。旧日志和故障记录可能用于排查问题。</p></div></div>}
        {(stage === 'preview' ? plan.groups : plan.groups.map(group => ({...group, entries: group.entries.filter(entry => selected.has(entry.id)), file_count: group.entries.filter(entry => selected.has(entry.id)).length, bytes: group.entries.filter(entry => selected.has(entry.id)).reduce((sum,entry) => sum+entry.size,0)})).filter(group => group.entries.length)).map(renderGroup)}
        <details className="cc-diagnostics"><summary>查看检查说明</summary><p>跳过无法访问的位置 {plan.stats.denied_directories} 处，保护的位置 {plan.stats.protected_directories} 处。{plan.stats.limited ? '当前候选展示已达到上限。' : ''}</p>{plan.warnings.map((warning,index) => <p key={index}>{warning}</p>)}</details>
      </>}
      {error && <div className="cc-check-notice" role="alert"><p>本次操作未能完成，请重新检查后再试。</p><details className="cc-diagnostics"><summary>查看原因</summary><p>{error}</p></details></div>}
    </div>
    <div className="cc-footer"><div className="cc-selection"><span>{plan ? `已选 ${chosen.length} 个文件` : '检查不会删除文件'}</span>{plan && <strong>{formatBytes(bytes)}</strong>}</div><div className="cc-actions">
      {stage === 'scanning' ? <button className="cc-button" disabled={cancelling} onClick={() => void cancel()}>{cancelling ? '正在取消…' : '取消检查'}</button> : stage === 'executing' ? <button className="cc-button cc-primary" disabled><LoaderCircle className="cc-spin" size={15}/>清理中…</button> : stage === 'confirm' ? <><button className="cc-button" onClick={() => {setPages({}); setStage('preview');}}>返回选择</button><button className="cc-button cc-primary" onClick={() => void execute()}><Trash2 size={15}/>确认移入回收站</button></> : stage === 'preview' ? <><button className="cc-text" disabled={!selected.size} onClick={() => setSelected(new Set())}>清空选择</button><button className="cc-button" onClick={changeDrives}><HardDrive size={14}/>更换磁盘</button><button className="cc-button cc-primary" disabled={!chosen.length || chosen.length > 10000} onClick={() => {setPages({}); setOpen(new Set(plan?.groups.map(group => group.id))); setStage('confirm');}}>下一步<ArrowRight size={15}/></button></> : <button className="cc-button cc-primary" disabled={driveLoading || !driveIds.size} onClick={() => void scan()}><HardDrive size={15}/>开始全盘检查</button>}
    </div></div>
  </div>;
}
