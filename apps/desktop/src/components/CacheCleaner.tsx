import { useCallback, useEffect, useRef, useState } from 'react';
import { createPortal } from 'react-dom';
import { Brush, Check, CheckCircle2, ChevronRight, Chrome, FileWarning, FolderClock, HardDrive, LoaderCircle, RotateCcw, ShieldCheck, Trash2, X } from 'lucide-react';
import { executeCacheCleanup, getCacheCleanupScan, listCleanupHistory, startCacheCleanupScan, type CachePlan, type CacheResult, type CacheRule, type CacheScanSession, type CleanupHistory } from '../cacheCleanerApi';
import { formatBytes } from '../format';
import { isTauri } from '../env';
import { getDiskCleanupScan, type DiskScanSession } from '../diskCleanerApi';
import './cacheCleaner.css';
import { DiskCleaner } from './DiskCleaner';

type Stage = 'initial' | 'scanning' | 'preview' | 'executing' | 'complete' | 'error';
const ruleIcons = { temp: FolderClock, chrome: Chrome, edge: HardDrive, brave: Chrome, firefox: Chrome, 'pip-cache': HardDrive, 'crash-dumps': FileWarning };
const isActionable = (message: string) => /请.{0,12}(关闭|退出|重试|重新扫描)|先关闭/.test(message);

function impact(rule: CacheRule) {
  if (rule.id === 'crash-dumps') return '旧故障的诊断资料将被清理，需要排查问题时请保留。';
  if (rule.id === 'pip-cache') return '以后安装 Python 软件包可能需重新下载或构建，离线安装也可能受影响。';
  if (rule.id === 'temp') return '应用留下的旧临时文件，正在使用的文件会跳过。';
  if (/chrome|edge|brave|vivaldi|opera|firefox|browser/.test(rule.id)) return '网页首次打开可能稍慢，之后会重新缓存。';
  return rule.consequence.replace(/^直接删除[；，]?/, '');
}

