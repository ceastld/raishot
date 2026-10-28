import React, { useEffect, useMemo, useRef, useState } from "react";
import { DEFAULT_ENDPOINT, DEFAULT_MODEL, runStream, runRequest, splitParagraphs } from "./translateApi.js";

const SAMPLE = `RustShot 0.9 adds a recognition menu next to the capture toolbar. Text recognition, table recognition, image translation and QR codes now share one button that remembers your last choice.

The OCR panel opens next to the selection and keeps the original layout: paragraphs are merged by line spacing and indentation, and tables stay inline as real rows and columns.

Hovering a paragraph in the panel highlights the matching region on the screenshot, so you can check what was recognized without leaving the capture.

Translation only sends the recognized text to the service you configure. Images, the clipboard and other parts of the screenshot never leave your computer.

Pinned screenshots keep annotations as vectors. Copying, saving or dragging a pin renders the image at that moment, so zooming never blurs arrows or text.

Press Esc at any time to leave the text layer. Your selection and annotations stay exactly where they were.`;

const MODES = [
  { id: "whole", label: "整段等待", tag: "旧版", how: "一次非流式请求，全部生成完才显示" },
  { id: "stream", label: "流式", tag: "已实现", how: "同一请求流式返回，逐段对照显示（空流按设置重发一次）" },
  { id: "parallel", label: "分段并发 + 流式", tag: "候选", how: "每段一个流式请求，按并发数同时跑" },
];

const DEFAULT_CFG = {
  endpoint: DEFAULT_ENDPOINT,
  model: DEFAULT_MODEL,
  apiKey: "",
  target: "auto",
  concurrency: 4,
  retryEmpty: true,
  direct: false,
  order: "sequential",
  modes: { whole: true, stream: true, parallel: true },
  source: SAMPLE,
};
const CFG_KEY = "rs-wb-translate-cfg";
const HISTORY_KEY = "rs-wb-translate-history";

function load(key, fallback) {
  try {
    const v = JSON.parse(localStorage.getItem(key));
    return v ?? fallback;
  } catch {
    return fallback;
  }
}
function save(key, value) {
  try {
    localStorage.setItem(key, JSON.stringify(value));
  } catch {}
}

const fmt = (ms) => (ms == null ? "—" : `${(ms / 1000).toFixed(2)}s`);
const host = (endpoint) => {
  try {
    return new URL(endpoint).host;
  } catch {
    return endpoint;
  }
};
const median = (xs) => {
  const s = xs.filter((x) => x != null).sort((a, b) => a - b);
  if (!s.length) return null;
  const m = s.length >> 1;
  return s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2;
};
const emptyRun = () => ({ state: "idle", text: "", m: null, paras: [], startedAt: 0 });

