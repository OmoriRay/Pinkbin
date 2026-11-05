import { useEffect, useMemo, useRef, useState } from 'react';
import { ChevronRight, Copy, HardDrive, LoaderCircle, ShieldCheck, Trash2 } from 'lucide-react';
import { cancelDiskCleanupScan, executeDiskCleanup, executeRecommendedCleanup, getDiskCleanupScan, getDiskCleanupExecution, listCleanupDrives, onDiskProgress, startDiskCleanupScan, type CleanupDrive, type DiskEntry, type DiskGroup, type DiskPlan, type DiskProgress, type DiskScanSession, type DiskCleanupExecution, type DiskScanMode } from '../diskCleanerApi';
import type { CacheResult } from '../cacheCleanerApi';
import { formatBytes } from '../format';
import './diskCleaner.css';
import { onSharedScanInvalidated } from '../sharedScanEvents';
import { groupRecommendations } from '../recommendationGroups';
import { restoreRecommendedSelection, saveRecommendedSelection, selectedRecommendationTotals } from '../recommendedSelection';
type Stage = 'initial' | 'scanning' | 'preview' | 'executing' | 'error';
interface Props { active: boolean; onBusy: (busy: boolean, executing: boolean) => void; registerCancel: (cancel: (() => void) | null) => void; onComplete: (result: CacheResult, driveIds: string[]) => void; recheckRequest?: { id: number; driveIds: string[] } | null }
function fileDate(value: string | number | null) { if (value == null) return '日期未知'; const date = new Date(typeof value === 'number' ? value * 1000 : value); return Number.isNaN(date.getTime()) ? '日期未知' : date.toLocaleDateString('zh-CN'); }
export function DiskCleaner({ active, onBusy, registerCancel, onComplete, recheckRequest }: Props) {
  const [drives, setDrives] = useState<CleanupDrive[]>([]);
  const [driveIds, setDriveIds] = useState<Set<string>>(new Set());
  const [driveLoading, setDriveLoading] = useState(true);
  const [restoring, setRestoring] = useState(true);
  const [stage, setStage] = useState<Stage>('initial');
  const [scanMode, setScanMode] = useState<DiskScanMode>('full');
  const [plan, setPlan] = useState<DiskPlan | null>(null);
  const [progress, setProgress] = useState<DiskProgress | null>(null);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [pages, setPages] = useState<Record<string, number>>({});
  const [open, setOpen] = useState<Set<string>>(new Set(['temp']));
  const [error, setError] = useState('');
  const [notice, setNotice] = useState('');
  const [cancelling, setCancelling] = useState(false);
  const [copyNotice, setCopyNotice] = useState('');
  const [executionProgress, setExecutionProgress] = useState<DiskCleanupExecution | null>(null);
  const [selectedSoftware, setSelectedSoftware] = useState<Set<string>>(new Set());
  const handledRecheck = useRef(0);
  const trackingExecution = useRef(''); const settledExecution = useRef(''); const hydrated = useRef(false);
  const onCompleteRef = useRef(onComplete); onCompleteRef.current = onComplete;
  const onBusyRef = useRef(onBusy); onBusyRef.current = onBusy;
  const [executionKind, setExecutionKind] = useState<'manual' | 'recommended'>('manual');
  const alive = useRef(true); const operation = useRef(false); const scanId = useRef(''); const cancelRequested = useRef(false);
  const generation = useRef(0); const ignoredSession = useRef(''); const acceptedPlan = useRef(''); const settledScan = useRef('');
  const shouldPoll = useRef(true); const starting = useRef(false); const startFailure = useRef('');
  const refreshSession = useRef<() => Promise<void>>(async () => {});
  const container = useRef<HTMLDivElement>(null);
  const entries = useMemo(() => plan?.groups.flatMap(group => group.entries) ?? [], [plan]);
  const chosen = useMemo(() => entries.filter(entry => selected.has(entry.id)), [entries, selected]);
  const bytes = chosen.reduce((sum, entry) => sum + entry.size, 0);
  const recommended = plan?.recommended;
  const recommendationCategories = useMemo(() => groupRecommendations(recommended?.groups ?? []), [recommended?.groups]);
  const selectedTotals = useMemo(() => selectedRecommendationTotals(recommended?.groups ?? [], selectedSoftware), [recommended?.groups, selectedSoftware]);
  const quickCount = selectedTotals.files;
  const quickBytes = selectedTotals.bytes;
  const quickExecuting = stage === 'executing' && executionKind === 'recommended';
  const displayCount = quickExecuting ? (executionProgress?.total_files ?? quickCount) : chosen.length || quickCount;
  const quickResult = plan?.scan_mode === 'quick';
  const displayBytes = quickExecuting ? (executionProgress?.deleted_bytes ?? 0) : quickResult || !chosen.length ? quickBytes : bytes;
  const busy = stage === 'scanning' || stage === 'executing';
  const applyExecution = (job: DiskCleanupExecution) => {
    if (settledExecution.current === job.plan_id) return;
    setExecutionProgress(job); shouldPoll.current = false;
    if (job.status === 'running') {
      trackingExecution.current = job.plan_id; operation.current = true;
      setExecutionKind('recommended'); setStage('executing'); setError(''); setNotice('');
      return;
    }
    settledExecution.current = job.plan_id; trackingExecution.current = ''; operation.current = false; onBusyRef.current(false, false);
    setPlan(null); setSelected(new Set());
    if (job.status === 'completed' && job.result) {
      setStage('initial'); onCompleteRef.current(job.result, job.drive_ids ?? [...driveIds]);
    } else { setStage('error'); setError(job.error || '便捷清理未能完成，请查看清理记录。'); }
  };
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
    setDriveIds(new Set(session.drive_ids)); setProgress(session.progress); setScanMode(session.scan_mode ?? session.preview?.scan_mode ?? 'full');
    startFailure.current = ''; setNotice('');
    if (session.status === 'running') { shouldPoll.current = true; operation.current = true; setStage('scanning'); return; }
    shouldPoll.current = false;
    settledScan.current = session.scan_id; operation.current = false; setCancelling(false);
    if (session.status === 'completed' && session.preview) {
      if (acceptedPlan.current !== session.preview.plan_id) { acceptedPlan.current = session.preview.plan_id; setPlan(session.preview); setSelectedSoftware(restoreRecommendedSelection(session.preview.plan_id, session.preview.recommended?.groups ?? [])); setSelected(new Set()); setStage('preview'); setError(''); setNotice(''); }
    } else if (session.status === 'cancelled') { setPlan(null); setStage('initial'); setNotice('检查已取消，没有更改任何文件。'); }
    else { setPlan(null); setError(session.error || '检查未能完成，请重试。'); setStage('error'); }
  };
  useEffect(() => {
    alive.current = true;
    shouldPoll.current = true;
    const epoch = ++generation.current;
    void listCleanupDrives().then(next => { if (alive.current && epoch === generation.current) { setDrives(next); if (!scanId.current) setDriveIds(new Set(next.map(drive => drive.id))); } }).catch(err => { if(alive.current) setError(String(err)); }).finally(() => { if(alive.current) setDriveLoading(false); });
    let polling = false; let unlisten: (() => void) | undefined; let stopShared: (() => void) | undefined;
    const refresh = async () => {
      if (polling || !alive.current || starting.current || (!shouldPoll.current && !trackingExecution.current && hydrated.current)) return;
      polling = true; const requestEpoch = generation.current;
      try {
        const execution = await getDiskCleanupExecution();
        if (!alive.current || requestEpoch !== generation.current) return;
        hydrated.current = true;
        if (execution && (execution.status === 'running' || trackingExecution.current === execution.plan_id || !settledExecution.current)) { applyExecution(execution); setRestoring(false); return; }
        if (!shouldPoll.current) return;
        const session = await getDiskCleanupScan();
        if (alive.current && requestEpoch === generation.current) { applySession(session); setRestoring(false); }
      }
      catch { if (alive.current && requestEpoch === generation.current) setNotice('暂时无法更新检查进度，正在自动重试…'); }
      finally { polling = false; }
    };
    refreshSession.current = refresh;
    void onSharedScanInvalidated(() => {
      if (!alive.current || trackingExecution.current || starting.current || ignoredSession.current === scanId.current) return;
      shouldPoll.current = true;
      void refresh();
    }).then(stop => { if (alive.current) stopShared = stop; else stop(); });
    void onDiskProgress(next => { if(alive.current && scanId.current === next.scan_id) setProgress(next); }).then(stop => { if(alive.current && epoch === generation.current) unlisten = stop; else stop(); }).catch(() => { /* 会话轮询仍会更新进度。 */ });
    void refresh(); const timer = window.setInterval(() => void refresh(), 1000);
    return () => { alive.current = false; generation.current++; window.clearInterval(timer); unlisten?.(); stopShared?.(); refreshSession.current = async () => {}; };
  }, []);
  useEffect(() => { if(active) { if(stage !== 'initial') container.current?.closest<HTMLElement>('.cc-dialog')?.focus(); container.current?.querySelector('.dc-body')?.scrollTo({top:0}); } }, [stage, active]);
  useEffect(() => { onBusy(busy, stage === 'executing'); }, [busy, stage, onBusy]);
  useEffect(() => { registerCancel(() => { cancelRequested.current = true; if(scanId.current) void cancelDiskCleanupScan(scanId.current); }); return () => registerCancel(null); }, [registerCancel]);
  const scan = async (force = false, mode: DiskScanMode = 'full', requestedDrives = [...driveIds]) => {
    if(operation.current || restoring || !requestedDrives.length) return;
    setDriveIds(new Set(requestedDrives));
    const epoch = ++generation.current; ignoredSession.current = ''; acceptedPlan.current = ''; settledScan.current = '';
    trackingExecution.current = ''; setExecutionProgress(null);
    shouldPoll.current = true; starting.current = true; startFailure.current = '';
    operation.current = true; cancelRequested.current = false; const id = crypto.randomUUID(); scanId.current = id;
    setScanMode(mode); setStage('scanning'); onBusy(true, false); setProgress(null); setError(''); setNotice(''); setPlan(null); setSelected(new Set()); setPages({}); setCancelling(false);
    try {
      const next = await startDiskCleanupScan(requestedDrives, id, force, mode);
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
  useEffect(() => {
    if (!recheckRequest || handledRecheck.current === recheckRequest.id || driveLoading || restoring || operation.current) return;
    handledRecheck.current = recheckRequest.id;
    void scan(false, 'quick', recheckRequest.driveIds);
    // The explicit request carries the exact original drive selection.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [recheckRequest, driveLoading, restoring]);
  const toggleSoftware = (id: string) => {
    const next = new Set(selectedSoftware);
    if (next.has(id)) next.delete(id); else next.add(id);
    setSelectedSoftware(next);
    if (plan && !saveRecommendedSelection(plan.plan_id, next)) setNotice('本次选择暂时无法保存，刷新后请重新确认。');
  };
  const cancel = async () => { if(!scanId.current || cancelling) return; cancelRequested.current = true; setCancelling(true); try { await cancelDiskCleanupScan(scanId.current); } catch(err) { if(alive.current) { setError(String(err)); setCancelling(false); } } };
  const execute = async (kind: 'manual' | 'recommended' = 'manual') => {
    if(operation.current || stage !== 'preview' || !plan) return;
    if(kind === 'recommended' ? !quickCount : !chosen.length || chosen.length > 10000) return;
    setExecutionKind(kind);
    operation.current = true; setStage('executing'); onBusy(true, true);
    generation.current++; ignoredSession.current = scanId.current; shouldPoll.current = false;
    if (kind === 'recommended') {
      trackingExecution.current = plan.plan_id; settledExecution.current = '';
      setExecutionProgress(null);
      try { const job = await executeRecommendedCleanup(plan.plan_id, [...selectedSoftware]); if (alive.current) applyExecution(job); }
      catch (err) {
        // A lost start response can still leave a live backend job; reconnect before failing.
        try {
          const job = await getDiskCleanupExecution();
          if (alive.current && job?.plan_id === plan.plan_id) { applyExecution(job); return; }
        } catch { /* 下一次打开面板仍可重新连接后台任务。 */ }
        trackingExecution.current = ''; operation.current = false;
        if (alive.current) { setError(String(err)); setPlan(null); setSelected(new Set()); setStage('error'); onBusyRef.current(false, false); }
      }
      return;
    }
    try { const result = await executeDiskCleanup(plan.plan_id, chosen.map(entry => entry.id)); if (alive.current) { setPlan(null); setSelected(new Set()); setStage('initial'); onBusyRef.current(false, false); onCompleteRef.current(result, [...driveIds]); } }
    catch (err) { if (alive.current) { setError(String(err)); setPlan(null); setSelected(new Set()); setStage('error'); } }
    finally { operation.current = false; if (alive.current) onBusyRef.current(false, false); }
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
      {isOpen && <div className="dc-group-content">{group.description && <p className="dc-consequence">{group.description}</p>}
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
        <div className="dc-intro"><span className="dc-drive-stamp"><HardDrive size={28}/></span><p>快速复查仅检查已知清理位置；新安装或移动软件后请用全盘检查。</p></div>
        <div className="dc-drive-heading"><strong>检查哪些磁盘？</strong><span>已选 {driveIds.size} 个</span></div>
        <div className="dc-drives">{drives.map(drive => <label className={`dc-drive${driveIds.has(drive.id) ? ' dc-drive-selected' : ''}`} key={drive.id}><input type="checkbox" aria-label={drive.label} checked={driveIds.has(drive.id)} disabled={busy || restoring} onChange={() => setDriveIds(current => {const next = new Set(current); if(next.has(drive.id)) next.delete(drive.id); else next.add(drive.id); return next;})}/><HardDrive size={22}/><span><strong>{drive.label}</strong><small>可用 {formatBytes(drive.available_bytes)} / 共 {formatBytes(drive.total_bytes)}</small></span></label>)}</div>
        {restoring && <p role="status" className="cc-hint">正在连接检查状态…</p>}{driveLoading && <p role="status" className="cc-hint">正在读取本地磁盘…</p>}{!driveLoading && !drives.length && <p className="cc-check-notice">暂未找到可检查的本地磁盘，请关闭面板后重试。</p>}
        {stage === 'scanning' ? <div className="dc-progress" role="status"><LoaderCircle className="cc-spin" size={22}/><div><strong>{cancelling ? '正在停止检查…' : progress?.phase === 'recommendations' ? '正在整理便捷清理项目…' : `${scanMode === 'quick' ? '正在复查已知位置' : '正在检查'} ${progress?.drive_id || [...driveIds][0]}`}</strong><p>已查看 {(progress?.scanned_files ?? 0).toLocaleString()} 个文件 · {formatBytes(progress?.scanned_bytes ?? 0)}</p></div></div> : null}
        {notice && <p className="cc-check-notice" role="status">{notice}</p>}
      </>}
      {quickExecuting && <div className="dc-progress" role="status"><LoaderCircle className="cc-spin" size={22}/><div><strong>正在便捷清理 · 第 {executionProgress?.batch ?? 1} 批</strong><p>已处理 {(executionProgress?.processed_files ?? 0).toLocaleString()} / {displayCount.toLocaleString()} 个文件 · 已删除 {formatBytes(displayBytes)}</p><progress className="dc-cleanup-meter" aria-label="便捷清理进度" max={Math.max(1, displayCount)} value={executionProgress?.processed_files ?? 0}/><small>会自动继续下一批，请保持程序打开。</small></div></div>}
      {plan && <>
        {quickResult && <p className="cc-hint">快速复查仅覆盖已知位置；新安装或移动软件后可做全盘检查。</p>}
        {chosen.length > 10000 && <p className="cc-check-notice" role="alert">一次最多清理 10,000 个文件，请减少选择后分次处理。</p>}
        <section className="dc-quick" aria-label="便捷清理推荐项">
          <div className="dc-quick-heading"><span className="dc-quick-icon"><ShieldCheck size={25} aria-hidden="true"/></span><div><h3>便捷清理</h3></div><strong className="dc-quick-bytes">{formatBytes(quickBytes)}</strong></div>
          {(recommended?.file_count ?? 0) > 0 ? <><p className="dc-quick-description">已选 {quickCount.toLocaleString()} 个文件。缓存可能需重新生成或下载；旧故障记录不保留。</p><div className="dc-quick-categories">{recommendationCategories.map(category => <details className="dc-quick-category" key={category.id} data-category={category.id}>
            <summary><ChevronRight size={16} className="dc-category-chevron" aria-hidden="true"/><span className="dc-category-label"><strong>{category.title}</strong><small>{selectedRecommendationTotals(category.groups, selectedSoftware).files.toLocaleString()} 个已选</small></span><b>{formatBytes(selectedRecommendationTotals(category.groups, selectedSoftware).bytes)}</b></summary>
            <div className="dc-quick-groups">{category.groups.map(group => <label className={`dc-software-choice${selectedSoftware.has(group.id) ? ' dc-software-selected' : ''}`} key={`${group.id}:${group.title}`}><input type="checkbox" checked={selectedSoftware.has(group.id)} disabled={stage !== 'preview'} onChange={() => toggleSoftware(group.id)} aria-label={`选择${group.title}`}/><span>{group.title}</span><b>{formatBytes(group.bytes)}</b></label>)}</div>
          </details>)}</div></>
            : <p className="dc-quick-description">{recommended?.warning || (!recommended ? '重新检查后，即可准备便捷清理项目。' : '所选磁盘中暂未发现适合便捷清理的项目。')}</p>}
          {!!recommended?.blocked_apps.length && <p className="dc-quick-hint">可先关闭 {recommended.blocked_apps.join('、')} 后重新检查，看看还有哪些项目可以清理。</p>}
          {recommended?.limited && <p className="dc-quick-hint">本次只列出已核验的一批项目，可在清理后重新检查。</p>}
          <div className="dc-quick-actions"><small>永久删除推荐项目，不含手动勾选；不进回收站。</small><button className="cc-button cc-primary" disabled={stage !== 'preview' || !quickCount} onClick={() => void execute('recommended')}><ShieldCheck size={15}/>便捷清理</button></div>
        </section>
        {!quickResult && !!plan.overview?.length && <details className="dc-space-overview"><summary>空间都用在哪里了？</summary><div className="dc-space-list">{plan.overview.map(item => <div key={item.path}><span title={item.path}>{item.title}</span><b>{formatBytes(item.bytes)}</b></div>)}</div>{!!plan.unclassified_caches?.length && <><h4>还有一些缓存目录需要核实</h4><p>这些目录未列入推荐清理。</p><div className="dc-space-list">{plan.unclassified_caches.map(item => <div key={item.path}><span title={item.path}>{item.path}</span><b>{formatBytes(item.bytes)}</b></div>)}</div></>}{plan.overview_limited && <small>目录较多，当前展示部分占用。</small>}</details>}
        {!quickResult && <div className="dc-summary dc-manual-summary"><div><span>人工检查文件</span><strong>{formatBytes(plan.groups.reduce((sum,group) => sum + group.bytes,0))}</strong></div><p>大文件或旧文件不一定是垃圾，请确认不再需要后勾选。直接删除后无法从回收站恢复。</p></div>}
        <div className="dc-rescan">{(quickResult || plan.shared_reused) && <span className="cc-hint">{quickResult ? '快速复查' : '已使用近期扫描结果'}</span>}<button className="cc-text" disabled={stage !== 'preview'} onClick={() => void scan(!quickResult, quickResult ? 'quick' : 'full')}>重新检查</button></div>
        <p className="cc-hint">{quickResult ? '已复查' : '已查看'} {plan.stats.scanned_files.toLocaleString()} 个文件 · {formatBytes(plan.stats.scanned_bytes)}。{plan.stats.limited ? '文件较多，当前仅展示部分候选；清理后可再次检查。' : ''}{plan.stats.denied_directories > 0 ? '部分位置无法访问，已跳过。' : ''}</p>
        {stage === 'executing' && !quickExecuting && <div className="cc-confirm" role="status"><Trash2 size={22}/><div><strong>正在直接删除，请保持窗口打开…</strong><p>共 {formatBytes(displayBytes)}。删除后无法从回收站恢复。</p></div></div>}
        {(quickExecuting || quickResult ? [] : stage === 'preview' ? plan.groups : plan.groups.map(group => ({...group, entries: group.entries.filter(entry => selected.has(entry.id)), file_count: group.entries.filter(entry => selected.has(entry.id)).length, bytes: group.entries.filter(entry => selected.has(entry.id)).reduce((sum,entry) => sum+entry.size,0)})).filter(group => group.entries.length)).map(renderGroup)}
        <details className="cc-diagnostics"><summary>查看检查说明</summary><p>跳过无法访问的位置 {plan.stats.denied_directories} 处，保护的位置 {plan.stats.protected_directories} 处。{plan.stats.limited ? '当前候选展示已达到上限。' : ''}</p>{plan.warnings.map((warning,index) => <p key={index}>{warning}</p>)}</details>
      </>}
      {error && <div className="cc-check-notice" role="alert"><p>本次操作未能完成，请重新检查后再试。</p><details className="cc-diagnostics"><summary>查看原因</summary><p>{error}</p></details></div>}
    </div>
    <div className="cc-footer">{(plan || quickExecuting) && <div className="cc-selection"><span>{quickExecuting ? `已处理 ${(executionProgress?.processed_files ?? 0).toLocaleString()} / ${displayCount.toLocaleString()} 个文件` : quickResult || !chosen.length ? `便捷清理已选 ${quickCount.toLocaleString()} 个文件` : `人工已选 ${chosen.length} 个文件`}</span><strong>{formatBytes(displayBytes)}</strong></div>}<div className="cc-actions">
      {stage === 'scanning' ? <button className="cc-button" disabled={cancelling} onClick={() => void cancel()}>{cancelling ? '正在取消…' : '取消检查'}</button>
        : stage === 'executing' ? <button className="cc-button cc-primary" disabled><LoaderCircle className="cc-spin" size={15}/>删除中…</button>
        : stage === 'preview' ? <>{!quickResult && selected.size > 0 && <button className="cc-text" onClick={() => setSelected(new Set())}>清空人工选择</button>}<button className="cc-button" onClick={changeDrives}><HardDrive size={14}/>更换磁盘</button>{quickResult ? <button className="cc-button" onClick={() => void scan()}>全盘检查</button> : <button className="cc-button" disabled={!chosen.length || chosen.length > 10000} onClick={() => void execute()} title="直接删除手动勾选的文件，不进入回收站"><Trash2 size={15}/>直接删除</button>}<button className="cc-button cc-primary dc-quick-footer" disabled={!quickCount} onClick={() => void execute('recommended')} title="仅删除推荐项目，不包含手动勾选项"><ShieldCheck size={15}/>便捷清理{quickCount > 0 && <span>{formatBytes(quickBytes)}</span>}</button></>
        : <><button className="cc-button" disabled={driveLoading || restoring || !driveIds.size} onClick={() => void scan()}><HardDrive size={15}/>开始全盘检查</button><button className="cc-button cc-primary" disabled={driveLoading || restoring || !driveIds.size} onClick={() => void scan(false, 'quick')}><ShieldCheck size={15}/>快速复查</button></>}
    </div></div>
  </div>;
}