export function CacheCleaner({ onClose }: { onClose: () => void }) {
  const [stage, setStage] = useState<Stage>('initial');
  const [plan, setPlan] = useState<CachePlan | null>(null);
  const [selected, setSelected] = useState<Set<string>>(() => new Set());
  const [result, setResult] = useState<CacheResult | null>(null);
  const [error, setError] = useState('');
  const [view, setView] = useState<'disk' | 'scan' | 'history'>('disk');
  const [history, setHistory] = useState<CleanupHistory | null>(null);
  const [historyLoading, setHistoryLoading] = useState(false);
  const [historyError, setHistoryError] = useState('');
  const historyRequest = useRef(0);
  const panel = useRef<HTMLDivElement>(null);
  const alive = useRef(true);
  const operation = useRef(false);
  const request = useRef(0);
  const shouldPoll = useRef(true); const starting = useRef(false); const startFailure = useRef('');
  const cacheScanId = useRef(''); const ignoredSession = useRef(''); const acceptedPlan = useRef('');
  const settledScan = useRef(''); const userNavigated = useRef(false);
  const restoringDiskSession = useRef<DiskScanSession | null>(null);
  const refreshSession = useRef<() => Promise<void>>(async () => {});
  const [connectionNotice, setConnectionNotice] = useState('');
  const [diskBusy, setDiskBusy] = useState(false);
  const [diskExecuting, setDiskExecuting] = useState(false);
  const diskCancel = useRef<(() => void) | null>(null);
  const diskExecutingRef = useRef(false);
  const reportDiskBusy = useCallback((nextBusy: boolean, nextExecuting: boolean) => { setDiskBusy(nextBusy); setDiskExecuting(nextExecuting); diskExecutingRef.current = nextExecuting; }, []);
  const registerDiskCancel = useCallback((cancel: (() => void) | null) => { diskCancel.current = cancel; }, []);
  const close = () => { if (diskExecutingRef.current || executingRef.current) return; onClose(); };
  const executing = stage === 'executing' || diskExecuting;
  const busy = stage === 'scanning' || executing || diskBusy;
  const executingRef = useRef(executing);
  executingRef.current = stage === 'executing';
  const closeRef = useRef(onClose);
  closeRef.current = close;
  const selectedRules = plan?.rules.filter((rule) => selected.has(rule.id) && rule.status === 'ready') ?? [];
  const totalBytes = selectedRules.reduce((sum, rule) => sum + rule.bytes, 0);
  const totalFiles = selectedRules.reduce((sum, rule) => sum + rule.file_count, 0);
  const availableRules = plan?.rules.filter((rule) => rule.status === 'ready' && rule.file_count > 0) ?? [];
  const blockedRules = plan?.rules.filter((rule) => rule.status === 'blocked' && (rule.file_count > 0 || rule.bytes > 0)) ?? [];
  const closableRules = blockedRules.filter((rule) => /关闭/.test(rule.message ?? ''));
  const manualRules = blockedRules.filter((rule) => !/关闭/.test(rule.message ?? ''));
  const blockedBytes = closableRules.reduce((sum, rule) => sum + rule.bytes, 0);
  const otherRules = plan?.rules.filter((rule) => rule.file_count === 0 && rule.bytes === 0) ?? [];
  const incomplete = !!plan && (plan.warnings.length > 0 || plan.rules.some((rule) => rule.status === 'blocked' && rule.id !== 'nvidia-shader' && rule.file_count === 0 && rule.bytes === 0));
  const rawActionHints = plan ? [...new Set([...plan.warnings, ...plan.rules.flatMap((rule) => rule.message ? [rule.message] : [])].filter(isActionable))] : [];
  const applicationsToClose = rawActionHints.flatMap((hint) => { const match = hint.match(/请先关闭\s*(.+?)\s*后重新扫描/); return match ? [match[1]] : []; });
  const actionHints = applicationsToClose.length ? [`请先关闭 ${applicationsToClose.join('、')} 后重新扫描。`] : rawActionHints.slice(0, 1);

  useEffect(() => {
    alive.current = true;
    const previous = document.activeElement as HTMLElement | null;
    const root = document.getElementById('root');
    const wasInert = root?.hasAttribute('inert') ?? false;
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    root?.setAttribute('inert', '');
    const focusable = () => Array.from(panel.current?.querySelectorAll<HTMLElement>('button:not(:disabled), input:not(:disabled), summary, [tabindex="0"]') ?? []).filter((el) => el.getClientRects().length > 0);
    panel.current?.focus();
    const keydown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        event.stopPropagation();
        if (!executingRef.current) closeRef.current();
      }
      if (event.key !== 'Tab') return;
      const elements = focusable();
      const first = elements[0];
      const last = elements[elements.length - 1];
      if (!first) { event.preventDefault(); panel.current?.focus(); return; }
      if (event.shiftKey && (document.activeElement === first || document.activeElement === panel.current)) {
        event.preventDefault(); last.focus();
      } else if (!event.shiftKey && (document.activeElement === last || document.activeElement === panel.current)) {
        event.preventDefault(); first.focus();
      }
    };
    const keepFocus = (event: FocusEvent) => {
      if (!panel.current?.contains(event.target as Node)) panel.current?.focus();
    };
    document.addEventListener('keydown', keydown, true);
    document.addEventListener('focusin', keepFocus);
    return () => {
      alive.current = false;
      request.current += 1;
      historyRequest.current += 1;
      document.removeEventListener('keydown', keydown, true);
      document.removeEventListener('focusin', keepFocus);
      if (!wasInert) root?.removeAttribute('inert');
      document.body.style.overflow = previousOverflow;
      previous?.focus();
    };
  }, []);

  const applyCacheSession = (session: CacheScanSession | null) => {
    if (!session) {
      shouldPoll.current = false;
      if (operation.current || startFailure.current) {
        operation.current = false; setError(startFailure.current || '检查会话已结束，请重新扫描。');
        startFailure.current = ''; setConnectionNotice(''); setStage('error');
      }
      return;
    }
    if (session.scan_id === ignoredSession.current) { shouldPoll.current = false; return; }
    if (session.status === 'running' && settledScan.current === session.scan_id) return;
    const diskSession = restoringDiskSession.current;
    let preferCache = session.status === 'running' || (session.status === 'completed' && !!session.preview);
    if (diskSession?.status === 'running') preferCache = false;
    else if (session.status === 'completed' && session.preview && diskSession?.status === 'completed' && diskSession.preview) {
      const cacheTime = Date.parse(session.preview.created_at); const diskTime = Date.parse(diskSession.preview.created_at);
      preferCache = Number.isFinite(cacheTime) && Number.isFinite(diskTime) && cacheTime > diskTime;
    }
    if (cacheScanId.current !== session.scan_id && !userNavigated.current && preferCache) setView('scan');
    cacheScanId.current = session.scan_id; startFailure.current = ''; setConnectionNotice('');
    if (session.status === 'running') { shouldPoll.current = true; operation.current = true; setStage('scanning'); return; }
    shouldPoll.current = false; operation.current = false; settledScan.current = session.scan_id;
    if (session.status === 'completed' && session.preview) {
      const next = session.preview;
      if (acceptedPlan.current !== next.plan_id) {
        acceptedPlan.current = next.plan_id; setPlan(next); setError('');
        setSelected(new Set(next.rules.filter(rule => rule.status === 'ready' && rule.file_count > 0 && rule.id !== 'crash-dumps').map(rule => rule.id)));
        setStage('preview');
      }
    } else { setPlan(null); setSelected(new Set()); setError(session.error || '检查未能完成，请重新扫描。'); setStage('error'); }
  };

  useEffect(() => {
    shouldPoll.current = true;
    let polling = false; let detached = false; let recoverContext = true;
    const refresh = async () => {
      if (detached || !alive.current || polling || starting.current || !shouldPoll.current) return;
      polling = true; const requestId = request.current;
      try {
        // 仅恢复时读取全盘会话，避免旧缓存结果抢走正在进行的全盘检查页签。
        const [cacheResult, diskResult] = await Promise.allSettled([getCacheCleanupScan(), recoverContext ? getDiskCleanupScan() : Promise.resolve(null)]);
        if (detached || !alive.current || requestId !== request.current) return;
        if (recoverContext && diskResult.status === 'fulfilled') { restoringDiskSession.current = diskResult.value; recoverContext = false; }
        if (cacheResult.status === 'rejected') throw cacheResult.reason;
        applyCacheSession(cacheResult.value);
      }
      catch { if (!detached && alive.current && requestId === request.current) setConnectionNotice('暂时无法连接后台检查，正在自动重试…'); }
      finally { polling = false; }
    };
    refreshSession.current = refresh;
    void refresh(); const timer = window.setInterval(() => void refresh(), 1000);
    return () => { detached = true; window.clearInterval(timer); refreshSession.current = async () => {}; };
  }, []);

  useEffect(() => {
    panel.current?.focus();
    if (view === 'history') panel.current?.querySelector('.cc-body')?.scrollTo({ top: 0 });
  }, [stage, view]);

  const showHistory = async () => {
    if (executingRef.current || diskExecutingRef.current) return;
    const requestId = ++historyRequest.current;
    userNavigated.current = true; setView('history'); setHistoryLoading(true); setHistoryError('');
    try {
      const next = await listCleanupHistory();
      if (alive.current && requestId === historyRequest.current) setHistory(next);
    } catch (err) {
      if (alive.current && requestId === historyRequest.current) { setHistory(null); setHistoryError(String(err)); }
    } finally {
      if (alive.current && requestId === historyRequest.current) setHistoryLoading(false);
    }
  };

  const scan = async () => {
    if (operation.current || diskBusy || diskExecutingRef.current) return;
    operation.current = true;
    const requestId = ++request.current;
    starting.current = true; shouldPoll.current = true; startFailure.current = ''; ignoredSession.current = ''; acceptedPlan.current = ''; settledScan.current = '';
    userNavigated.current = true; setConnectionNotice('');
    setView('scan'); setStage('scanning'); setError(''); setPlan(null); setResult(null); setSelected(new Set());
    try {
      const next = await startCacheCleanupScan();
      if (!alive.current || requestId !== request.current) return;
      starting.current = false; applyCacheSession(next);
    } catch (err) {
      if (alive.current && requestId === request.current) {
        starting.current = false; shouldPoll.current = true; startFailure.current = String(err);
        setConnectionNotice('正在重新连接后台检查，请稍候…'); await refreshSession.current();
      }
    }
  };

  const execute = async () => {
    if (operation.current || stage !== 'preview' || !plan || !selectedRules.length) return;
    operation.current = true;
    request.current++; shouldPoll.current = false; ignoredSession.current = cacheScanId.current;
    executingRef.current = true;
    setStage('executing');
    try {
      const next = await executeCacheCleanup(plan.plan_id, selectedRules.map((rule) => rule.id));
      if (alive.current) {
        setResult(next); setPlan(null); setSelected(new Set()); setStage('complete');
        executingRef.current = false;
        void showHistory();
      }
    } catch (err) {
      if (alive.current) { setError(String(err)); setPlan(null); setSelected(new Set()); setStage('error'); }
    } finally { operation.current = false; executingRef.current = false; }
  };

  const toggle = (id: string) => setSelected((current) => {
    const next = new Set(current);
    if (next.has(id)) next.delete(id); else next.add(id);
    return next;
  });

  return createPortal(
    <div className="cc-backdrop" onMouseDown={(event) => { if (event.target === event.currentTarget && !executingRef.current) close(); }}>
      <div className="cc-dialog" ref={panel} role="dialog" aria-modal="true" aria-labelledby="cc-title" aria-describedby="cc-subtitle" tabIndex={-1} aria-busy={busy || diskBusy}>
        <div className="cc-header">
          <span className="cc-mark"><Brush size={23} aria-hidden="true" /></span>
          <div className="cc-heading"><h2 id="cc-title">磁盘清理</h2><p id="cc-subtitle">先检查磁盘，再选择不再需要的文件。</p></div>
          <button className="cc-close" onClick={close} disabled={executing} aria-label="关闭磁盘清理"><X size={19} /></button>
        </div>
        {!isTauri && <div className="cc-demo">DEMO · 浏览器演示数据，所有清理均为模拟，不会操作本机文件。</div>}
        <nav className="cc-navigation" aria-label="磁盘清理视图"><button aria-pressed={view === 'disk'} disabled={executing || stage === 'scanning'} onClick={() => { userNavigated.current = true; setView('disk'); }}>全盘检查</button><button aria-pressed={view === 'scan'} disabled={executing || stage === 'scanning'} onClick={() => { userNavigated.current = true; setView('scan'); if (stage === 'complete') setStage('initial'); }}>常用缓存</button><button aria-pressed={view === 'history'} disabled={executing || stage === 'scanning' || historyLoading} onClick={showHistory}>清理记录</button></nav>
        <DiskCleaner active={view === 'disk'} onBusy={reportDiskBusy} registerCancel={registerDiskCancel} onComplete={(next) => { setResult(next); void showHistory(); }} />
        {view !== 'disk' && <><div className="cc-body">
          {connectionNotice && <p className="cc-check-notice" role="status">{connectionNotice}</p>}
          {view === 'history' ? <>{result && <ResultCard result={result} />}<HistoryView history={history} loading={historyLoading} error={historyError} compact={!!result} /></> : <>
          {(stage === 'initial' || stage === 'scanning') && <div className="cc-intro">
            <div className={`cc-intro-icon${stage === 'scanning' ? ' cc-working' : ''}`}>{stage === 'scanning' ? <LoaderCircle size={34} /> : <FolderClock size={34} />}</div>
            <h3>{stage === 'scanning' ? '正在本机查找可清理文件…' : '先看看，再决定清理什么'}</h3>
            <p>检查浏览器、常用软件和系统留下的缓存。<br />先看看能清理多少，扫描不会删除文件。</p>
            <div className="cc-category-tags"><span>浏览器缓存</span><span>软件缓存</span><span>临时文件</span><span>旧故障记录</span></div>
            <div className="cc-pledge"><ShieldCheck size={16} /> 本机检查 <span>·</span> 自己选择 <span>·</span> 直接删除</div>
          </div>}
          {(stage === 'preview' || stage === 'executing') && plan && <>
            <div className="cc-overview"><div><span>本次已选</span><strong>{formatBytes(totalBytes)}</strong></div><p>{selectedRules.length ? `${selectedRules.length} 个清理项目` : '还没有选择清理项目'}<br /><span>删除后无法从回收站恢复。</span></p></div>
            {(incomplete || actionHints.length > 0) && <div className="cc-check-notice" role="status">{incomplete && <p>部分位置未能检查，下方只列出已找到的文件。</p>}{actionHints.length > 0 && <p>{actionHints.join('；')}</p>}</div>}
            <div className="cc-list-heading"><span>{stage === 'preview' ? '选择要清理的项目' : '本次清理清单'}</span><span className="cc-safe">{stage === 'preview' ? '不想清理的项目可以取消勾选' : '将直接删除'}</span></div>
            {stage === 'preview' && availableRules.length > 0 && <p className="cc-hint">清理前请关闭相关软件，正在使用的文件会跳过。</p>}
            <div className="cc-rules">{(stage === 'preview' ? availableRules : selectedRules).map((rule) => <RuleRow key={rule.id} rule={rule} checked={selected.has(rule.id)} disabled={stage !== 'preview'} onChange={() => toggle(rule.id)} />)}</div>
            {stage === 'preview' && closableRules.length > 0 && <section className="cc-blocked"><div className="cc-list-heading"><span>关闭软件后可清理</span><strong>{formatBytes(blockedBytes)}</strong></div><p>已发现这些缓存，但软件仍在运行。关闭后重新扫描，再决定是否清理。</p>{closableRules.map((rule) => <div className="cc-blocked-rule" key={rule.id}><span>{rule.title}</span><strong>{formatBytes(rule.bytes)}</strong></div>)}</section>}
            {stage === 'preview' && manualRules.length > 0 && <section className="cc-blocked cc-manual"><div className="cc-list-heading"><span>其他缓存空间参考</span><span className="cc-safe">仅查看占用</span></div><p>这类缓存可提高游戏或应用运行速度，当前仅显示占用。</p>{manualRules.map((rule) => <div key={rule.id}><div className="cc-blocked-rule"><span>{rule.title}</span><strong>{formatBytes(rule.bytes)}</strong></div><p>{rule.message || '此项目暂不支持在这里清理。'}</p></div>)}</section>}
            {stage === 'preview' && availableRules.length === 0 && blockedRules.length === 0 && <div className="cc-nothing"><CheckCircle2 size={26} /><strong>暂时没有找到需要清理的文件</strong><p>{incomplete ? '部分位置未能检查，可以稍后重新扫描。' : '以后使用一段时间，再来检查就好。'}</p></div>}
            {stage === 'preview' && otherRules.length > 0 && <details className="cc-other"><summary><ChevronRight size={13} />其他 {otherRules.length} 个检查项目</summary><div>{otherRules.map((rule) => <p key={rule.id}><span>{rule.title}</span><span>{rule.status === 'blocked' ? '暂时无法检查' : '暂无可清理文件'}</span></p>)}</div></details>}
            <details className="cc-diagnostics"><summary>查看检查详情</summary><p className="cc-hint">以下是检查说明和文件位置，无需了解这些信息也可以清理。</p>{plan.warnings.map((warning, i) => <p className="cc-technical" key={i}>{warning}</p>)}{plan.rules.map((rule) => <div className="cc-diagnostic-rule" key={rule.id}><strong>{rule.title}</strong><p>{rule.description}{rule.retention_days > 0 ? `；保留最近 ${rule.retention_days} 天的文件。` : ''}</p>{rule.message && <p>{rule.message}</p>}{rule.samples.length > 0 && <ul>{rule.samples.map((path, i) => <li key={i}>{path}</li>)}</ul>}</div>)}</details>
            {stage === 'executing' && <div className="cc-confirm" role="status"><Trash2 size={21} /><div><strong>正在直接删除，请稍候…</strong><p>执行期间请保持窗口打开，完成后显示实际结果。</p></div></div>}
          </>}
          {stage === 'error' && <div className="cc-intro cc-error"><FileWarning size={38} /><h3>本次操作未能完成</h3>{error.includes('本次未执行清理') ? <p className="cc-check-notice" role="alert">未能保存清理记录，本次未执行清理，请重新扫描。</p> : <p>请重新扫描后再试一次，也可以展开查看具体原因。</p>}<details className="cc-diagnostics"><summary>查看错误详情</summary><pre>{error}</pre></details></div>}

          </>}
        </div>
        <div className="cc-footer">
          {view === 'history' ? <><div className="cc-selection"><span>最多保留最近 100 次记录</span></div><div className="cc-actions"><button className="cc-button" disabled={historyLoading} onClick={showHistory}><RotateCcw size={14} />刷新记录</button>{result ? <><button className="cc-button" onClick={() => setView('disk')}><RotateCcw size={14} />再次检查</button><button className="cc-button cc-primary" onClick={close}><Check size={15} />完成</button></> : <button className="cc-button cc-primary" onClick={() => setView('disk')}>返回清理</button>}</div></> : <>
          <div className="cc-selection">{stage === 'preview' || stage === 'executing' ? <><span>已选 <b>{selectedRules.length}</b> 项 · {totalFiles.toLocaleString()} 个文件</span><strong>{formatBytes(totalBytes)}</strong></> : <span><ShieldCheck size={14} /> 所有操作都在本机完成</span>}</div>
          <div className="cc-actions">
            {stage === 'preview' && <><button className="cc-text" disabled={!selected.size} onClick={() => setSelected(new Set())}>清空选择</button><button className="cc-button" onClick={scan}><RotateCcw size={14} />重新扫描</button><button className="cc-button cc-primary" disabled={!totalFiles} onClick={() => void execute()} title="直接删除所选文件，不进入回收站"><Trash2 size={15} />直接删除</button></>}
            {stage === 'executing' && <button className="cc-button cc-primary" disabled><LoaderCircle size={15} className="cc-spin" />清理中…</button>}
            {(stage === 'initial' || stage === 'scanning' || stage === 'error') && <button className="cc-button cc-primary" disabled={busy} onClick={scan}>{busy ? <LoaderCircle size={15} className="cc-spin" /> : <Brush size={15} />}{busy ? '扫描中…' : stage === 'error' ? '重新扫描' : '扫描可清理项目'}</button>}
          </div>
          </>}
        </div>
        </>}
      </div>
    </div>, document.body,
  );
}

