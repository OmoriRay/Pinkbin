import { invoke } from '@tauri-apps/api/core';
import { isTauri } from './env';
import { scan as demoScan } from './mocks';
import type { Node } from './types';

export interface ScanStatsEvent {
  mode: string; mft_attempted: boolean; mft_succeeded: boolean; mft_ms: number;
  walk_ms: number; build_tree_ms: number; scanner_total_ms: number; tag_ms: number;
  cmd_total_ms: number; files_seen: number; bytes_seen: number; dirs_in_acc: number;
}
export interface ScanSession {
  scan_id: string;
  path: string;
  status: 'running' | 'completed' | 'failed';
  progress: { files_seen: number; bytes_seen: number; current_path: string } | null;
  result: Node | null;
  error: string | null;
  stats: ScanStatsEvent | null;
}

// Preview only: real scan results and cleanup capabilities remain in Rust memory.
const DEMO_KEY = 'pinkbin.demo.scan-session.v1';
interface DemoSession { session: ScanSession; startedAt: number }
function readDemo(): DemoSession | null {
  try { return JSON.parse(sessionStorage.getItem(DEMO_KEY) || 'null'); } catch { return null; }
}
let demo = readDemo();
function saveDemo(value: DemoSession) {
  demo = value;
  try { sessionStorage.setItem(DEMO_KEY, JSON.stringify(value)); } catch { /* Preview storage may be unavailable. */ }
}
export async function startScanSession(path: string): Promise<ScanSession> {
  if (isTauri) return invoke('start_scan_session', { path });
  if (demo?.session.status === 'running') return (await getScanSession())!;
  const session: ScanSession = { scan_id: crypto.randomUUID(), path, status: 'running', progress: null, result: null, error: null, stats: null };
  saveDemo({ session, startedAt: Date.now() });
  return session;
}
export async function getScanSession(): Promise<ScanSession | null> {
  if (isTauri) return invoke('get_scan_session');
  const current = demo;
  if (!current || current.session.status !== 'running') return current?.session ?? null;
  const elapsed = Date.now() - current.startedAt;
  const result = elapsed >= 2000 ? await demoScan(current.session.path) : null;
  if (demo !== current) return demo?.session ?? null;
  const session: ScanSession = {
    ...current.session,
    status: result ? 'completed' : 'running', result,
    progress: { files_seen: Math.floor(elapsed * 120), bytes_seen: Math.floor(elapsed * 20_000_000), current_path: current.session.path },
  };
  saveDemo({ ...current, session });
  return session;
}
