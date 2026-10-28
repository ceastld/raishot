'use strict';

// 仅接管 supervisor 新建的 Chrome；本文件不启动、导航或关闭其它浏览器。
const fs = require('node:fs');
const path = require('node:path');
const crypto = require('node:crypto');
const { fileURLToPath } = require('node:url');
const { spawn } = require('node:child_process');
const { Worker, isMainThread, parentPort, workerData } = require('node:worker_threads');
const readline = require('node:readline');

// PNG 解码在工作线程完成，避免同步解码挡住 live lease 心跳。
if (!isMainThread) {
  try {
    const { PNG } = require(workerData.pngjs_module);
    const png = PNG.sync.read(fs.readFileSync(workerData.png_path), { checkCRC: true });
    let transparent = 0;
    for (let i = 3; i < png.data.length; i += 4) if (png.data[i] !== 255) transparent++;
    parentPort.postMessage({ width: png.width, height: png.height, transparent });
  } catch (error) {
    parentPort.postMessage({ error: String(error.stack || error) });
  }
}

const protocol = 1;
const heartbeatMs = 75;
const maxLineBytes = 128 * 1024;
const maxLogBytes = 8 * 1024 * 1024;
const manualMaxRequests = 96;
const manualScope = '仅自有 Chrome 的 renderer 校准及手动 DOM wheel；非 Auto、32k 或网页字形验收';
const nativeAutoScope = '仅自有 Chrome、当前 DPI、纵向 1×生产 native Auto；Node 只观察，不替代原生输入；非32k、真实网站或生产工具栏验收';
const nativeUiAutoScope = '仅自有 Chrome、当前 DPI、真实 GPUI 两级工具栏原生 Auto 场景；Node 只观察，不替代原生输入；非32k或真实网站验收';
const errorText = error => String(error && (error.stack || error.message) || error);
const ensure = (condition, message) => { if (!condition) throw new Error(message); };
const integer = value => Number.isFinite(value) && Math.abs(value - Math.round(value)) < 1e-6;
const inside = (candidate, root) => candidate === root || candidate.startsWith(root + path.sep);
const canonicalJson = value => JSON.stringify(value, null, 2) + '\n';
// 唯一 DOM bridge 为已冻结页面 API；没有 scrollTo/scrollBy/赋值/dispatchEvent 路由。
const domOperations = Object.freeze({
  ready: () => Boolean(window.__rustshotLongBrowserQa),
  configure: value => window.__rustshotLongBrowserQa.configureMode(value),
  scenario: value => window.__rustshotLongBrowserQa.configureScenario(value),
  measure: () => window.__rustshotLongBrowserQa.measure(),
  settled: ({ previous, movement }) => window.__rustshotLongBrowserQa.settled(previous, movement),
});
async function bounded(promise, milliseconds, message) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(message)), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}