const historyStatuses = { completed: '已完成', partial: '部分完成', failed: '清理失败', incomplete: '未完成' };

export function HistoryView({ history, loading, error, compact }: { history: CleanupHistory | null; loading: boolean; error: string; compact: boolean }) {
  if (loading) return <div className="cc-nothing" role="status"><LoaderCircle className="cc-spin" size={26} /><strong>正在读取清理记录…</strong></div>;
  if (error) return <div className="cc-nothing"><FileWarning size={28} /><strong>暂时无法读取清理记录</strong><p>请点击「刷新记录」重试。</p><details className="cc-diagnostics"><summary>查看原因</summary><p className="cc-technical">{error}</p></details></div>;
  if (!history) return null;
  return <div className="cc-history">
    <div className="cc-history-heading"><h3>{compact ? '最近记录' : '清理记录'}</h3><p>查看每次清理的实际结果；旧版回收记录会单独标明。</p></div>
    {history.warnings.length > 0 && <div className="cc-check-notice" role="status"><p>部分记录未能读取，以下记录可能不完整。</p><details className="cc-diagnostics"><summary>查看详情</summary>{history.warnings.map((warning, index) => <p key={index}>{warning}</p>)}</details></div>}
    {history.records.length === 0 ? <div className="cc-nothing"><FolderClock size={32} /><strong>{compact ? '暂未读到已保存的记录' : '还没有清理记录'}</strong><p>{compact ? '本次结果保留在上方，可以刷新记录重试。' : '完成一次清理后，就能在这里查看结果。'}</p></div> : <details className="cc-recent-records" open={!compact}><summary>{compact ? '展开查看' : '最近记录'} · {history.records.length} 次</summary><div className="cc-history-list">{history.records.map((record) => {
      const started = new Date(record.started_at);
      const timestamp = Number.isNaN(started.getTime()) ? '时间未知' : started.toLocaleString('zh-CN', { year: 'numeric', month: '2-digit', day: '2-digit', hour: '2-digit', minute: '2-digit' });
      return <article className="cc-history-record" key={record.id}>
        <div className="cc-history-record-heading"><time dateTime={record.started_at}>{timestamp}</time><span className={`cc-history-status cc-status-${record.status}`}>{historyStatuses[record.status] ?? '未完成'}</span></div>
        <p className="cc-history-projects">{record.selected_rules.map((rule) => rule.title).join('、') || '未记录清理项目'}</p>
        {record.status === 'incomplete' ? <p className="cc-history-incomplete">这次清理尚未完成，实际清理量可能未记录。</p> : <div className="cc-history-amount"><span>{record.permanent_delete ? '已直接删除' : '历史移入回收站'}</span><strong>{formatBytes(record.deleted_bytes)}</strong><span>{record.deleted_files.toLocaleString()} 个文件</span></div>}
        <div className="cc-history-counts"><span>跳过 {record.skipped_files.toLocaleString()}</span><span>失败 {record.failed_files.toLocaleString()}</span></div>
        {record.message && <details className="cc-diagnostics"><summary>查看本次说明</summary><p className="cc-technical">{record.message}</p></details>}
      </article>;
    })}</div></details>}
  </div>;
}

