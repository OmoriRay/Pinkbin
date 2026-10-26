import { invoke } from '@tauri-apps/api/core';
import { isTauri } from './env';
import packageInfo from '../package.json';

export interface AppInfo {
  version: string;
  build_date: string;
}

export type AppInfoState =
  | { status: 'loading' }
  | { status: 'ready'; info: AppInfo }
  | { status: 'error' };

// Browser preview has no installed binary. Desktop always reads its own metadata.
const DEMO_INFO: AppInfo = { version: packageInfo.version, build_date: '2026-10-26' };

export async function loadAppInfo(): Promise<AppInfo> {
  if (!isTauri) return DEMO_INFO;
  const info = await invoke<AppInfo>('get_app_info');
  if (!info?.version || !info.build_date) throw new Error('版本信息不完整');
  return info;
}
