import { useEffect, useRef, useState } from 'react';
import { X, CheckCircle2, Info, Eye, EyeOff, Settings2, PlugZap } from 'lucide-react';
import { api } from '../api';
import { isTauri } from '../env';
import type { AppInfoState } from '../appInfo';
import {
  loadSettings,
  saveSettings,
  clearSettings,
  detectProvider,
  testAdvisorConnection,
  type Provider,
  type ConnectionMode,
} from '../advisorClient';

type Props = { onClose: () => void; appInfo: AppInfoState };

const PROVIDER_LABEL: Record<Provider, string> = {
  openai: 'OpenAI 兼容接口',
  anthropic: 'Anthropic',
  gemini: 'Gemini',
  ollama: 'Ollama（本地，免 Key）',
};

// 手动开关里的选项要短，五个塞在一行；「已识别」标签用完整版说明。
const PROVIDER_LABEL_SHORT: Record<Provider, string> = {
  openai: 'OpenAI 兼容',
  anthropic: 'Anthropic',
  gemini: 'Gemini',
  ollama: 'Ollama',
};

export function Settings({ onClose, appInfo }: Props) {
  const [baseUrl, setBaseUrl] = useState('');
  const [apiKey, setApiKey] = useState('');
  const [model, setModel] = useState('');
  const [connectionMode, setConnectionMode] = useState<ConnectionMode>('default');
  const [showKey, setShowKey] = useState(false);
  const [msg, setMsg] = useState<string | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);
  const [testing, setTesting] = useState(false);
  const [testResult, setTestResult] = useState<{ kind: 'success' | 'error'; message: string } | null>(null);
  const testInFlight = useRef(false);
  const mounted = useRef(true);
  // undefined = 自动识别；手动指定时存具体 Provider。默认收起，不常驻展示。
  const [providerOverride, setProviderOverride] = useState<Provider | undefined>(undefined);
  const [showManual, setShowManual] = useState(false);

  useEffect(() => {
    const existing = loadSettings();
    if (existing) {
      setModel(existing.model);
      setApiKey(existing.apiKey);
      setBaseUrl(existing.baseUrl);
      setConnectionMode(existing.connectionMode ?? 'default');
      setProviderOverride(existing.providerOverride);
      setSaved(true);
    }
  }, []);

  const provider = providerOverride ?? detectProvider(baseUrl);
  const needsKey = provider !== 'ollama';
  const settingsKey = JSON.stringify([provider, model.trim(), apiKey.trim(), baseUrl.trim(), connectionMode]);
  const latestSettingsKey = useRef(settingsKey);
  latestSettingsKey.current = settingsKey;
  useEffect(() => { setTestResult(null); }, [settingsKey]);
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; };
  }, []);

  const testConnection = async () => {
    if (testInFlight.current) return;
    if (!baseUrl.trim() || !model.trim() || (needsKey && !apiKey.trim())) {
      setTestResult({ kind: 'error', message: !baseUrl.trim() ? '请填 Base URL' : !model.trim() ? '请填 Model 名' : '请填 API Key' });
      return;
    }
    testInFlight.current = true;
    setTesting(true);
    setTestResult(null);
    const key = settingsKey;
    try {
      const result = await testAdvisorConnection({ provider, model: model.trim(), apiKey: apiKey.trim(), baseUrl: baseUrl.trim(), providerOverride, connectionMode });
      if (mounted.current && latestSettingsKey.current === key) {
        setTestResult({ kind: 'success', message: `连接成功 · ${(result.durationMs / 1000).toFixed(1)} 秒` });
      }
    } catch (error) {
      if (mounted.current && latestSettingsKey.current === key) {
        setTestResult({ kind: 'error', message: error instanceof Error ? error.message : String(error) });
      }
    } finally {
      testInFlight.current = false;
      if (mounted.current) setTesting(false);
    }
  };

  const save = async () => {
    setErr(null); setMsg(null);
    if (!baseUrl.trim()) { setErr('请填 Base URL'); return; }
    if (!model.trim())   { setErr('请填 Model 名'); return; }
    if (needsKey && !apiKey.trim()) { setErr('请填 API Key'); return; }
    try {
      saveSettings({ provider, model: model.trim(), apiKey: apiKey.trim(), baseUrl: baseUrl.trim(), providerOverride, connectionMode });
      if (isTauri) {
        await api.setAdvisor(provider, model.trim(), needsKey ? apiKey.trim() : undefined, baseUrl.trim());
      }
      setMsg(testResult ? '已保存配置' : '已保存配置 · 尚未验证连接');
      setSaved(true);
    } catch (e) {
      setErr(String(e));
    }
  };

  const wipe = () => {
    clearSettings();
    setApiKey('');
    setBaseUrl('');
    setModel('');
    setConnectionMode('default');
    setProviderOverride(undefined);
    setShowManual(false);
    setSaved(false);
    setMsg('已清除本地保存的配置');
  };

  return (
    <div className="modal-bg" onClick={onClose}>
      <div className="modal settings-modal" role="dialog" aria-modal="true" aria-labelledby="settings-title" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <div id="settings-title">AI 顾问设置 {saved && <CheckCircle2 size={16} aria-label="已保存配置" style={{ verticalAlign: 'middle', marginLeft: 6, color: 'var(--pink-deep)' }} />}</div>
          <button className="ghost icon" aria-label="关闭 AI 顾问设置" onClick={onClose}><X size={16} /></button>
        </div>

        <p className="hint">
          <Info size={12} />
          <span>填写服务商提供的 Base URL、API Key 和模型名。支持兼容接口与本地 Ollama；保存只记录配置，可点击「测试」验证连接。</span>
        </p>

        <label className="field">
          <span>Base URL</span>
          <input
            value={baseUrl}
            onChange={(e) => setBaseUrl(e.target.value)}
            placeholder="https://api.openai.com/v1"
          />
        </label>

        {baseUrl.trim() && (
          <div className="provider-detect">
            <span className="badge">
              接口类型：{PROVIDER_LABEL[provider]}
              {providerOverride && ' · 手动'}
            </span>
            <button type="button" className="provider-detect-toggle" onClick={() => setShowManual((v) => !v)}>
              <Settings2 size={11} />
              手动指定
            </button>
          </div>
        )}

        <p className="provider-explanation muted small">
          {provider === 'openai'
            ? '“OpenAI 兼容接口”仅表示请求格式，不代表服务商是 OpenAI。实际服务由你填写的 Base URL 决定。'
            : '接口类型决定请求格式，实际服务由你填写的 Base URL 决定。'}
        </p>

        {baseUrl.trim() && showManual && (
          <div className="seg seg-5">
            <button
              type="button"
              className={`seg-opt${providerOverride === undefined ? ' active' : ''}`}
              onClick={() => setProviderOverride(undefined)}
            >
              自动
            </button>
            {(Object.keys(PROVIDER_LABEL) as Provider[]).map((p) => (
              <button
                key={p}
                type="button"
                className={`seg-opt${providerOverride === p ? ' active' : ''}`}
                onClick={() => setProviderOverride(p)}
              >
                {PROVIDER_LABEL_SHORT[p]}
              </button>
            ))}
          </div>
        )}

        {needsKey && (
          <label className="field">
            <span>API Key（保存在本机，用于请求所填服务）</span>
            <div style={{ display: 'flex', gap: 6 }}>
              <input
                type={showKey ? 'text' : 'password'}
                value={apiKey}
                onChange={(e) => setApiKey(e.target.value)}
                placeholder="sk-..."
                style={{ flex: 1 }}
              />
              <button
                type="button"
                className="ghost icon"
                onClick={() => setShowKey((v) => !v)}
                title={showKey ? '隐藏' : '显示'}
              >
                {showKey ? <EyeOff size={14} /> : <Eye size={14} />}
              </button>
            </div>
          </label>
        )}

        <div className="field">
          <div className="settings-model-label">
            <label htmlFor="advisor-model">Model · 模型名</label>
            <button type="button" className="settings-test" aria-label="测试 AI 连接" title="用当前填写的配置发送一条简短测试消息，不会自动保存" onClick={testConnection} disabled={testing}>
              <PlugZap size={12} aria-hidden="true" />{testing ? '测试中…' : '测试'}
            </button>
          </div>
          <input
            id="advisor-model"
            value={model}
            onChange={(e) => setModel(e.target.value)}
            placeholder="gpt-4o-mini · deepseek-chat · claude-haiku-4-5 …"
          />
          {testResult && <div className="settings-test-result" data-state={testResult.kind} role={testResult.kind === 'error' ? 'alert' : 'status'}>{testResult.message}</div>}
        </div>

        {isTauri && <div className="field">
          <span>AI 连接方式</span>
          <div className="seg seg-2" role="group" aria-label="AI 连接方式">
            <button type="button" className={`seg-opt${connectionMode === 'default' ? ' active' : ''}`} aria-pressed={connectionMode === 'default'} onClick={() => setConnectionMode('default')}>默认连接</button>
            <button type="button" className={`seg-opt${connectionMode === 'direct' ? ' active' : ''}`} aria-pressed={connectionMode === 'direct'} onClick={() => setConnectionMode('direct')}>直接连接</button>
          </div>
          <small className="muted">直接连接不经过代理，仅影响 Pinkbin 的 AI 请求。连接失败时可以切换后重试。</small>
        </div>}

        {msg && <div className="ok">{msg}</div>}
        {err && <div className="error">{err}</div>}

        <div className="modal-actions">
          {saved && <button className="ghost" onClick={wipe}>清除</button>}
          <button className="primary" onClick={save}>保存</button>
          <button className="ghost" onClick={onClose}>关闭</button>
        </div>

        <p className="muted small" style={{ marginTop: 4 }}>
          Pinkbin 只把目录元数据发给 AI（路径、大小、文件数、扩展名分布、抽样路径），<strong>不会</strong>读取或上传文件内容。
        </p>
        <section className="app-about" aria-label="关于 Pinkbin">
          <h2>关于 Pinkbin</h2>
          {appInfo.status === 'ready' ? (
            <dl>
              <div><dt>版本</dt><dd>{appInfo.info.edition} v{appInfo.info.version}</dd></div>
              <div><dt>构建日期</dt><dd>{appInfo.info.build_date}</dd></div>
            </dl>
          ) : <p className="muted small">{appInfo.status === 'loading' ? '正在读取版本信息…' : '版本暂不可读取，请重新启动后查看。'}</p>}
          {!isTauri && <p className="muted small">浏览器演示 · 版本信息为演示数据</p>}
        </section>
      </div>
    </div>
  );
}