function RuleRow({ rule, checked, disabled, onChange }: { rule: CacheRule; checked: boolean; disabled: boolean; onChange: () => void }) {
  const Icon = ruleIcons[rule.id as keyof typeof ruleIcons] ?? FolderClock;
  const available = rule.status === 'ready' && rule.file_count > 0;
  return <div className={`cc-rule${checked ? ' cc-selected' : ''}${!available ? ' cc-unavailable' : ''}`}>
    <div className="cc-rule-main">
      <input type="checkbox" aria-label={`选择${rule.title}`} checked={checked} disabled={disabled || !available} onChange={onChange} />
      <span className="cc-rule-icon"><Icon size={21} aria-hidden="true" /></span>
      <div className="cc-rule-copy"><strong>{rule.title}</strong><p>{impact(rule)}</p></div>
      <div className="cc-rule-quantity"><strong>{formatBytes(rule.bytes)}</strong>{rule.id === 'crash-dumps' && <span>按需选择</span>}</div>
    </div>
  </div>;
}

export function ResultCard({ result }: { result: CacheResult }) {
  const hasUnprocessed = result.failed_files > 0 || result.skipped_files > 0;
  const ResultIcon = hasUnprocessed ? FileWarning : CheckCircle2;
  const title = hasUnprocessed
    ? result.deleted_files > 0 ? '部分文件未能清理' : '本次未能清理文件'
    : isTauri ? '本次清理已结束' : '演示清理已结束';
  return <div className={`cc-result cc-current-result${hasUnprocessed ? ' cc-result-warning' : ''}`} role="status" aria-label="本次清理结果">
            <div className="cc-result-eyebrow">本次结果</div><span className="cc-result-icon"><ResultIcon size={32} /></span><h3>{title}</h3>
            <p>直接删除 <strong>{result.deleted_files.toLocaleString()}</strong> 个文件，合计 <strong>{formatBytes(result.deleted_bytes)}</strong></p>
            <div className="cc-result-counts"><span>跳过 <b>{result.skipped_files.toLocaleString()}</b></span><span>失败 <b>{result.failed_files.toLocaleString()}</b></span></div>
            <p className="cc-recycle-note"><Trash2 size={16} /> 直接删除不进入回收站；未处理的文件保持原样。</p>
            {result.history_warning && <div className="cc-history-save-error" role="alert"><strong>清理记录未能保存</strong><p>{result.history_warning}</p><p>本次清理结果如上，记录可能仍显示为未完成。</p></div>}
            {(result.skipped_files > 0 || result.failed_files > 0) && <p>有些文件未能清理，可以关闭相关软件后重新扫描。</p>}
            {result.messages.filter(isActionable).map((message, i) => <p className="cc-check-notice" key={i}>{message}</p>)}
            {result.messages.length > 0 && <details className="cc-diagnostics"><summary>查看清理详情</summary>{result.messages.map((message, i) => <p className="cc-technical" key={i}>{message}</p>)}</details>}
            {result.log_path && <details className="cc-log"><summary>查看清理日志路径</summary><pre>{result.log_path}</pre></details>}
          </div>;
}