export default function TranslateBench() {
  const [cfg, setCfg] = useState(() => ({ ...DEFAULT_CFG, ...load(CFG_KEY, {}) }));
  const [history, setHistory] = useState(() => load(HISTORY_KEY, []));
  const [runs, setRuns] = useState({ whole: emptyRun(), stream: emptyRun(), parallel: emptyRun() });
  const [running, setRunning] = useState(false);
  const [proxy, setProxy] = useState(undefined);
  const [, setTick] = useState(0);
  const ctrl = useRef(null);

  useEffect(() => save(CFG_KEY, cfg), [cfg]);
  useEffect(() => {
    fetch("/api/relay-info")
      .then((r) => r.json())
      .then((v) => setProxy(v.proxy))
      .catch(() => setProxy(null));
  }, []);
  const route = cfg.direct || !proxy ? "直连" : "代理";
  useEffect(() => save(HISTORY_KEY, history), [history]);
  useEffect(() => {
    if (!running) return;
    const id = setInterval(() => setTick((t) => t + 1), 100);
    return () => clearInterval(id);
  }, [running]);

  const paragraphs = useMemo(() => splitParagraphs(cfg.source), [cfg.source]);
  const set = (patch) => setCfg((c) => ({ ...c, ...patch }));
  const patchRun = (mode, patch) =>
    setRuns((r) => ({ ...r, [mode]: { ...r[mode], ...(typeof patch === "function" ? patch(r[mode]) : patch) } }));

  const record = (mode, m, extra = {}) => {
    const chars = [...(m.text || "")].length;
    const genMs = m.totalMs != null && m.firstMs != null ? m.totalMs - m.firstMs : null;
    const entry = {
      at: new Date().toISOString(),
      host: host(cfg.endpoint),
      model: cfg.model,
      mode,
      ok: m.ok,
      status: m.status,
      firstMs: m.firstMs,
      totalMs: m.totalMs,
      chars,
      cps: genMs && genMs > 50 ? Math.round((chars / genMs) * 1000) : null,
      error: m.error || "",
      paragraphs: paragraphs.length,
      concurrency: mode === "parallel" ? cfg.concurrency : 1,
      order: cfg.order,
      route,
      retries: m.retries || 0,
      ...extra,
    };
    setHistory((h) => [entry, ...h].slice(0, 300));
  };

  async function runSingle(mode, signal) {
    const text = paragraphs.join("\n\n");
    const startedAt = performance.now();
    patchRun(mode, { state: "running", text: "", m: null, paras: [], startedAt });
    const onText = (t) => patchRun(mode, { text: t });
    const m =
      mode === "whole"
        ? await runRequest(cfg, text, { stream: false, signal, onText, t0: startedAt })
        : await runStream(cfg, text, { signal, onText, t0: startedAt });
    patchRun(mode, { state: "done", m, text: m.text || "" });
    if (m.error !== "已停止") record(mode, m);
  }

  async function runParallel(signal) {
    const startedAt = performance.now();
    const paras = paragraphs.map(() => ({ text: "", start: null, first: null, end: null, error: "", status: 0, retries: 0 }));
    patchRun("parallel", { state: "running", text: "", m: null, paras, startedAt });
    const update = (i, patch) =>
      patchRun("parallel", (r) => ({ paras: r.paras.map((p, j) => (j === i ? { ...p, ...patch } : p)) }));
    let next = 0;
    const results = [];
    const worker = async () => {
      while (next < paragraphs.length && !signal.aborted) {
        const i = next++;
        const t0 = performance.now();
        const start = t0 - startedAt;
        update(i, { start });
        const m = await runStream(cfg, paragraphs[i], { signal, t0, onText: (t) => update(i, { text: t }) });
        const p = {
          text: m.text,
          start,
          first: m.firstMs != null ? start + m.firstMs : null,
          end: start + m.totalMs,
          error: m.error,
          status: m.status,
          retries: m.retries,
        };
        update(i, p);
        results[i] = p;
      }
    };
    await Promise.all(Array.from({ length: Math.max(1, Math.min(cfg.concurrency, paragraphs.length)) }, worker));
    const done = results.filter(Boolean);
    const okAll = done.length === paragraphs.length && done.every((p) => !p.error);
    const firsts = done.map((p) => p.first).filter((x) => x != null);
    const m = {
      ok: okAll,
      status: done.find((p) => p.error)?.status ?? done[0]?.status ?? 0,
      firstMs: firsts.length ? Math.min(...firsts) : null,
      totalMs: done.length ? Math.max(...done.map((p) => p.end)) : null,
      text: done.map((p) => p.text).join("\n\n"),
      error: done.filter((p) => p.error).length ? `${done.filter((p) => p.error).length}/${paragraphs.length} 段失败：${done.find((p) => p.error).error}` : "",
      retries: done.reduce((n, p) => n + (p.retries || 0), 0),
    };
    patchRun("parallel", { state: "done", m });
    if (!signal.aborted) record("parallel", m);
  }

  async function start() {
    if (running || !paragraphs.length) return;
    const controller = new AbortController();
    ctrl.current = controller;
    setRunning(true);
    const selected = MODES.filter((x) => cfg.modes[x.id]).map((x) => x.id);
    setRuns((r) => Object.fromEntries(Object.keys(r).map((k) => [k, selected.includes(k) ? emptyRun() : r[k]])));
    const task = (mode) => (mode === "parallel" ? runParallel(controller.signal) : runSingle(mode, controller.signal));
    try {
      if (cfg.order === "together") await Promise.all(selected.map(task));
      else for (const mode of selected) {
        if (controller.signal.aborted) break;
        await task(mode);
      }
    } finally {
      setRunning(false);
    }
  }

  const stats = useMemo(() => {
    const h = host(cfg.endpoint);
    return MODES.map(({ id }) => {
      const rows = history.filter((e) => e.mode === id && e.host === h).slice(0, 10);
      const ok = rows.filter((e) => e.ok);
      return { id, n: rows.length, ok: ok.length, first: median(ok.map((e) => e.firstMs)), total: median(ok.map((e) => e.totalMs)) };
    });
  }, [history, cfg.endpoint]);

  const copyHistory = async () => {
    const json = JSON.stringify(history, null, 2);
    try {
      await navigator.clipboard.writeText(json);
    } catch {
      window.prompt("复制以下 JSON", json);
    }
  };

  return (
    <div className="bench">
      <header className="bench-head">
        <h1>翻译测速</h1>
        <p className="muted">
          真实调用。请求经本机 Vite 服务转发（绕过浏览器跨域），提示词、参数和代理选择同 <code>src/translate.rs</code>
          （当前：{proxy === undefined ? "检测中" : route === "代理" ? `经代理 ${proxy}` : "直连"}）。配置和历史保存在本机浏览器。
        </p>
      </header>

      <section className="config">
        <label className="f wide">
          <span>接口根地址</span>
          <input id="endpoint" value={cfg.endpoint} onChange={(e) => set({ endpoint: e.target.value })} spellCheck={false} />
        </label>
        <label className="f">
          <span>模型</span>
          <input id="model" value={cfg.model} onChange={(e) => set({ model: e.target.value })} spellCheck={false} />
        </label>
        <label className="f">
          <span>API Key（可选）</span>
          <input id="apikey" type="password" value={cfg.apiKey} onChange={(e) => set({ apiKey: e.target.value })} placeholder="默认服务无需密钥" autoComplete="off" />
        </label>
        <label className="f narrow">
          <span>目标语言</span>
          <select id="target" value={cfg.target} onChange={(e) => set({ target: e.target.value })}>
            <option value="auto">auto</option>
            <option value="zh-CN">zh-CN</option>
            <option value="en">en</option>
            <option value="ja">ja</option>
          </select>
        </label>
        <label className="f narrow">
          <span>并发数</span>
          <input id="concurrency" type="number" min={1} max={12} value={cfg.concurrency} onChange={(e) => set({ concurrency: Math.max(1, Math.min(12, Number(e.target.value) || 1)) })} />
        </label>
        <label className="f">
          <span>运行顺序</span>
          <select id="order" value={cfg.order} onChange={(e) => set({ order: e.target.value })}>
            <option value="sequential">依次（互不抢占，计时公平）</option>
            <option value="together">同时（直观对比）</option>
          </select>
        </label>
        <div className="f checks">
          <span>模式</span>
          <div>
            {MODES.map((x) => (
              <label key={x.id} className="check">
                <input id={`mode-${x.id}`} type="checkbox" checked={cfg.modes[x.id]} onChange={(e) => set({ modes: { ...cfg.modes, [x.id]: e.target.checked } })} />
                {x.label}
              </label>
            ))}
            <label className="check">
              <input id="retry-empty" type="checkbox" checked={cfg.retryEmpty} onChange={(e) => set({ retryEmpty: e.target.checked })} />
              空流重发一次
            </label>
            <label className="check" title={proxy ? `默认与 App 一样经 ${proxy}` : "未检测到系统代理"}>
              <input id="direct" type="checkbox" checked={cfg.direct} disabled={!proxy} onChange={(e) => set({ direct: e.target.checked })} />
              直连（不走代理）
            </label>
          </div>
        </div>
        <details className="f full">
          <summary>
            源文 · {paragraphs.length} 段 · {[...cfg.source].length} 字符（空行分段）
          </summary>
          <textarea id="source" rows={10} value={cfg.source} onChange={(e) => set({ source: e.target.value })} />
          <div className="row">
            <button type="button" className="btn" onClick={() => set({ source: SAMPLE })}>恢复示例</button>
            <button type="button" className="btn" onClick={() => set({ ...DEFAULT_CFG, apiKey: cfg.apiKey, source: cfg.source })}>恢复默认服务</button>
          </div>
        </details>
        <div className="actions">
          {running ? (
            <button type="button" className="btn danger" onClick={() => ctrl.current?.abort()}>停止</button>
          ) : (
            <button type="button" className="btn primary" onClick={start} disabled={!paragraphs.length || !MODES.some((x) => cfg.modes[x.id])}>
              开始测试
            </button>
          )}
        </div>
      </section>

      <section className="panels">
        {MODES.filter((x) => cfg.modes[x.id]).map((x) => (
          <Panel key={x.id} mode={x} run={runs[x.id]} paragraphs={paragraphs} />
        ))}
      </section>

      <section className="history">
        <div className="history-head">
          <h2>历史</h2>
          <span className="muted">
            {host(cfg.endpoint)} 最近 10 次成功中位数：
            {stats.map((s) => (
              <span key={s.id} className="stat">
                {MODES.find((m) => m.id === s.id).label} 首字 {fmt(s.first)} · 完成 {fmt(s.total)}（成功 {s.ok}/{s.n}）
              </span>
            ))}
          </span>
          <div className="row">
            <button type="button" className="btn" onClick={copyHistory} disabled={!history.length}>复制 JSON</button>
            <button type="button" className="btn" onClick={() => setHistory([])} disabled={!history.length}>清空</button>
          </div>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>时间</th>
                <th>服务</th>
                <th>模式</th>
                <th>结果</th>
                <th className="num">首字</th>
                <th className="num">完成</th>
                <th className="num">字/秒</th>
                <th className="num">段×并发</th>
                <th>说明</th>
              </tr>
            </thead>
            <tbody>
              {history.length === 0 && (
                <tr>
                  <td colSpan={9} className="muted">还没有记录。点“开始测试”后每个模式记一行。</td>
                </tr>
              )}
              {history.map((e, i) => (
                <tr key={i}>
                  <td>{new Date(e.at).toLocaleTimeString()}</td>
                  <td title={e.model}>{e.host}</td>
                  <td>{MODES.find((m) => m.id === e.mode)?.label ?? e.mode}</td>
                  <td>
                    <span className={`chip ${e.ok ? "ok" : "bad"}`}>{e.ok ? "成功" : e.status || "失败"}</span>
                  </td>
                  <td className="num">{fmt(e.firstMs)}</td>
                  <td className="num">{fmt(e.totalMs)}</td>
                  <td className="num">{e.cps ?? "—"}</td>
                  <td className="num">
                    {e.paragraphs}×{e.concurrency}
                  </td>
                  <td className="muted clip" title={e.error}>
                    {[e.route, e.order === "together" ? "同时" : "", e.retries ? `重发 ${e.retries}` : "", e.error].filter(Boolean).join(" · ")}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </section>
    </div>
  );
}

function Panel({ mode, run, paragraphs }) {
  const elapsed = run.state === "running" ? performance.now() - run.startedAt : run.m?.totalMs;
  const m = run.m;
  const chars = [...(run.text || "")].length;
  const first =
    m?.firstMs ??
    (mode.id === "parallel"
      ? (() => {
          const xs = run.paras.map((p) => p.first).filter((x) => x != null);
          return xs.length ? Math.min(...xs) : null;
        })()
      : null);
  const genMs = m?.totalMs != null && m?.firstMs != null ? m.totalMs - m.firstMs : null;
  const cps = genMs && genMs > 50 ? Math.round(([...(m.text || "")].length / genMs) * 1000) : null;
  const state = run.state === "running" ? "进行中" : run.state === "done" ? (m?.ok ? "成功" : "失败") : "未运行";

  return (
    <article className="panel">
      <div className="ph">
        <h2>{mode.label}</h2>
        <span className={`tag ${mode.id}`}>{mode.tag}</span>
        <span className={`chip ${run.state === "done" ? (m?.ok ? "ok" : "bad") : run.state === "running" ? "live" : ""}`}>{state}</span>
      </div>
      <p className="how">{mode.how}</p>
      <div className="metrics">
        <Metric label="首字" value={fmt(first)} dim={first == null} />
        <Metric label={run.state === "running" ? "已用时" : "完成"} value={fmt(elapsed)} dim={run.state !== "done"} />
        <Metric label="响应头" value={fmt(m?.headersMs)} dim={m?.headersMs == null} />
        <Metric label="字/秒" value={cps ?? "—"} dim={cps == null} />
      </div>
      {m?.error && <div className="err">{m.error}</div>}
      {mode.id === "parallel" && run.paras.length > 0 && <Timeline paras={run.paras} now={elapsed} />}
      <div className="pbody">
        {run.state === "idle" && <div className="muted pad">点“开始测试”。</div>}
        {mode.id === "parallel"
          ? run.paras.map((p, i) => <Card key={i} src={paragraphs[i]} dst={p.text} pending={p.end == null} error={p.error} />)
          : run.state !== "idle" && <SingleBody text={run.text} paragraphs={paragraphs} live={run.state === "running"} />}
        {run.state === "running" && mode.id !== "parallel" && chars === 0 && <div className="skel"><i /><i /><i /></div>}
      </div>
    </article>
  );
}

function SingleBody({ text, paragraphs, live }) {
  if (!text) return null;
  const parts = text.split("\n\n").map((s) => s.trim()).filter(Boolean);
  if (parts.length > paragraphs.length || (!live && parts.length !== paragraphs.length)) {
    return <div className={`dst plain${live ? " caret" : ""}`}>{text}</div>;
  }
  return parts.map((p, i) => <Card key={i} src={paragraphs[i]} dst={p} pending={live && i === parts.length - 1} />);
}

function Card({ src, dst, pending, error }) {
  return (
    <div className="card">
      <div className="src">{src}</div>
      {error ? (
        <div className="err small">{error}</div>
      ) : dst ? (
        <div className={`dst${pending ? " caret" : ""}`}>{dst}</div>
      ) : (
        <div className="skel">
          <i />
          <i />
        </div>
      )}
    </div>
  );
}

function Metric({ label, value, dim }) {
  return (
    <div>
      <span>{label}</span>
      <strong className={dim ? "dim" : ""}>{value}</strong>
    </div>
  );
}

// 每段一行：灰色＝排队到首字，强调色＝生成，红色＝失败
function Timeline({ paras, now }) {
  const max = Math.max(1, now || 0, ...paras.map((p) => p.end ?? 0));
  const pct = (ms) => `${(Math.max(0, ms) / max) * 100}%`;
  return (
    <div className="timeline" aria-label="各段请求时间线">
      {paras.map((p, i) => {
        if (p.start == null) return <div key={i} className="lane" />;
        const firstAt = p.first ?? (p.end ?? now);
        const endAt = p.end ?? now;
        return (
          <div key={i} className="lane" title={`第 ${i + 1} 段：开始 ${fmt(p.start)} · 首字 ${fmt(p.first)} · 完成 ${fmt(p.end)}`}>
            <i className={`wait${p.error ? " bad" : ""}`} style={{ left: pct(p.start), width: pct(firstAt - p.start) }} />
            {p.first != null && <i className="gen" style={{ left: pct(p.first), width: pct(endAt - p.first) }} />}
          </div>
        );
      })}
      <div className="axis">
        <span>0</span>
        <span>{fmt(max)}</span>
      </div>
    </div>
  );
}
