import { useEffect, useRef, useState } from 'react';
import { startScanSession, getScanSession, type ScanSession, type ScanStatsEvent } from './scanSessionApi';
import { useStore } from './store';

export interface ScanDiag {
  backend: ScanStatsEvent | null;
  ipcMs: number | null;
  scanCallMs: number;
  setRootMs: number;
  totalMs: number;
}

export function useScanSession() {
  const [pickedPath, setPickedPath] = useState('');
  const [scanning, setScanning] = useState(false);
  const [restoring, setRestoring] = useState(true);
  const [scanProgress, setScanProgress] = useState<{ files: number; bytes: number; path: string } | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [diag, setDiag] = useState<ScanDiag | null>(null);
  const alive = useRef(false);
  const generation = useRef(0);
  const busy = useRef(true);
  const applied = useRef('');
  const timer = useRef<ReturnType<typeof setTimeout>>();

  const apply = (session: ScanSession | null) => {
    setRestoring(false);
    if (!session) { busy.current = false; setScanning(false); return; }
    setPickedPath(session.path);
    if (session.progress) setScanProgress({ files: session.progress.files_seen, bytes: session.progress.bytes_seen, path: session.progress.current_path });
    busy.current = session.status === 'running';
    setScanning(busy.current);
    setErr(session.error);
    if (session.status === 'completed' && session.result && applied.current !== session.scan_id) {
      applied.current = session.scan_id;
      const start = performance.now();
      useStore.getState().setRoot(session.result);
      useStore.getState().selectPath(session.result.path);
      if (session.stats) setDiag({ backend: session.stats, ipcMs: null, scanCallMs: session.stats.cmd_total_ms, setRootMs: performance.now() - start, totalMs: session.stats.cmd_total_ms });
    }
  };
  const poll = async (token: number) => {
    try {
      const session = await getScanSession();
      if (!alive.current || token !== generation.current) return;
      apply(session);
      if (session?.status === 'running') timer.current = setTimeout(() => void poll(token), 600);
    } catch {
      if (!alive.current || token !== generation.current) return;
      setErr('暂时无法读取扫描进度，正在重新连接…');
      // A lost UI response is not permission to restart an existing background job.
      timer.current = setTimeout(() => void poll(token), 1500);
    }
  };
  useEffect(() => {
    alive.current = true;
    const token = ++generation.current;
    void poll(token);
    return () => { alive.current = false; ++generation.current; clearTimeout(timer.current); };
    // The backend owns the job; effect cleanup only detaches this view.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const scan = async () => {
    if (busy.current || !pickedPath) return;
    busy.current = true;
    const token = ++generation.current;
    clearTimeout(timer.current);
    setScanning(true); setErr(null); setScanProgress(null); setDiag(null);
    try {
      const session = await startScanSession(pickedPath);
      if (!alive.current || token !== generation.current) return;
      apply(session);
      if (session.status === 'running') timer.current = setTimeout(() => void poll(token), 300);
    } catch (error) {
      if (!alive.current || token !== generation.current) return;
      // The start response may be lost after the backend accepted the job.
      setErr(`正在确认扫描状态：${String(error)}`);
      void poll(token);
    }
  };
  return { pickedPath, setPickedPath, scanning, restoring, scanProgress, err, diag, scan };
}
