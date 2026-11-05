// Browser preview only. Native inventories stay in Rust memory.
import { SHARED_SCAN_INVALIDATED } from './sharedScanEvents';
const KEY = 'pinkbin.demo.shared-inventory.v1';
const TTL = 15 * 60 * 1000;
const DURATION = 2400;
interface Capture { key: string; at: number }
interface DemoInventory {
  epoch: number;
  ready: Record<string, Capture>;
  active: { roots: string[]; started: number; key: string } | null;
  traversals: string[][];
}
export interface DemoRequest { roots: string[]; epoch: number; reused: boolean }
export const demoRoot = (path: string) => /^[A-Za-z]:[\\/]?$/.test(path) ? `${path[0].toUpperCase()}:\\` : path;
function read(): DemoInventory {
  try { const s = JSON.parse(sessionStorage.getItem(KEY) || 'null'); if (s) return s; } catch { /* Preview only. */ }
  return { epoch: 0, ready: {}, active: null, traversals: [] };
}
function save(s: DemoInventory) { sessionStorage.setItem(KEY, JSON.stringify(s)); }
function advance(s: DemoInventory) {
  const now = Date.now();
  for (const [root, value] of Object.entries(s.ready)) if (now - value.at > TTL) delete s.ready[root];
  if (s.active && now - s.active.started >= DURATION) {
    for (const root of s.active.roots) s.ready[root] = { key: s.active.key, at: s.active.started };
    s.active = null;
  }
}
function startMissing(s: DemoInventory, roots: string[]) {
  const missing = roots.filter(root => !s.ready[root]);
  if (missing.length && !s.active) {
    s.active = { roots: missing, started: Date.now(), key: crypto.randomUUID() };
    s.traversals.push(missing);
  }
}
export function startDemoInventory(roots: string[], force = false): DemoRequest {
  const s = read(); advance(s);
  roots = roots.map(demoRoot);
  if (force && !s.active) {
    s.epoch++; s.ready = {};
    save(s);
    window.dispatchEvent(new Event(SHARED_SCAN_INVALIDATED));
  }
  const reused = roots.some(root => !!s.ready[root]) || !!s.active?.roots.some(root => roots.includes(root));
  startMissing(s, roots); save(s);
  return { roots, epoch: s.epoch, reused };
}
export function pollDemoInventory(request: DemoRequest) {
  const s = read(); advance(s);
  if (request.epoch !== s.epoch) return { invalidated: true, completed: false, fraction: 0, key: '', at: 0 };
  startMissing(s, request.roots); save(s);
  const completed = request.roots.every(root => !!s.ready[root]);
  const fraction = completed ? 1 : Math.min(.99, (Date.now() - (s.active?.started ?? Date.now())) / DURATION);
  return { invalidated: false, completed, fraction, key: request.roots.map(r => s.ready[r]?.key).join(':'), at: Math.min(...request.roots.map(r => s.ready[r]?.at ?? Date.now())) };
}
export function demoInventoryEpoch() { return read().epoch; }
export function invalidateDemoInventory() {
  const s = read(); s.epoch++; s.ready = {}; s.active = null; save(s);
  window.dispatchEvent(new Event(SHARED_SCAN_INVALIDATED));
}