async function main() {
  ensure(process.argv.length === 3 || (process.argv.length === 5 && process.argv[3] === '--mode'), '用法：node qa-long-browser-control.cjs <config.json> [--mode manual|native-auto]');
  const configPath = fs.realpathSync(process.argv[2]);
  const config = JSON.parse(fs.readFileSync(configPath, 'utf8').replace(/^\uFEFF/, ''));
  ensure(config.protocol === protocol, 'config protocol 必须为 1');
  const mode = process.argv.length === 5 ? process.argv[4] : config.mode || 'manual';
  ensure(mode === 'manual' || mode === 'native-auto', 'mode 必须为 manual 或 native-auto');
  if (config.mode !== undefined) ensure(config.mode === mode, 'CLI mode 与 config mode 不一致');
  const nativeAuto = mode === 'native-auto';
  const scenario = config.scenario || 'backend';
  ensure(scenario === 'backend' || scenario === 'ui', 'scenario 必须为 backend 或 ui');
  const uiScenario = scenario === 'ui';
  ensure(!uiScenario || nativeAuto, '真实 GPUI Auto 场景必须使用 observe-only native-auto');
  const documentHeightCss = uiScenario ? 1440 : 720;
  if (config.document_height_css !== undefined) ensure(config.document_height_css === documentHeightCss, '配置文档高度与独立场景不符');
  const maxRequests = uiScenario ? 256 : manualMaxRequests;
  const rustArgs = [uiScenario ? '--long-browser-ui-auto-smoke' : '--long-browser-smoke'];
  const scope = uiScenario ? nativeUiAutoScope : nativeAuto ? nativeAutoScope : manualScope;
  const qaRoot = fs.realpathSync(path.resolve(__dirname, '../target/qa'));
  const artifactRoot = fs.realpathSync(config.artifact_root);
  ensure(inside(artifactRoot, qaRoot) && artifactRoot !== qaRoot, 'artifact_root 必须是 target/qa 内本次独立目录');
  ensure(inside(configPath, artifactRoot), 'config 文件必须属于本次 artifact_root');
  const artifact = name => {
    ensure(/^[A-Za-z0-9_.-]+$/.test(name), '非法产物文件名');
    const result = path.join(artifactRoot, name);
    ensure(inside(result, artifactRoot), '产物路径越界');
    return result;
  };
  const writeJson = (name, value) => fs.writeFileSync(artifact(name), canonicalJson(value), { flag: 'wx' });
  const ownUrl = new URL(config.own_url);
  ensure(ownUrl.protocol === 'file:' && !ownUrl.hash, 'own_url 必须是无 fragment 的本地 file URL');
  ensure(/^[A-Za-z0-9_-]{20,128}$/.test(config.nonce), 'nonce 格式无效');
  ensure(ownUrl.searchParams.size === 1 && ownUrl.searchParams.get('nonce') === config.nonce, 'own_url nonce 不唯一');
  const ownPagePath = fs.realpathSync(fileURLToPath(ownUrl));
  ensure(inside(ownPagePath, artifactRoot), '本地测试页面必须属于本次 artifact_root');
  const endpoint = new URL(config.endpoint_url);
  ensure(['ws:', 'http:'].includes(endpoint.protocol) && ['127.0.0.1', '[::1]', 'localhost'].includes(endpoint.hostname), 'CDP endpoint 必须为 supervisor 给出的 loopback');
  ensure(!endpoint.username && !endpoint.password && !endpoint.search && !endpoint.hash && endpoint.port, 'CDP endpoint 含未授权字段');
  const owner = config.native_owner;
  ensure(owner && owner.expected_document_nonce === config.nonce && owner.title_nonce === config.nonce, '原生文档 nonce 不一致');
  ensure(Number.isSafeInteger(owner.browser_pid) && owner.browser_pid > 0 && Number.isSafeInteger(owner.hwnd) && owner.hwnd > 0, '缺少原生 PID/HWND 身份');
  ensure(typeof owner.browser_creation_filetime === 'string' && /^\d+$/.test(owner.browser_creation_filetime), '创建时间必须为十进制 string');
  ensure(typeof owner.job_name === 'string' && owner.job_name.includes(config.nonce), '缺少本次 Job 身份');
  const client = owner.client_rect;
  ensure(client && ['left', 'top', 'width', 'height'].every(key => Number.isSafeInteger(client[key])) && client.width > 0 && client.height > 0, 'client_rect 必须为物理 left/top/width/height');
  // 沿用已验证 normal-app 的 raw child 与父客户区可见交集合同；不能猜标题栏高度。
  const contentWindow = owner.content_window;
  let content = client;
  if (contentWindow !== undefined && contentWindow !== null) {
    ensure(Number.isSafeInteger(contentWindow.hwnd) && contentWindow.hwnd > 0
      && Number.isSafeInteger(contentWindow.window_pid) && contentWindow.window_pid > 0
      && Number.isSafeInteger(contentWindow.window_thread) && contentWindow.window_thread > 0
      && contentWindow.class === 'Chrome_RenderWidgetHostHWND'
      && typeof contentWindow.creation_filetime === 'string' && /^\d+$/.test(contentWindow.creation_filetime), '原生 content 子窗口身份不完整');
    const raw = contentWindow.client_rect;
    ensure(raw && ['left', 'top', 'width', 'height'].every(key => Number.isSafeInteger(raw[key]))
      && raw.width > 0 && raw.height > 0, 'content raw client 矩形无效');
    const left = Math.max(client.left, raw.left), top = Math.max(client.top, raw.top);
    const right = Math.min(client.left + client.width, raw.left + raw.width);
    const bottom = Math.min(client.top + client.height, raw.top + raw.height);
    ensure(right > left && bottom > top, 'content 未在父客户区内形成可见交集');
    content = { left, top, width: right - left, height: bottom - top };
  }
  ensure(Number.isSafeInteger(owner.dpi) && owner.dpi > 0, '原生 dpi 无效');
  const { chromium } = require(config.playwright_module);
  require.resolve(config.pngjs_module);

  let browser = null, browserCdp = null, page = null, pageCdp = null, rust = null;
  let fatal = null, expectedBrowserClose = false, browserClosed = false, finishRequested = false;
  let lease = null, leaseSequence = 0, leaseTimer = null, heartbeatBusy = false;
  let initialState = null, targetId = null, fingerprint = null, snapshotIndex = 0;
  let requests = 0, writeChain = Promise.resolve(), lastLeaseAt = null, maxLeaseGap = 0;
  let lateLeaseUpdates = 0;
  let bytesStdout = 0, bytesStderr = 0, closeRequested = false, logsClosed = false;
  let childExitObserved = null;
  let autoPhase = 'setup', autoGeneration = null, observerLastState = null;
  let cdpInputCalls = 0, rejectedInputCalls = 0, rejectedDomCalls = 0;
  const domScrollMutationCalls = 0;
  const autoPhaseAcks = [], rendererCaptures = [];
  const revocations = [], leaseMeasurements = [], lateLeaseGaps = [];
  const requestIds = new Set();
  let childResolve;
  const childDone = new Promise(resolve => { childResolve = resolve; });
  const report = {
    suite: 'long-browser-control', protocol, mode, scenario, document_height_css: documentHeightCss, scope, pid: process.pid,
    preflight_only: config.preflight_only === true,
    own_url: ownUrl.href, artifact_root: artifactRoot,
    browser_pid: owner.browser_pid, full_desktop_freeze_used: false,
    global_input_sent: false, clipboard_untouched: true,
    renderer_only_preflight: config.preflight_only === true,
    browser_close_requested: false, browser_disconnected_after_close: false,
    rust_started: false, rust_normal_exit: false, finished_protocol: false,
    ok: false,
  };
  let stdoutLog = null, stderrLog = null, protocolLog = null, rendererDropLog = null;
  let rendererDropCount = 0;
  const output = value => process.stdout.write(JSON.stringify(value) + '\n');
  const stopHeartbeat = () => { if (leaseTimer) clearInterval(leaseTimer); leaseTimer = null; };
  const send = packet => {
    const encoded = JSON.stringify(packet) + '\n';
    ensure(Buffer.byteLength(encoded) <= maxLineBytes, '发送 NDJSON 超过上限');
    writeChain = writeChain.then(() => new Promise((resolve, reject) => {
      if (!rust || rust.stdin.destroyed || rust.stdin.writableEnded) return reject(new Error('Rust stdin 已关闭'));
      if (protocolLog && !logsClosed) fs.writeSync(protocolLog, encoded);
      rust.stdin.write(encoded, error => error ? reject(error) : resolve());
    }));
    // 错误由 await 调用者和 final cleanup 记录；不产生未处理 Promise 拒绝。
    writeChain.catch(() => {});
    return writeChain;
  };
  const revoke = async reason => {
    stopHeartbeat();
    if (!lease || !lease.valid) return;
    const id = lease.lease_id;
    lease.valid = false;
    revocations.push({ lease_id: id, reason, observed_unix_ms: Date.now() });
    if (rust && !rust.stdin.writableEnded && !rust.stdin.destroyed) {
      await send({ type: 'revoke_lease', protocol, seq: ++leaseSequence, lease_id: id, valid: false, reason });
    }
  };
  const abort = reason => {
    if (fatal) return;
    fatal = String(reason);
    stopHeartbeat();
    process.stderr.write(`归属/lease 中止：${fatal}\n`);
    void (async () => {
      try {
        await revoke(fatal);
        if (rust && !rust.stdin.writableEnded && !rust.stdin.destroyed) {
          await send({ type: 'abort', protocol, reason: fatal });
          rust.stdin.end();
        }
      } catch (error) { process.stderr.write(`中止通知失败：${errorText(error)}\n`); }
    })();
  };
  const assertNotFatal = () => ensure(!fatal, fatal || '浏览器归属已失效');
  const rejectInput = label => {
    rejectedInputCalls++;
    throw new Error(`native-auto observe-only 禁止输入：${label}`);
  };
  const guardCdp = (session, kind) => {
    const sendOriginal = session.send.bind(session);
    const methods = kind === 'browser'
      ? new Set(['SystemInfo.getProcessInfo', 'Target.getTargets', 'Target.setDiscoverTargets', 'Browser.close'])
      : new Set(['Target.getTargetInfo']);
    session.send = (method, parameters) => {
      if (/^Input\./.test(method)) {
        if (nativeAuto) return rejectInput(`CDP ${method}`);
        cdpInputCalls++;
      }
      if (nativeAuto && !methods.has(method)) return rejectInput(`非只读/关闭 allowlist 的 CDP ${method}`);
      return sendOriginal(method, parameters);
    };
  };
  const guardPageInput = () => {
    if (!nativeAuto) return;
    for (const [kind, methods] of [['mouse', ['move', 'down', 'up', 'click', 'dblclick', 'wheel']],
      ['keyboard', ['down', 'up', 'insertText', 'type', 'press']], ['touchscreen', ['tap']]]) {
      for (const method of methods) page[kind][method] = () => rejectInput(`Page.${kind}.${method}`);
    }
    for (const method of ['click', 'dblclick', 'tap', 'press', 'fill', 'type', 'check', 'uncheck', 'setChecked', 'hover', 'dragAndDrop']) {
      page[method] = () => rejectInput(`Page.${method}`);
    }
    page.locator = () => rejectInput('Page.locator：可能触发滚入视口/输入');
    const evaluateOriginal = page.evaluate.bind(page);
    page.evaluate = (operation, argument) => {
      if (!Object.values(domOperations).includes(operation)) {
        rejectedDomCalls++;
        throw new Error('native-auto DOM bridge 仅允许明确的 ready/configure/scenario/measure/settled；禁止滚动修改或任意脚本');
      }
      return evaluateOriginal(operation, argument);
    };
    for (const method of ['evaluateHandle', 'addInitScript', 'addScriptTag', 'addStyleTag']) {
      page[method] = () => {
        rejectedDomCalls++;
        throw new Error(`native-auto 禁止任意 DOM/脚本修改：Page.${method}`);
      };
    }
  };
  const callDom = (operation, argument) => {
    ensure(Object.hasOwn(domOperations, operation), '不存在的 DOM bridge operation');
    return page.evaluate(domOperations[operation], argument);
  };
  const pageFingerprint = state => JSON.stringify({
    document_nonce: state.document_nonce, document_epoch: state.document_epoch,
    layout_epoch: state.layout_epoch, dpr: state.dpr, visual_viewport: state.visual_viewport,
    scrollport: state.scrollport, viewport: state.viewport,
    noise_width: state.noise_width, noise_height: state.noise_height,
    scenario: state.scenario, document_height_css: state.document_height_css,
    pattern_id: state.pattern_id, marker_rects: state.marker_rects, phase_rects: state.phase_rects,
  });
  const compareObservation = (left, right) => {
    for (const [a, b] of [[left.observed_unix_ms, right.observed_unix_ms],
      [left.input_observer.last_event_sequence, right.input_observer.last_event_sequence],
      [left.scroll_epoch, right.scroll_epoch]]) {
      if (a < b) return -1;
      if (a > b) return 1;
    }
    return 0;
  };
  const checkState = async state => {
    ensure(state && state.ready === true && state.document_nonce === config.nonce, '文档 API/nonce 不属于本调用');
    // 仅等待 Windows 进程间毫秒量化差追平，不重盖 renderer 测量时刻或放宽未来值门。
    const clockLead = state.observed_unix_ms - Date.now();
    if (clockLead > 0 && clockLead <= 2) await new Promise(resolve => setTimeout(resolve, 2));
    const receivedAt = Date.now();
    ensure(Number.isSafeInteger(state.observed_unix_ms) && state.observed_unix_ms > 0 && state.observed_unix_ms <= receivedAt && receivedAt - state.observed_unix_ms <= 500,
      `DOM measurement 不是当前 500ms 内的非未来测量：observed=${String(state.observed_unix_ms)} received=${receivedAt}`);
    ensure(state.url === ownUrl.href && state.title === `RustShot Browser QA ${config.nonce}`, '文档导航或标题身份已变化');
    ensure(typeof state.document_epoch === 'string' && state.document_epoch && Number.isSafeInteger(state.layout_epoch), '缺少文档/layout epoch');
    ensure(Number.isFinite(state.dpr) && state.dpr > 0 && Math.abs(state.dpr - owner.dpi / 96) < 1e-6, 'DOM DPR 与原生 DPI 不一致');
    const visual = state.visual_viewport;
    ensure(visual && visual.scale === 1 && visual.offset_left === 0 && visual.offset_top === 0, 'visualViewport 有缩放或平移');
    ensure(integer(state.viewport.width * state.dpr) && integer(state.viewport.height * state.dpr), 'DOM viewport 非整设备像素');
    ensure(Math.round(state.viewport.width * state.dpr) === content.width && Math.round(state.viewport.height * state.dpr) === content.height,
      `DOM viewport × DPR (${state.viewport.width}×${state.viewport.height} @${state.dpr}) 与原生可见 content (${content.width}×${content.height}) 不等`);
    ensure(visual.width === state.viewport.width && visual.height === state.viewport.height, 'visualViewport 与 inner viewport 不等');
    const rect = state.scrollport;
    ensure(rect && rect.width === 320 && rect.height === 240 && rect.left >= 0 && rect.top >= 0 && rect.left + rect.width <= state.viewport.width && rect.top + rect.height <= state.viewport.height, 'scroller 未完整处于自有 opaque viewport 内');
    ensure([rect.left, rect.top, rect.width, rect.height].every(value => integer(value * state.dpr)), 'scroller 存在设备像素小数相位');
    // 原生 Auto 的运动中间态必须保留真实小数，不能取整或修改 DOM 以凑合 oracle。
    ensure(Number.isFinite(state.scroll_top) && (nativeAuto || integer(state.scroll_top * state.dpr)), 'scrollTop 无效或手动组存在设备像素小数相位');
    ensure(state.scenario === scenario && state.document_height_css === documentHeightCss, 'DOM scene 或独立文档高度改变');
    ensure(state.scroll_left === 0 && state.client_width === 320 && state.client_height === 240 && state.scroll_height === documentHeightCss, 'DOM scrollport 尺寸/轴不符合固定合同');
    ensure(state.scroll_top >= 0 && state.scroll_top <= documentHeightCss - 240, 'scrollTop 越界');
    ensure(state.pattern_id === 'bgra-world-v1' && state.noise_width === Math.round(320 * state.dpr) && state.noise_height === Math.round(documentHeightCss * state.dpr) && state.noise_width <= 2048 && state.noise_height <= 2048, 'canvas backing/独立 pattern 合同不符');
    ensure(Array.isArray(state.text_rects) && state.text_rects.length === 0 && state.text_outside_oracle === true, '本轮文本必须在 oracle ROI 外');
    ensure(state.observer_mode === mode, '页面 observer mode 与 helper 不一致');
    const observation = state.input_observer;
    ensure(Number.isSafeInteger(state.scroll_epoch) && state.scroll_epoch >= 0
      && observation && ['wheel_events', 'scroll_events', 'last_event_sequence', 'log_dropped'].every(key => Number.isSafeInteger(observation[key]) && observation[key] >= 0)
      && observation.log_limit === 256 && Array.isArray(observation.events) && observation.events.length <= observation.log_limit,
    'DOM input observer 记录缺失或超过有界上限');
    // 迟到测量仍完整检查500ms freshness及非滚动身份/布局漂移；不能先按排序忽略它。
    if (fingerprint !== null) ensure(pageFingerprint(state) === fingerprint, 'document/layout/DPR/标记合同变化，旧 lease 永久失效');
    const newer = observerLastState === null || compareObservation(state, observerLastState) > 0;
    if (observerLastState && newer) {
      ensure(state.scroll_epoch >= observerLastState.scroll_epoch && observation.wheel_events >= observerLastState.input_observer.wheel_events
        && observation.scroll_events >= observerLastState.input_observer.scroll_events, 'DOM scroll/输入计数倒退');
    }
    // request与heartbeat可重叠；只更新最新摘要，旧合法响应不覆盖它，也不改变lease/token。
    if (newer) observerLastState = state;
    // 原样保留 renderer 测量时刻，不能把迟到的 CDP 回复重盖为 Node 接收时刻。
    return { ...state, target_id: targetId };
  };
  const checkSurface = async () => {
    assertNotFatal();
    ensure(!browserClosed && browser && browser.isConnected(), '自有浏览器已断连');
    const pages = browser.contexts().flatMap(context => context.pages());
    ensure(pages.length === 1 && pages[0] === page && !page.isClosed() && page.url() === ownUrl.href, '出现额外/替换/导航页面');
    const targets = (await browserCdp.send('Target.getTargets')).targetInfos.filter(target => target.type === 'page');
    ensure(targets.length === 1 && targets[0].targetId === targetId && targets[0].url === ownUrl.href, 'CDP page target 不再唯一');
    const state = await callDom('measure');
    return checkState(state);
  };
  const settled = async (previous, movement) => {
    assertNotFatal();
    const state = await callDom('settled', { previous, movement });
    await checkState(state);
    return checkSurface();
  };
  const decodePng = pngPath => new Promise((resolve, reject) => {
    const worker = new Worker(__filename, { workerData: { pngjs_module: config.pngjs_module, png_path: pngPath } });
    let completed = false;
    worker.once('message', result => {
      completed = true;
      result.error ? reject(new Error(result.error)) : resolve(result);
    });
    worker.once('error', reject);
    worker.once('exit', code => { if (!completed) reject(new Error(`PNG 解码线程 exit=${code}`)); });
  });
  const snapshot = async () => {
    ensure(!nativeAuto || autoPhase === 'setup' || autoPhase === 'paused', 'native-auto 运行期禁止 renderer screenshot');
    const before = await settled(null, false);
    ensure(integer(before.scroll_top * before.dpr), '暂停/校准 scrollTop 尚未落在整设备像素；不能修改或取整');
    const pngPath = artifact(`renderer-${String(++snapshotIndex).padStart(3, '0')}.png`);
    // Auto 使用固定的已校准 CSS ROI clip；locator.screenshot 可能 scrollIntoView，不能用于只观察组。
    const rect = before.scrollport;
    const pngBytes = nativeAuto
      ? await page.screenshot({ clip: { x: rect.left, y: rect.top, width: rect.width, height: rect.height }, scale: 'device',
        animations: 'allow', caret: 'initial', omitBackground: false, timeout: 5000 })
      : await page.locator('#scroller').screenshot({ scale: 'device', animations: 'disabled', caret: 'hide', omitBackground: false, timeout: 5000 });
    ensure(pngBytes.length > 0 && pngBytes.length <= 8 * 1024 * 1024, 'renderer PNG 超过有界上限');
    fs.writeFileSync(pngPath, pngBytes, { flag: 'wx' });
    const after = await checkSurface();
    ensure(before.scroll_top === after.scroll_top && before.scroll_left === after.scroll_left && before.scroll_epoch === after.scroll_epoch,
      'renderer 捕获期间滚动改变，不能作为 settled oracle');
    if (nativeAuto) rendererCaptures.push({ phase: autoPhase, generation: autoGeneration, clip: { x: rect.left, y: rect.top, width: rect.width, height: rect.height },
      before_scroll_top: before.scroll_top, after_scroll_top: after.scroll_top, before_scroll_left: before.scroll_left, after_scroll_left: after.scroll_left,
      before_scroll_epoch: before.scroll_epoch, after_scroll_epoch: after.scroll_epoch, no_scroll_into_view: true });
    const decoded = await decodePng(pngPath);
    ensure(decoded.width === Math.round(320 * after.dpr) && decoded.height === Math.round(240 * after.dpr) && decoded.transparent === 0, 'renderer PNG 并非 exact opaque scroller 设备尺寸');
    return {
      state: after,
      renderer: { path: fs.realpathSync(pngPath), sha256: crypto.createHash('sha256').update(pngBytes).digest('hex'), pixel_width: decoded.width, pixel_height: decoded.height },
    };
  };
  const sendLease = async install => {
    const currentLease = lease;
    ensure(currentLease && currentLease.valid, '只能更新仍有效的显式 lease');
    let state;
    try { state = await checkSurface(); }
    catch (error) {
      if (lease !== currentLease || !currentLease.valid || browserClosed || expectedBrowserClose) return;
      throw error;
    }
    // revoke/new_lease 可与 DOM measurement 重叠；晚到的旧测量不能复活旧许可。
    if (lease !== currentLease || !currentLease.valid || browserClosed) return;
    const now = state.observed_unix_ms;
    if (lastLeaseAt !== null) {
      const gap = now - lastLeaseAt;
      maxLeaseGap = Math.max(maxLeaseGap, gap);
      if (gap > 100) {
        lateLeaseUpdates++;
        if (lateLeaseGaps.length < 64) lateLeaseGaps.push({ lease_id: currentLease.lease_id, gap_ms: gap, observed_unix_ms: now });
      }
      // 75ms 是调度目标；100ms 记录迟到。硬新鲜期与原生 owner 保持500ms一致。
      ensure(gap <= 500, `live lease 更新间隔 ${gap}ms 超过 500ms，不能继续采集`);
    }
    lastLeaseAt = now;
    leaseMeasurements.push({ seq: leaseSequence + 1, observed_unix_ms: now, scroll_top: state.scroll_top, scroll_left: state.scroll_left,
      scroll_epoch: state.scroll_epoch, layout_epoch: state.layout_epoch,
      wheel_events: state.input_observer.wheel_events, scroll_events: state.input_observer.scroll_events });
    ensure(leaseMeasurements.length <= 4096, 'lease 记录超过有界上限');
    await send({ type: install ? 'new_lease' : 'lease', protocol, seq: ++leaseSequence, valid: true,
      lease_id: currentLease.lease_id, document_nonce: state.document_nonce, target_id: targetId,
      layout_epoch: state.layout_epoch, dpr: state.dpr,
      visual_viewport: state.visual_viewport, scrollport: state.scrollport,
      observed_unix_ms: now });
  };
  const newLease = async () => {
    assertNotFatal();
    ensure(!browserClosed, 'Browser.close 后不得创建新 lease');
    await revoke('显式安装新的采集 lease');
    lease = { lease_id: crypto.randomUUID(), valid: true };
    lastLeaseAt = null;
    await sendLease(true);
    leaseTimer = setInterval(() => {
      if (heartbeatBusy || !lease || !lease.valid || browserClosed || fatal) return;
      heartbeatBusy = true;
      sendLease(false).catch(error => abort(errorText(error))).finally(() => { heartbeatBusy = false; });
    }, heartbeatMs);
    return lease.lease_id;
  };
  const closeBrowser = async () => {
    if (browserClosed) return;
    await revoke('显式 Browser.close：旧 HWND/target lease 不可复活');
    expectedBrowserClose = true;
    closeRequested = true;
    report.browser_close_requested = true;
    // 唯一 browser CDP 会话仅来自本次 supervisor 的 PID/port 绑定。
    let resolveDisconnect;
    const disconnected = new Promise(resolve => { resolveDisconnect = resolve; });
    if (!browser.isConnected()) resolveDisconnect();
    else browser.once('disconnected', resolveDisconnect);
    try { await browserCdp.send('Browser.close'); }
    catch (error) { if (browser.isConnected()) throw error; }
    await bounded(disconnected, 10000, 'Browser.close 后 CDP 未正常断连');
    browserClosed = true;
    report.browser_disconnected_after_close = true;
  };

  try {
    browser = await chromium.connectOverCDP(endpoint.href, { timeout: 15000 });
    browserCdp = await browser.newBrowserCDPSession();
    guardCdp(browserCdp, 'browser');
    const processes = (await browserCdp.send('SystemInfo.getProcessInfo')).processInfo;
    const browserProcesses = processes.filter(value => value.type === 'browser');
    ensure(browserProcesses.length === 1 && browserProcesses[0].id === owner.browser_pid, 'CDP browser PID 与 supervisor 原生身份不等');
    const pages = browser.contexts().flatMap(context => context.pages());
    ensure(pages.length === 1, '自有 Chrome 必须只有唯一 page，不能选择其中之一继续');
    page = pages[0];
    guardPageInput();
    ensure(page.url() === ownUrl.href, '唯一 page URL 不属于本次文档');
    pageCdp = await page.context().newCDPSession(page);
    guardCdp(pageCdp, 'page');
    const target = (await pageCdp.send('Target.getTargetInfo')).targetInfo;
    targetId = target.targetId;
    ensure(target.type === 'page' && target.url === ownUrl.href && targetId, '页面 target 身份无效');
    if (owner.expected_target_id) ensure(owner.expected_target_id === targetId, '预定 target ID 已变化');
    const apiReady = await callDom('ready');
    ensure(apiReady, '自有页面 API 未初始化（检查 pageerror/CSP/DPR）');
    await callDom('configure', mode);
    await callDom('scenario', { scenario, document_height_css: documentHeightCss });
    browser.on('disconnected', () => { if (!expectedBrowserClose) abort('CDP 浏览器非预期断连'); });
    for (const context of browser.contexts()) context.on('page', extra => { if (extra !== page) abort('出现额外 page'); });
    page.on('crash', () => { if (!expectedBrowserClose) abort('自有 renderer crash'); });
    page.on('close', () => { if (!expectedBrowserClose) abort('自有 page 非预期关闭'); });
    page.on('framenavigated', () => { if (!expectedBrowserClose) abort('连接后的 frame 导航禁止复用采集 lease'); });
    page.on('download', () => abort('自有页面出现非预期下载'));
    page.on('pageerror', error => { if (!expectedBrowserClose) abort(`页面脚本错误：${errorText(error)}`); });
    page.on('request', request => { if (!expectedBrowserClose && request.url() !== ownUrl.href) abort(`页面出现非本地文档资源请求：${request.resourceType()}`); });
    await browserCdp.send('Target.setDiscoverTargets', { discover: true });
    browserCdp.on('Target.targetCreated', event => { if (event.targetInfo.type === 'page' && event.targetInfo.targetId !== targetId) abort('CDP 出现额外 page target'); });
    browserCdp.on('Target.targetDestroyed', event => { if (event.targetId === targetId && !expectedBrowserClose) abort('自有 target 被销毁'); });
    browserCdp.on('Target.targetInfoChanged', event => {
      if (!expectedBrowserClose && event.targetInfo.type === 'page' && (event.targetInfo.targetId !== targetId || event.targetInfo.url !== ownUrl.href)) abort('CDP page target URL/身份改变');
    });
    initialState = await checkSurface();
    fingerprint = pageFingerprint(initialState);
    const initial = await snapshot();
    const resolvedConfig = { ...config, mode, scenario, document_height_css: documentHeightCss, max_requests: maxRequests,
      artifact_root: artifactRoot, own_url: ownUrl.href,
      native_owner: { ...owner, expected_target_id: targetId },
      page_state: initial.state, initial_renderer: initial.renderer,
      scope, heartbeat_interval_ms: heartbeatMs, late_lease_gap_ms: 100, max_lease_gap_ms: 500 };
    const resolvedPath = artifact('node-resolved-config.json');
    fs.writeFileSync(resolvedPath, canonicalJson(resolvedConfig), { flag: 'wx' });
    const ready = { type: 'ready', protocol, pid: process.pid, browser_pid: owner.browser_pid,
      target_id: targetId, document_nonce: config.nonce, state: initial.state, renderer: initial.renderer,
      native_client_exact: contentWindow === undefined || contentWindow === null,
      native_content_exact: true, native_visible_content_rect: content,
      unique_page_count: 1, config_path: resolvedPath, mode, scenario, document_height_css: documentHeightCss, scope };
    writeJson('node-ready.json', ready);
    output(ready);

    if (config.preflight_only === true) {
      await closeBrowser();
      report.ok = true;
      report.state = initial.state;
      report.renderer = initial.renderer;
      report.native_client_exact = contentWindow === undefined || contentWindow === null;
      report.native_content_exact = true;
      report.native_visible_content_rect = content;
      report.unique_page_count = 1;
      report.rust_started = false;
    } else {
      // 仅是本次启动握手；它不能替代之后每帧的原生归属/遮挡门。
      const placementAckPath = artifact('supervisor-placement-ack.json');
      const placementDeadline = Date.now() + 10000;
      while (!fs.existsSync(placementAckPath)) {
        ensure(Date.now() < placementDeadline, 'supervisor 自有 Chrome placement ACK 10秒超时');
        await new Promise(resolve => setTimeout(resolve, 100));
      }
      const placementPath = fs.realpathSync(placementAckPath);
      ensure(inside(placementPath, artifactRoot) && fs.statSync(placementPath).mtimeMs >= fs.statSync(artifact('node-ready.json')).mtimeMs, 'placement ACK 不属于本次 ready 后的产物');
      const placement = JSON.parse(fs.readFileSync(placementPath, 'utf8').replace(/^\uFEFF/, ''));
      ensure(placement.ok === true && placement.nonce === config.nonce && placement.browser_pid === owner.browser_pid && placement.hwnd === owner.hwnd
        && placement.native_client_rect && ['left', 'top', 'width', 'height'].every(key => placement.native_client_rect[key] === client[key]), 'placement ACK 原生身份/客户区不一致');
      await checkSurface();
      report.supervisor_placement_ack = placementPath;
      const rustExe = fs.realpathSync(config.rust_exe);
      ensure(path.isAbsolute(rustExe) && path.basename(rustExe).toLowerCase() === 'raishot-gpui.exe', 'rust_exe 必须为 final raishot-gpui.exe');
      stdoutLog = fs.openSync(artifact('rust-stdout.jsonl'), 'wx');
      stderrLog = fs.openSync(artifact('rust-stderr.log'), 'wx');
      protocolLog = fs.openSync(artifact('node-to-rust.jsonl'), 'wx');
      if (uiScenario) rendererDropLog = fs.openSync(artifact('gpui-renderer-drops.jsonl'), 'wx');
      rust = spawn(rustExe, rustArgs, { windowsHide: true, cwd: path.resolve(__dirname, '..'),
        stdio: ['pipe', 'pipe', 'pipe'], env: { ...process.env,
          RUSTSHOT_LONG_BROWSER_CONFIG: resolvedPath, RUSTSHOT_LONG_BROWSER_QA_DIR: artifactRoot,
          RUSTSHOT_BROWSER_QA_MODE: mode,
          RUSTSHOT_BROWSER_QA_SCENARIO: scenario,
          ...(uiScenario ? { RUSTSHOT_GPUI_LIFECYCLE_TRACE: '1' } : {}),
          RUSTSHOT_QA_NO_RESIDENT_NOTIFY: '1' } });
      report.rust_started = true;
      ensure(Number.isSafeInteger(rust.pid) && rust.pid > 0, '无法启动自有 Rust QA 子进程');
      writeJson('rust-process.json', { pid: rust.pid, parent_pid: process.pid, exe: rustExe, args: rustArgs, scenario });
      let spawnError = null;
      rust.on('error', error => { spawnError = errorText(error); abort(`Rust 子进程错误：${spawnError}`); });
      // close 在 stdio 已排空之后触发；不能仅等 exit 就关闭日志 fd。
      rust.on('close', (code, signal) => {
        childExitObserved = { code, signal, error: spawnError };
        childResolve(childExitObserved);
      });
      rust.stdin.on('error', error => { if (!finishRequested) abort(`Rust stdin 错误：${errorText(error)}`); });
      rust.stderr.on('data', bytes => {
        bytesStderr += bytes.length;
        if (bytesStderr > maxLogBytes) abort('Rust stderr 超过有界日志预算');
        else if (!logsClosed) fs.writeSync(stderrLog, bytes);
      });
      rust.stdout.on('data', bytes => {
        bytesStdout += bytes.length;
        if (bytesStdout > maxLogBytes) abort('Rust stdout 超过有界日志预算');
        else if (!logsClosed) fs.writeSync(stdoutLog, bytes);
      });
      const lines = readline.createInterface({ input: rust.stdout, crlfDelay: Infinity });
      let requestChain = Promise.resolve();
      lines.on('line', line => {
        requestChain = requestChain.then(async () => {
          ensure(Buffer.byteLength(line) <= maxLineBytes, 'Rust request NDJSON 超过上限');
          const request = JSON.parse(line);
          if (uiScenario && request.phase === 'gpui_renderer_drop') {
            ensure(Object.keys(request).length === 3 && Object.hasOwn(request, 'hwnd') && Object.hasOwn(request, 'unix_ms')
              && Number.isSafeInteger(request.hwnd) && request.hwnd > 0 && Number.isSafeInteger(request.unix_ms) && request.unix_ms > 0,
            'GPUI renderer Drop JSON 身份字段不完整');
            ensure(rendererDropLog !== null && rendererDropCount < 16, 'GPUI renderer Drop 日志超过16条上限');
            fs.writeSync(rendererDropLog, line + '\n');
            rendererDropCount++;
            return;
          }
          ensure(request.type === 'request' && request.protocol === protocol && (typeof request.id === 'string' || Number.isSafeInteger(request.id)), 'Rust stdout 必须为有 ID 的 protocol request');
          const idKey = `${typeof request.id}:${request.id}`;
          ensure(!requestIds.has(idKey) && ++requests <= maxRequests, 'Rust request ID 重复或请求超过上限');
          requestIds.add(idKey);
          let payload;
          try {
            if (request.op === 'finish') {
              ensure(browserClosed, 'finish 前必须正常 close_browser');
              stopHeartbeat();
              finishRequested = true;
              await send({ type: 'response', protocol, id: request.id, ok: true, finished: true });
              rust.stdin.end();
              report.finished_protocol = true;
              return;
            }
            assertNotFatal();
            switch (request.op) {
              case 'calibrate':
                ensure(!nativeAuto || autoPhase === 'setup', 'native-auto 首次 active 后禁止重新校准截图');
                payload = await snapshot(); break;
              case 'observe': payload = nativeAuto ? { state: await checkSurface() } : await snapshot(); break;
              case 'native_auto_phase': {
                ensure(nativeAuto && Number.isSafeInteger(request.generation) && request.generation > 0, 'native_auto_phase 需要 Auto 模式与真实 generation');
                ensure(autoGeneration === null || request.generation >= autoGeneration, 'native Auto generation 倒退');
                if (request.phase === 'active') {
                  ensure(autoPhase === 'setup' || autoPhase === 'paused', 'Auto active ACK 重复或缺少上一轮暂停');
                  autoPhase = 'active';
                } else if (request.phase === 'paused') {
                  ensure(autoPhase === 'active' && request.producer_quiet === true && request.consumer_quiet === true
                    && request.raw_slots === 0 && request.queue_slots === 0, 'renderer 对照前必须有生产 Pause/drain 的真实 quiet ACK');
                  autoPhase = 'paused';
                } else throw new Error('未知 native Auto phase');
                autoGeneration = request.generation;
                const ack = { phase: autoPhase, generation: autoGeneration, producer_quiet: request.producer_quiet === true,
                  consumer_quiet: request.consumer_quiet === true, raw_slots: request.raw_slots, queue_slots: request.queue_slots,
                  observed_unix_ms: Date.now(), source: 'Rust production Pause/drain ACK；Node 不独立推断工作线程状态' };
                ensure(autoPhaseAcks.length < 16, 'native Auto phase ACK 超过有界上限');
                autoPhaseAcks.push(ack);
                payload = { native_auto_phase: ack, state: await checkSurface() };
                break;
              }
              case 'observe_paused':
                ensure(nativeAuto && autoPhase === 'paused' && request.generation === autoGeneration, 'observe_paused 缺少同代暂停 quiet ACK');
                payload = await snapshot(); break;
              case 'wheel': {
                if (nativeAuto) rejectInput('protocol wheel');
                const before = await checkSurface();
                const delta = request.delta_y === undefined ? 32 : request.delta_y;
                ensure(Number.isFinite(delta) && delta !== 0 && Math.abs(delta) <= 120, 'wheel delta_y 超出本轮手动 CSS 范围');
                const rect = before.scrollport;
                cdpInputCalls++;
                await page.mouse.move(rect.left + rect.width / 2, rect.top + rect.height / 2);
                cdpInputCalls++;
                await page.mouse.wheel(0, delta);
                const actual = await settled(before.scroll_top, true);
                ensure(Math.sign(actual.scroll_top - before.scroll_top) === Math.sign(delta), 'actual DOM scrollTop 与 wheel 方向不一致');
                payload = await snapshot();
                payload.wheel = { delta_y: delta, before_scroll_top: before.scroll_top,
                  after_scroll_top: payload.state.scroll_top, actual_delta_css: payload.state.scroll_top - before.scroll_top,
                  settled_by_actual_scroll_and_two_raf: true };
                break;
              }
              case 'revoke_lease':
                await revoke('Rust 请求永久撤销当前 lease');
                payload = { lease_id: lease ? lease.lease_id : null, lease_revoked: true, state: await checkSurface() };
                break;
              case 'new_lease':
                payload = { lease_id: await newLease(), state: await checkSurface() };
                break;
              case 'close_browser':
                await closeBrowser(); payload = { browser_closed: true }; break;
              default: throw new Error(`不支持的 op：${String(request.op)}`);
            }
            await send({ type: 'response', protocol, id: request.id, ok: true, ...payload });
          } catch (error) {
            await send({ type: 'response', protocol, id: request.id, ok: false, error: errorText(error) });
            throw error;
          }
        }).catch(error => abort(errorText(error)));
      });
      await send({ type: 'config', protocol, config: resolvedConfig });
      await newLease();
      const childExit = await bounded(childDone, 150000, 'Rust protocol 150 秒超时');
      stopHeartbeat();
      await requestChain;
      await writeChain;
      report.rust_exit_code = childExit.code;
      report.rust_exit_signal = childExit.signal;
      report.rust_normal_exit = childExit.code === 0 && childExit.signal === null;
      let nativeReport = null;
      if (fs.existsSync(artifact('report.json'))) {
        const nativeReportPath = fs.realpathSync(artifact('report.json'));
        ensure(inside(nativeReportPath, artifactRoot), 'Rust report 路径越过本次 artifact_root');
        const nativeBytes = fs.readFileSync(nativeReportPath);
        nativeReport = JSON.parse(nativeBytes.toString('utf8'));
        report.native_report = nativeReportPath;
        report.native_report_sha256 = crypto.createHash('sha256').update(nativeBytes).digest('hex');
        report.native_groups = nativeReport.groups;
        report.native_limitations = nativeReport.limitations;
        report.native_error = nativeReport.error;
      }
      ensure(report.rust_normal_exit, `Rust 未正常退出：${JSON.stringify(childExit)}；native error=${String(nativeReport && nativeReport.error || '')}`);
      assertNotFatal();
      ensure(finishRequested && browserClosed, 'Rust 未通过 finish EOF 和正常 Browser.close 完成');
      ensure(nativeReport !== null, 'Rust 正常退出却没有本次 native report.json');
      ensure(nativeReport.suite === 'long-browser' && nativeReport.pid === rust.pid && nativeReport.ok === true, `Rust native report 失败/身份不符：${String(nativeReport.error || '')}`);
      const expectedGroup = uiScenario ? 'browser-ui-native-auto' : nativeAuto ? 'browser-native-auto' : 'browser-roi-renderer-calibration';
      ensure(Array.isArray(nativeReport.groups) && nativeReport.groups.length === 1 && nativeReport.groups[0].name === expectedGroup && nativeReport.groups[0].ok === true, 'Rust native 实际验收组不符');
      if (nativeAuto) ensure(cdpInputCalls === 0 && domScrollMutationCalls === 0 && rejectedInputCalls === 0 && rejectedDomCalls === 0,
        'native-auto 发生了禁止的输入或 DOM scroll 修改');
      ensure(nativeReport.protocol_thread_joined === true && nativeReport.capture_threads_after_close === 0 && nativeReport.consumer_sessions_after_close === 0, 'Rust native 未正常 join reader/生产者/消费者');
      ensure(nativeReport.real_browser_compositor === true && nativeReport.real_dxgi_capture === true && nativeReport.fixture_substituted === false, 'Rust native 不能以合成夹具或仅renderer代替真实DXGI');
      ensure(nativeReport.full_desktop_freeze_used === false && nativeReport.global_input_sent === false && nativeReport.clipboard_untouched === true, 'Rust native 归属/输入边界不符');
      report.ok = true;
      report.target_id = targetId;
      report.rust_pid = rust.pid;
      report.requests = requests;
    }
  } catch (error) {
    report.error = errorText(error);
    report.ok = false;
    process.stderr.write(report.error + '\n');
    try { await revoke(`Node 失败：${report.error}`); } catch (_) { /* 原始失败保留于 report。 */ }
    if (rust && !rust.stdin.writableEnded && !rust.stdin.destroyed) {
      try { await send({ type: 'abort', protocol, reason: report.error }); } catch (_) { /* 管道已闭。 */ }
      rust.stdin.end();
    }
    if (browser && browser.isConnected() && !closeRequested) {
      try { await closeBrowser(); } catch (closeError) { report.close_error = errorText(closeError); }
    }
    if (rust && childExitObserved === null) {
      try { childExitObserved = await bounded(childDone, 8000, '失败通知/EOF 后 Rust 未自行退出，需 supervisor 自有句柄清理'); }
      catch (exitError) { report.child_cleanup_error = errorText(exitError); }
    }
    if (childExitObserved) {
      report.rust_exit_code = childExitObserved.code;
      report.rust_exit_signal = childExitObserved.signal;
      report.rust_normal_exit = childExitObserved.code === 0 && childExitObserved.signal === null;
    }
    process.exitCode = 1;
  } finally {
    stopHeartbeat();
    report.lease_revocations = revocations;
    report.lease_count = leaseMeasurements.length;
    report.max_live_lease_gap_ms = maxLeaseGap;
    report.heartbeat_target_interval_ms = heartbeatMs;
    report.late_live_lease_updates = lateLeaseUpdates;
    report.late_live_lease_gaps = lateLeaseGaps;
    report.lease_freshness_limit_ms = 500;
    report.renderer_snapshots = snapshotIndex;
    if (uiScenario) {
      report.gpui_lifecycle_trace_enabled_for_owned_child = true;
      report.gpui_renderer_drop_count = rendererDropCount;
      report.gpui_renderer_drop_limit = 16;
      report.gpui_renderer_drop_scope = '仅本次Rust子进程stdout的精确析构事件；不查询或承诺COM/atlas同步释放';
    }
    report.request_limit = maxRequests;
    report.cdp_input_calls = cdpInputCalls;
    report.dom_scroll_mutation_calls = domScrollMutationCalls;
    report.rejected_input_calls = rejectedInputCalls;
    report.rejected_dom_calls = rejectedDomCalls;
    report.dom_scroll_mutation_guard = nativeAuto ? 'page.evaluate 函数身份白名单；所有任意evaluateHandle/script/style与DOM滚动命令均无可调用路径' : 'manual仅Page.mouse.move/wheel；无DOM滚动修改路径';
    report.native_auto_phase_acks = autoPhaseAcks;
    report.renderer_capture_observations = rendererCaptures;
    report.last_dom_observation = observerLastState;
    report.fatal_guard_reason = fatal;
    logsClosed = true;
    if (stdoutLog !== null) fs.closeSync(stdoutLog);
    if (stderrLog !== null) fs.closeSync(stderrLog);
    if (protocolLog !== null) fs.closeSync(protocolLog);
    if (rendererDropLog !== null) {
      fs.closeSync(rendererDropLog);
      const dropPath = artifact('gpui-renderer-drops.jsonl');
      const dropBytes = fs.readFileSync(dropPath);
      report.gpui_renderer_drop_log = { path: fs.realpathSync(dropPath), sha256: crypto.createHash('sha256').update(dropBytes).digest('hex'), bytes: dropBytes.length };
    }
    writeJson('lease-measurements.json', leaseMeasurements);
    writeJson('node-result.json', report);
    output({ type: 'result', protocol, ...report });
  }
}

if (isMainThread) {
  main().catch(error => {
    process.stderr.write(`${errorText(error)}\n`);
    process.exitCode = 1;
  });
}
