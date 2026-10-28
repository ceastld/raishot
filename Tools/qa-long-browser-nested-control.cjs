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
const maxProtocolLogBytes = 128 * 1024 * 1024;
const maxRequests = 2048;
const maxRendererSnapshots = 18;
const phaseTolerance = 0.003;
const maxRendererDrops = 32;
const scope = '自有Chrome400×300物理ROI/1800行有限嵌套页面；每冷case使用真实光标或中心回退、完整GPUI原生Auto/DXGI/Edit/Return，Node仅观察；非小数相位/sticky动态页面/任意网站/混合DPI/长期零泄漏验收';
const errorText = error => String(error && (error.stack || error.message) || error);
const ensure = (condition, message) => { if (!condition) throw new Error(message); };
const integer = value => Number.isFinite(value) && Math.abs(value - Math.round(value)) < 1e-6;
const inside = (candidate, root) => candidate === root || candidate.startsWith(root + path.sep);
const canonicalJson = value => JSON.stringify(value, null, 2) + '\n';
async function bounded(promise, milliseconds, message) {
  let timer;
  try {
    return await Promise.race([promise, new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(message)), milliseconds);
    })]);
  } finally { clearTimeout(timer); }
}

async function main() {
  ensure(process.argv.length === 3, '用法：node qa-long-browser-nested-control.cjs <config.json>');
  const configPath = fs.realpathSync(process.argv[2]);
  const config = JSON.parse(fs.readFileSync(configPath, 'utf8').replace(/^\uFEFF/, ''));
  ensure(config.protocol === protocol, 'config protocol 必须为 1');
  const mode = 'native-auto', scenario = 'nested', documentHeightCss = 1440;
  ensure(config.mode === mode && config.scenario === scenario && config.document_height_css === documentHeightCss, '必须是独立 native-auto/nested 1440CSS行场景');
  const nestedCase = config.nested_case;
  ensure(nestedCase === 1 || nestedCase === 2, 'nested_case必须为1或2');
  const cursor = config.cursor_screen_point;
  ensure(cursor && ['x', 'y'].every(key => Number.isSafeInteger(cursor[key]) && cursor[key] >= -2147483648 && cursor[key] <= 2147483647), '缺少真实原鼠标screen point');
  const innerLeftCss = nestedCase === 1 ? 168 : 96, innerWidthCss = 320 - innerLeftCss;
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
  ensure(ownUrl.searchParams.size === 2 && ownUrl.searchParams.get('nonce') === config.nonce && ownUrl.searchParams.get('case') === String(nestedCase), 'own_url nonce 不唯一');
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
  // 网页渲染子窗口可能带有原生裁剪，冻结原始矩形和父客户区的精确交集。
  // 标题栏不属于DOM viewport；不猜标题高度，也不容忍活跃期几何变化。
  const contentWindow = owner.content_window;
  let content = client;
  if (contentWindow !== undefined && contentWindow !== null) {
    ensure(Number.isSafeInteger(contentWindow.hwnd) && contentWindow.hwnd > 0
      && Number.isSafeInteger(contentWindow.window_pid) && contentWindow.window_pid > 0
      && Number.isSafeInteger(contentWindow.window_thread) && contentWindow.window_thread > 0
      && contentWindow.class === 'Chrome_RenderWidgetHostHWND'
      && typeof contentWindow.creation_filetime === 'string' && /^\d+$/.test(contentWindow.creation_filetime), '原生content子窗口身份不完整');
    const raw = contentWindow.client_rect;
    ensure(raw && ['left', 'top', 'width', 'height'].every(key => Number.isSafeInteger(raw[key]))
      && raw.width > 0 && raw.height > 0, 'content raw client矩形无效');
    const left = Math.max(client.left, raw.left), top = Math.max(client.top, raw.top);
    const right = Math.min(client.left + client.width, raw.left + raw.width);
    const bottom = Math.min(client.top + client.height, raw.top + raw.height);
    ensure(right > left && bottom > top, 'content未在父客户区内形成可见交集');
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
  let lateLeaseUpdates = 0, staleLeaseMeasurements = 0, maxPhysicalPhaseError = 0;
  let bytesStdout = 0, bytesStderr = 0, bytesProtocol = 0, closeRequested = false, logsClosed = false;
  let currentCycle = 1, leasedCycle = 0, totalLeaseMeasurements = 0;
  let childExitObserved = null, autoPhase = 'setup', autoGeneration = null, observerLastState = null;
  let cdpInputCalls = 0, rejectedInputCalls = 0, rejectedDomCalls = 0, fixturePrepareCalls = 0;
  const domScrollMutationCalls = 0;
  const autoPhaseAcks = [], rendererCaptures = [], fixturePreparations = [], leaseInstallations = [];
  const revocations = [], leaseMeasurements = [], lateLeaseGaps = [];
  const requestIds = new Set();
  let childResolve;
  const childDone = new Promise(resolve => { childResolve = resolve; });
  const report = {
    suite: 'long-browser-nested-control', protocol, mode, scenario, document_height_css: documentHeightCss,
    physical_spec: { width: 400, viewport_height: 300, document_height: 1800, tile_height: 300 }, scope, pid: process.pid, nested_case: nestedCase, cursor_screen_point: cursor,
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
      if (protocolLog && !logsClosed) {
        bytesProtocol += Buffer.byteLength(encoded);
        if (bytesProtocol > maxProtocolLogBytes) return reject(new Error('Node→Rust NDJSON 超过128MiB有界预算'));
        fs.writeSync(protocolLog, encoded);
      }
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
  const domOperations = Object.freeze({
    ready: () => Boolean(window.__rustshotLongBrowserNestedQa),
    measure: () => window.__rustshotLongBrowserNestedQa.measure(),
    settled: options => window.__rustshotLongBrowserNestedQa.settled(options.previous, options.movement),
    hit_test: point => window.__rustshotLongBrowserNestedQa.hitTest(point),
  });
  const rejectInput = label => {
    rejectedInputCalls++;
    throw new Error(`nested observe-only 禁止输入：${label}`);
  };
  const guardCdp = (session, kind) => {
    const original = session.send.bind(session);
    const methods = kind === 'browser'
      ? new Set(['SystemInfo.getProcessInfo', 'Target.getTargets', 'Target.setDiscoverTargets', 'Browser.close'])
      : new Set(['Target.getTargetInfo']);
    session.send = (method, parameters) => {
      if (/^Input\./.test(method) || !methods.has(method)) return rejectInput(`CDP ${method}`);
      return original(method, parameters);
    };
  };
  const guardPageInput = () => {
    for (const [kind, methods] of [['mouse', ['move', 'down', 'up', 'click', 'dblclick', 'wheel']],
      ['keyboard', ['down', 'up', 'insertText', 'type', 'press']], ['touchscreen', ['tap']]]) {
      for (const method of methods) page[kind][method] = () => rejectInput(`Page.${kind}.${method}`);
    }
    for (const method of ['click', 'dblclick', 'tap', 'press', 'fill', 'type', 'check', 'uncheck', 'setChecked', 'hover', 'dragAndDrop']) {
      page[method] = () => rejectInput(`Page.${method}`);
    }
    page.locator = () => rejectInput('Page.locator可能滚入视口或注入输入');
    const original = page.evaluate.bind(page);
    page.evaluate = (operation, argument) => {
      if (!Object.values(domOperations).includes(operation)) {
        rejectedDomCalls++;
        throw new Error('nested DOM bridge只允许明确的ready/measure/settled/hitTest只读操作');
      }
      return original(operation, argument);
    };
    for (const method of ['evaluateHandle', 'addInitScript', 'addScriptTag', 'addStyleTag']) {
      page[method] = () => {
        rejectedDomCalls++;
        throw new Error(`nested禁止任意DOM/脚本修改：Page.${method}`);
      };
    }
  };
  const callDom = (operation, argument) => {
    ensure(Object.hasOwn(domOperations, operation), '不存在的DOM bridge operation');
    return page.evaluate(domOperations[operation], argument);
  };
  const pageFingerprint = state => JSON.stringify({
    document_nonce: state.document_nonce, document_epoch: state.document_epoch,
    layout_epoch: state.layout_epoch, dpr: state.dpr, visual_viewport: state.visual_viewport,
    scrollport: state.scrollport, viewport: state.viewport,
    nested_case: state.nested_case, inner_rect: state.nested.inner_rect, inner_left_css: state.nested.inner_left_css, inner_width_css: state.nested.inner_width_css,
    physical_spec: state.physical_spec, pattern_id: state.pattern_id,
    observer_mode: state.observer_mode, scenario: state.scenario, document_height_css: state.document_height_css,
    pool_slot_count: state.pool.slot_count, backing_bytes: state.pool.backing_bytes, scratch_bytes: state.pool.scratch_bytes,
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
    // Windows 各进程 Date.now 的毫秒量化偶尔相差 1ms。仅等 Node 时钟追上，
    // 不重盖 renderer 时刻；最终仍拒绝未来值及超过 500ms 的旧测量。
    const clockLead = state.observed_unix_ms - Date.now();
    if (clockLead > 0 && clockLead <= 2) await new Promise(resolve => setTimeout(resolve, 2));
    const receivedAt = Date.now();
    ensure(Number.isSafeInteger(state.observed_unix_ms) && state.observed_unix_ms > 0 && state.observed_unix_ms <= receivedAt && receivedAt - state.observed_unix_ms <= 500,
      `DOM measurement 不是当前 500ms 内的非未来测量：observed=${String(state.observed_unix_ms)} received=${receivedAt}`);
    ensure(state.url === ownUrl.href && state.title === `RustShot Browser Nested QA ${config.nonce}`, '文档导航或标题身份已变化');
    ensure(typeof state.document_epoch === 'string' && state.document_epoch && Number.isSafeInteger(state.layout_epoch), '缺少文档/layout epoch');
    ensure(state.dpr === 1.25 && Math.abs(state.dpr - owner.dpi / 96) < 1e-6, 'stress仅支持已验证DPR1.25，且必须与原生DPI一致');
    const visual = state.visual_viewport;
    ensure(visual && visual.scale === 1 && visual.offset_left === 0 && visual.offset_top === 0, 'visualViewport 有缩放或平移');
    ensure(integer(state.viewport.width * state.dpr) && integer(state.viewport.height * state.dpr), 'DOM viewport 非整设备像素');
    ensure(Math.round(state.viewport.width * state.dpr) === content.width && Math.round(state.viewport.height * state.dpr) === content.height, `DOM viewport × DPR (${state.viewport.width}×${state.viewport.height} @${state.dpr}) 与原生可见content (${content.width}×${content.height}) 不等`);
    ensure(visual.width === state.viewport.width && visual.height === state.viewport.height, 'visualViewport 与 inner viewport 不等');
    const rect = state.scrollport;
    ensure(rect && rect.width === 320 && rect.height === 240 && rect.left >= 0 && rect.top >= 0 && rect.left + rect.width <= state.viewport.width && rect.top + rect.height <= state.viewport.height, '400×300物理outer未完整处于自有opaque viewport内');
    ensure([rect.left, rect.top, rect.width, rect.height].every(value => integer(value * state.dpr)), 'ROI存在设备像素小数相位');
    ensure(Number.isFinite(state.scroll_top) && state.physical_scroll_top === state.scroll_top * state.dpr
      && Math.abs(state.physical_scroll_top - Math.round(state.physical_scroll_top)) <= phaseTolerance, '真实inner scrollTop设备相位超过明确0.003预算，禁止取整或修改');
    maxPhysicalPhaseError = Math.max(maxPhysicalPhaseError, Math.abs(state.physical_scroll_top - Math.round(state.physical_scroll_top)));
    ensure(state.scroll_left === 0 && state.client_width === 320 && state.client_height === 240 && state.scroll_height === 1440
      && state.scroll_top >= 0 && state.scroll_top <= 1200, 'inner真实scrollport尺寸/轴/范围改变');
    const nested = state.nested, ir = nested && nested.inner_rect;
    ensure(state.nested_case === nestedCase && nested && nested.inner_left_css === innerLeftCss && nested.inner_width_css === innerWidthCss
      && nested.inner_client_width === innerWidthCss && nested.inner_client_height === 240 && nested.inner_scroll_height === 1440
      && ir && ir.left === rect.left + innerLeftCss && ir.top === rect.top && ir.width === innerWidthCss && ir.height === 240
      && [ir.left, ir.top, ir.width, ir.height].every(value => integer(value * state.dpr)), '嵌套inner几何不符合固定case');
    ensure(nested.outer_client_height === 240 && nested.outer_scroll_height === 1440
      && nested.outer_scroll_top === 0 && nested.outer_scroll_left === 0 && nested.outer_scroll_epoch === 0
      && nested.document_scroll_top === 0 && nested.document_scroll_left === 0 && nested.document_scroll_epoch === 0
      && nested.inner_scroll_left === 0, 'native滚动泄漏至outer或document；不能用相同renderer HWND证明inner');
    ensure(nested.center_hit === (nestedCase === 1 ? 'outer-world' : 'inner') && nested.inner_center_hit === 'inner', '实际DOM中心命中不符');
    const spec = state.physical_spec, pool = state.pool;
    ensure(state.pattern_id === 'bgra-world-nested-v1' && spec && spec.width === 400 && spec.viewport_height === 300 && spec.document_height === 1800 && spec.tile_height === 300, 'nested独立世界规格不符');
    ensure(pool && pool.slot_count === 1 && pool.backing_bytes === innerWidthCss * 1.25 * 1800 * 4 && pool.scratch_bytes === 0
      && pool.paint_epoch === 1 && typeof pool.ready === 'boolean', '单canvas有限backing预算不符');
    ensure(Array.isArray(pool.painted_ranges) && pool.painted_ranges.length === 1 && pool.painted_ranges[0].tile === 0
      && pool.painted_ranges[0].first_row === 0 && pool.painted_ranges[0].row_count === 1800 && pool.painted_ranges[0].revision === 1, '有限canvas绘制范围改变');
    ensure(pool.visible_world_range && pool.visible_world_range.first_row === state.scroll_top * 1.25 && pool.visible_world_range.row_count === 300 && state.cycle === 1, 'world range/cold-case cycle不符');
    ensure(pool.physical_phase_error === state.physical_scroll_top - Math.round(state.physical_scroll_top)
      && pool.physical_phase_limit === phaseTolerance, 'DOM物理相位误差不等真实测量');
    ensure(Array.isArray(state.text_rects) && state.text_rects.length === 0 && state.text_outside_oracle === true, '文字必须位于oracle ROI外');
    ensure(state.observer_mode === mode && state.scenario === scenario && state.document_height_css === documentHeightCss
      && state.fixture_prepare_calls === 0, 'observer场景或fixture prepare改变');
    const observation = state.input_observer;
    ensure(Number.isSafeInteger(state.scroll_epoch) && state.scroll_epoch >= 0
      && observation && ['wheel_events', 'scroll_events', 'last_event_sequence', 'log_dropped'].every(key => Number.isSafeInteger(observation[key]) && observation[key] >= 0)
      && observation.log_limit === 256 && Array.isArray(observation.events) && observation.events.length <= observation.log_limit,
    'DOM input observer记录缺失或超过有界上限');
    ensure(observation.events.every(event => event.is_trusted === true && event.nested_case === nestedCase
      && event.outer_scroll_top === 0 && event.document_scroll_top === 0 && event.outer_scroll_epoch === 0 && event.document_scroll_epoch === 0
      && (event.type === 'wheel' ? event.composed_path.includes('inner') : event.type === 'scroll' && event.target === 'inner')), '实际wheel/scroll不是inner可信事件或outer/document曾移动');
    if (fingerprint !== null) ensure(pageFingerprint(state) === fingerprint, 'document/layout/DPR/嵌套几何合同变化，旧 lease 永久失效');
    const newer = observerLastState === null || compareObservation(state, observerLastState) > 0;
    if (observerLastState && newer) {
      ensure(state.scroll_epoch >= observerLastState.scroll_epoch && observation.wheel_events >= observerLastState.input_observer.wheel_events
        && observation.scroll_events >= observerLastState.input_observer.scroll_events, 'DOM scroll/输入计数倒退');
    }
    // 即使测量晚到也先检查freshness及所有非滚动指纹；只有严格更新的测量可更新摘要。
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
    ensure(state.pool.ready, '真实滚动尚未paintReady');
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
    ensure(autoPhase === 'setup' || autoPhase === 'paused', 'native Auto活跃期禁止renderer截图');
    const before = await settled(null, false);
    ensure(snapshotIndex < maxRendererSnapshots, 'own renderer checkpoint超过18张预算');
    const pngPath = artifact(`renderer-${String(++snapshotIndex).padStart(3, '0')}.png`);
    const rect = before.scrollport;
    const pngBytes = await page.screenshot({ clip: { x: rect.left, y: rect.top, width: rect.width, height: rect.height }, scale: 'device',
      animations: 'allow', caret: 'initial', omitBackground: false, timeout: 5000 });
    ensure(pngBytes.length > 0 && pngBytes.length <= 8 * 1024 * 1024, 'renderer PNG 超过有界上限');
    fs.writeFileSync(pngPath, pngBytes, { flag: 'wx' });
    const after = await checkSurface();
    ensure(before.scroll_top === after.scroll_top && before.scroll_left === after.scroll_left && before.scroll_epoch === after.scroll_epoch
      && before.pool.paint_epoch === after.pool.paint_epoch && after.pool.ready, 'renderer捕获期间滚动/paint改变，不能作为settled oracle');
    rendererCaptures.push({ cycle: currentCycle, phase: autoPhase, generation: autoGeneration, clip: { x: rect.left, y: rect.top, width: rect.width, height: rect.height },
      before_scroll_top: before.scroll_top, after_scroll_top: after.scroll_top, before_scroll_left: before.scroll_left, after_scroll_left: after.scroll_left,
      before_scroll_epoch: before.scroll_epoch, after_scroll_epoch: after.scroll_epoch,
      before_paint_epoch: before.pool.paint_epoch, after_paint_epoch: after.pool.paint_epoch,
      physical_scroll_top: after.physical_scroll_top, physical_phase_error: after.pool.physical_phase_error,
      physical_phase_limit: phaseTolerance, no_scroll_into_view: true });
    const decoded = await decodePng(pngPath);
    ensure(decoded.width === 400 && decoded.height === 300 && decoded.transparent === 0, 'renderer PNG 并非exact opaque 400×300物理outer ROI');
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
      // 原token撤销不能吞掉迟到测量发现的document/layout/DPR漂移。
      // 只在显式Browser.close已真正断连时忽略由关闭产生的只读请求失败。
      if (expectedBrowserClose && (browserClosed || !browser || !browser.isConnected())) return;
      throw error;
    }
    // revoke/new_lease 可与 DOM measurement 重叠；晚到的旧测量不能复活旧许可。
    if (lease !== currentLease || !currentLease.valid || browserClosed) return;
    const now = state.observed_unix_ms;
    if (lastLeaseAt !== null && now < lastLeaseAt) { staleLeaseMeasurements++; return; }
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
    totalLeaseMeasurements++;
    if (leaseMeasurements.length === 1024) leaseMeasurements.shift();
    leaseMeasurements.push({ seq: leaseSequence + 1, lease_id: currentLease.lease_id, observed_unix_ms: now,
      scroll_top: state.scroll_top, scroll_left: state.scroll_left, physical_scroll_top: state.physical_scroll_top,
      physical_phase_error: state.pool.physical_phase_error, physical_phase_limit: phaseTolerance,
      scroll_epoch: state.scroll_epoch, wheel_events: state.input_observer.wheel_events, scroll_events: state.input_observer.scroll_events,
      last_event_sequence: state.input_observer.last_event_sequence, layout_epoch: state.layout_epoch, cycle: currentCycle });
    await send({ type: install ? 'new_lease' : 'lease', protocol, seq: ++leaseSequence, valid: true,
      lease_id: currentLease.lease_id, document_nonce: state.document_nonce, target_id: targetId,
      layout_epoch: state.layout_epoch, dpr: state.dpr,
      visual_viewport: state.visual_viewport, scrollport: state.scrollport,
      observed_unix_ms: now });
  };
  const newLease = async () => {
    assertNotFatal();
    ensure(!browserClosed, 'Browser.close 后不得创建新 lease');
    ensure((!lease || !lease.valid) && autoPhase === 'setup' && autoGeneration === null
      && currentCycle === 1 && leasedCycle === 0, '每轮仅setup中显式安装一次fresh lease；不能复活旧门');
    await revoke('显式安装新的采集 lease');
    lease = { lease_id: crypto.randomUUID(), valid: true };
    lastLeaseAt = null;
    await sendLease(true);
    leasedCycle = currentCycle;
    ensure(leaseInstallations.length === 0, '每冷case仅安装一次lease');
    leaseInstallations.push({ cycle: currentCycle, lease_id: lease.lease_id, observed_unix_ms: lastLeaseAt, fixture_prepare_calls: fixturePrepareCalls });
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
    const resolvedConfig = { ...config, mode, scenario, nested_case: nestedCase, cursor_screen_point: cursor, document_height_css: documentHeightCss,
      artifact_root: artifactRoot, own_url: ownUrl.href,
      native_owner: { ...owner, expected_target_id: targetId },
      page_state: initial.state, initial_renderer: initial.renderer,
      scope, nested_spec: { width: 400, viewport_height: 300, document_height: 1800, tile_height: 300, dpr: 1.25 },
      heartbeat_interval_ms: heartbeatMs, late_lease_gap_ms: 100, max_lease_gap_ms: 500,
      max_requests: maxRequests, max_renderer_snapshots: maxRendererSnapshots, lease_trace_capacity: 1024,
      physical_phase_tolerance: phaseTolerance, max_native_auto_phase_acks: 48, max_renderer_drop_events: maxRendererDrops,
      cdp_input_allowed: false, dom_scroll_mutation_allowed: false, fixture_prepare_limit: 0 };
    const resolvedPath = artifact('node-resolved-config.json');
    fs.writeFileSync(resolvedPath, canonicalJson(resolvedConfig), { flag: 'wx' });
    const ready = { type: 'ready', protocol, pid: process.pid, browser_pid: owner.browser_pid,
      target_id: targetId, document_nonce: config.nonce, state: initial.state, renderer: initial.renderer,
      mode, scenario, nested_case: nestedCase, cursor_screen_point: cursor, document_height_css: documentHeightCss, physical_spec: initial.state.physical_spec,
      native_client_exact: contentWindow === undefined || contentWindow === null,
      native_content_exact: true, native_visible_content_rect: content,
      unique_page_count: 1, config_path: resolvedPath, scope };
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
      const appDataPath = fs.realpathSync(config.rust_data_dir);
      ensure(appDataPath === path.join(artifactRoot, 'data') && !fs.lstatSync(config.rust_data_dir).isSymbolicLink(), 'stress设置/history隔离目录必须为本轮artifact/data且不能为junction');
      const rustExe = fs.realpathSync(config.rust_exe);
      ensure(path.isAbsolute(rustExe) && path.basename(rustExe).toLowerCase() === 'raishot-gpui.exe', 'rust_exe 必须为 final raishot-gpui.exe');
      stdoutLog = fs.openSync(artifact('rust-stdout.jsonl'), 'wx');
      stderrLog = fs.openSync(artifact('rust-stderr.log'), 'wx');
      protocolLog = fs.openSync(artifact('node-to-rust.jsonl'), 'wx');
      rendererDropLog = fs.openSync(artifact('gpui-renderer-drops.jsonl'), 'wx');
      rust = spawn(rustExe, ['--long-browser-nested-smoke'], { windowsHide: true, cwd: path.resolve(__dirname, '..'),
        stdio: ['pipe', 'pipe', 'pipe'], env: { ...process.env,
          RUSTSHOT_LONG_BROWSER_CONFIG: resolvedPath, RUSTSHOT_LONG_BROWSER_QA_DIR: artifactRoot,
          RUSTSHOT_BROWSER_QA_MODE: mode, RUSTSHOT_BROWSER_QA_SCENARIO: scenario, RUSTSHOT_GPUI_LIFECYCLE_TRACE: '1',
          RUSTSHOT_DATA_DIR: appDataPath, RUSTSHOT_QA_DIR: artifactRoot, RUSTSHOT_QA_NO_RESIDENT_NOTIFY: '1' } });
      report.rust_started = true;
      ensure(Number.isSafeInteger(rust.pid) && rust.pid > 0, '无法启动自有 Rust QA 子进程');
      writeJson('rust-process.json', { pid: rust.pid, parent_pid: process.pid, exe: rustExe, args: ['--long-browser-nested-smoke'], mode, scenario, data_dir: appDataPath, started_unix_ms: Date.now() });
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
          if (request.phase === 'gpui_renderer_drop') {
            ensure(Object.keys(request).length === 3 && Object.hasOwn(request, 'hwnd') && Object.hasOwn(request, 'unix_ms')
              && Number.isSafeInteger(request.hwnd) && request.hwnd > 0 && Number.isSafeInteger(request.unix_ms) && request.unix_ms > 0,
            'GPUI renderer Drop JSON身份字段不完整');
            ensure(rendererDropLog !== null && rendererDropCount < maxRendererDrops, 'GPUI renderer Drop超过32条有界上限');
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
                ensure(autoPhase === 'setup' && autoGeneration === null && leasedCycle === currentCycle && lease && lease.valid, '仅本轮首次active前允许calibrate');
                payload = await snapshot(); break;
              case 'observe': payload = { state: await checkSurface() }; break;
              case 'native_auto_phase': {
                ensure(lease && lease.valid && leasedCycle === currentCycle
                  && Number.isSafeInteger(request.generation) && request.generation > 0, 'native_auto_phase需要当前轮有效lease和真实generation');
                ensure(autoGeneration === null || request.generation >= autoGeneration, 'native Auto generation倒退');
                if (request.phase === 'active') {
                  ensure(autoPhase === 'setup' || autoPhase === 'paused', 'Auto active ACK重复或缺少暂停边界');
                  ensure(autoGeneration === null || request.generation > autoGeneration, '恢复Auto必须是新的真实generation');
                  autoPhase = 'active';
                } else if (request.phase === 'paused') {
                  ensure(autoPhase === 'active' && request.producer_quiet === true && request.consumer_quiet === true
                    && request.raw_slots === 0 && request.queue_slots === 0, 'renderer对照前必须有真实Pause/drain quiet ACK');
                  autoPhase = 'paused';
                } else throw new Error('未知native Auto phase');
                autoGeneration = request.generation;
                const ack = { cycle: currentCycle, lease_id: lease.lease_id, phase: autoPhase, generation: autoGeneration,
                  producer_quiet: request.producer_quiet === true, consumer_quiet: request.consumer_quiet === true,
                  raw_slots: request.raw_slots, queue_slots: request.queue_slots, observed_unix_ms: Date.now(),
                  source: 'Rust生产Pause/drain ACK；Node不独立推断工作线程状态' };
                ensure(autoPhaseAcks.length < 48, 'native Auto phase ACK超过48条有界上限');
                autoPhaseAcks.push(ack);
                payload = { native_auto_phase: ack, state: await checkSurface() };
                break;
              }
              case 'observe_paused':
                ensure(autoPhase === 'paused' && request.generation === autoGeneration, 'observe_paused缺少同代quiet ACK');
                payload = await snapshot(); break;
              case 'wheel': rejectInput('protocol wheel'); break;
              case 'hit_test': {
                ensure(lease && lease.valid && request.nested_case === nestedCase, '只读hit_test必须属于当前case/lease');
                const state = await checkSurface(), point = request.point_css;
                ensure(point && Number.isFinite(point.x) && Number.isFinite(point.y) && point.x >= state.scrollport.left && point.x < state.scrollport.left + 320
                  && point.y >= state.scrollport.top && point.y < state.scrollport.top + 240, '只读point不在已冻结outer ROI内');
                const hit = await callDom('hit_test', point);
                ensure(hit && hit.document_nonce === config.nonce && hit.nested_case === nestedCase && hit.in_inner === true
                  && hit.composed_path.includes('inner'), '生产冻结点未实际命中inner');
                payload = { state: await checkSurface(), point_hit: hit };
                break;
              }
              case 'reset': throw new Error('两个冷隔离case禁止DOM fixture reset');
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
      const childExit = await bounded(childDone, 720000, 'Rust stress protocol 720秒超时');
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
      ensure(nativeReport.suite === 'long-browser-nested' && nativeReport.mode === mode && nativeReport.scenario === scenario
        && nativeReport.pid === rust.pid && nativeReport.ok === true && nativeReport.nested_case === nestedCase, `Rust native report失败/身份不符：${String(nativeReport.error || '')}`);
      ensure(Array.isArray(nativeReport.groups) && nativeReport.groups.length === 1 && nativeReport.groups[0].name === 'browser-nested-native-auto'
        && nativeReport.groups[0].ok === true && leasedCycle === 1 && currentCycle === 1 && fixturePrepareCalls === 0,
      'Rust native实际冷case/lease/无fixture准备不符');
      ensure(observerLastState && observerLastState.cycle === 1 && observerLastState.scroll_top === 1200
        && observerLastState.pool.ready === true, '最终真实inner DOM未严格落在1200CSS页尾');
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
    report.lease_count = totalLeaseMeasurements;
    report.lease_trace_retained = leaseMeasurements.length;
    report.lease_trace_capacity = 1024;
    report.lease_installations = leaseInstallations;
    report.completed_lease_cycles = leasedCycle;
    report.last_document_cycle = currentCycle;
    report.protocol_log_bytes = bytesProtocol;
    report.protocol_log_limit_bytes = maxProtocolLogBytes;
    report.request_limit = maxRequests;
    report.max_live_lease_gap_ms = maxLeaseGap;
    report.heartbeat_target_interval_ms = heartbeatMs;
    report.late_live_lease_updates = lateLeaseUpdates;
    report.late_live_lease_gaps = lateLeaseGaps;
    report.stale_lease_measurements_skipped = staleLeaseMeasurements;
    report.lease_freshness_limit_ms = 500;
    report.renderer_snapshots = snapshotIndex;
    report.renderer_snapshot_limit = maxRendererSnapshots;
    report.native_auto_phase_acks = autoPhaseAcks;
    report.native_auto_phase_ack_limit = 48;
    report.renderer_capture_observations = rendererCaptures;
    report.last_dom_observation = observerLastState;
    report.physical_phase_tolerance = phaseTolerance;
    report.maximum_observed_physical_phase_error = maxPhysicalPhaseError;
    report.cdp_input_calls = cdpInputCalls;
    report.dom_scroll_mutation_calls = domScrollMutationCalls;
    report.rejected_input_calls = rejectedInputCalls;
    report.rejected_dom_calls = rejectedDomCalls;
    report.fixture_prepare_calls = fixturePrepareCalls;
    report.fixture_preparations = fixturePreparations;
    report.fixture_prepare_is_native_auto_input = false;
    report.dom_scroll_mutation_guard = '精确DOM函数身份白名单；measure/settled/hitTest均只读，两个冷case没有DOM scroll修改/reset';
    report.gpui_lifecycle_trace_enabled_for_owned_child = true;
    report.gpui_renderer_drop_count = rendererDropCount;
    report.gpui_renderer_drop_limit = maxRendererDrops;
    report.gpui_renderer_drop_scope = '本次Rust子进程stdout的精确析构事件；不查询或承诺COM/atlas同步释放';
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
