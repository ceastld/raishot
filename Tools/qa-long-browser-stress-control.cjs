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
const maxProtocolLogBytes = 32 * 1024 * 1024;
const maxRequests = 512;
const maxRendererSnapshots = 18;
const scope = '自有 Chrome 960×640物理ROI、32000行三轮真实DOM/DXGI与GPUI移交；非native Auto或网页字形验收';
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
  ensure(process.argv.length === 3, '用法：node qa-long-browser-stress-control.cjs <config.json>');
  const configPath = fs.realpathSync(process.argv[2]);
  const config = JSON.parse(fs.readFileSync(configPath, 'utf8').replace(/^\uFEFF/, ''));
  ensure(config.protocol === protocol, 'config protocol 必须为 1');
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
  let lateLeaseUpdates = 0;
  let bytesStdout = 0, bytesStderr = 0, bytesProtocol = 0, closeRequested = false, logsClosed = false;
  let currentCycle = 1, leasedCycle = 0, totalLeaseMeasurements = 0;
  let childExitObserved = null;
  const revocations = [], leaseMeasurements = [], lateLeaseGaps = [];
  const requestIds = new Set();
  let childResolve;
  const childDone = new Promise(resolve => { childResolve = resolve; });
  const report = {
    suite: 'long-browser-stress-control', protocol, scope, pid: process.pid,
    preflight_only: config.preflight_only === true,
    own_url: ownUrl.href, artifact_root: artifactRoot,
    browser_pid: owner.browser_pid, full_desktop_freeze_used: false,
    global_input_sent: false, clipboard_untouched: true,
    renderer_only_preflight: config.preflight_only === true,
    browser_close_requested: false, browser_disconnected_after_close: false,
    rust_started: false, rust_normal_exit: false, finished_protocol: false,
    ok: false,
  };
  let stdoutLog = null, stderrLog = null, protocolLog = null;
  const output = value => process.stdout.write(JSON.stringify(value) + '\n');
  const stopHeartbeat = () => { if (leaseTimer) clearInterval(leaseTimer); leaseTimer = null; };
  const send = packet => {
    const encoded = JSON.stringify(packet) + '\n';
    ensure(Buffer.byteLength(encoded) <= maxLineBytes, '发送 NDJSON 超过上限');
    writeChain = writeChain.then(() => new Promise((resolve, reject) => {
      if (!rust || rust.stdin.destroyed || rust.stdin.writableEnded) return reject(new Error('Rust stdin 已关闭'));
      if (protocolLog && !logsClosed) {
        bytesProtocol += Buffer.byteLength(encoded);
        if (bytesProtocol > maxProtocolLogBytes) return reject(new Error('Node→Rust NDJSON 超过32MiB预算'));
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
  const pageFingerprint = state => JSON.stringify({
    document_nonce: state.document_nonce, document_epoch: state.document_epoch,
    layout_epoch: state.layout_epoch, dpr: state.dpr, visual_viewport: state.visual_viewport,
    scrollport: state.scrollport, viewport: state.viewport,
    physical_spec: state.physical_spec, pattern_id: state.pattern_id,
    pool_slot_count: state.pool.slot_count, backing_bytes: state.pool.backing_bytes, scratch_bytes: state.pool.scratch_bytes,
  });
  const checkState = async state => {
    ensure(state && state.ready === true && state.document_nonce === config.nonce, '文档 API/nonce 不属于本调用');
    // Windows 各进程 Date.now 的毫秒量化偶尔相差 1ms。仅等 Node 时钟追上，
    // 不重盖 renderer 时刻；最终仍拒绝未来值及超过 500ms 的旧测量。
    const clockLead = state.observed_unix_ms - Date.now();
    if (clockLead > 0 && clockLead <= 2) await new Promise(resolve => setTimeout(resolve, 2));
    const receivedAt = Date.now();
    ensure(Number.isSafeInteger(state.observed_unix_ms) && state.observed_unix_ms > 0 && state.observed_unix_ms <= receivedAt && receivedAt - state.observed_unix_ms <= 500,
      `DOM measurement 不是当前 500ms 内的非未来测量：observed=${String(state.observed_unix_ms)} received=${receivedAt}`);
    ensure(state.url === ownUrl.href && state.title === `RustShot Browser Stress QA ${config.nonce}`, '文档导航或标题身份已变化');
    ensure(typeof state.document_epoch === 'string' && state.document_epoch && Number.isSafeInteger(state.layout_epoch), '缺少文档/layout epoch');
    ensure(state.dpr === 1.25 && Math.abs(state.dpr - owner.dpi / 96) < 1e-6, 'stress仅支持已验证DPR1.25，且必须与原生DPI一致');
    const visual = state.visual_viewport;
    ensure(visual && visual.scale === 1 && visual.offset_left === 0 && visual.offset_top === 0, 'visualViewport 有缩放或平移');
    ensure(integer(state.viewport.width * state.dpr) && integer(state.viewport.height * state.dpr), 'DOM viewport 非整设备像素');
    ensure(Math.round(state.viewport.width * state.dpr) === content.width && Math.round(state.viewport.height * state.dpr) === content.height, `DOM viewport × DPR (${state.viewport.width}×${state.viewport.height} @${state.dpr}) 与原生可见content (${content.width}×${content.height}) 不等`);
    ensure(visual.width === state.viewport.width && visual.height === state.viewport.height, 'visualViewport 与 inner viewport 不等');
    const rect = state.scrollport;
    ensure(rect && rect.width === 768 && rect.height === 512 && rect.left >= 0 && rect.top >= 0 && rect.left + rect.width <= state.viewport.width && rect.top + rect.height <= state.viewport.height, '960×640物理scroller未完整处于自有opaque viewport内');
    ensure([rect.left, rect.top, rect.width, rect.height, state.scroll_top].every(value => integer(value * state.dpr)), 'scroller/scrollTop 存在设备像素小数相位');
    ensure(state.scroll_left === 0 && state.client_width === 768 && state.client_height === 512 && state.scroll_height === 25600, 'stress DOM scrollport尺寸/轴不符合固定合同');
    ensure(state.scroll_top >= 0 && state.scroll_top <= 25088, 'scrollTop 越界');
    const spec = state.physical_spec, pool = state.pool;
    ensure(state.pattern_id === 'bgra-world-tiled-v2' && spec && spec.width === 960 && spec.viewport_height === 640 && spec.document_height === 32000 && spec.tile_height === 1000, 'stress独立世界规格不符');
    ensure(pool && pool.slot_count === 3 && pool.backing_bytes === 11520000 && pool.scratch_bytes === 3840000 && Number.isSafeInteger(pool.paint_epoch) && pool.paint_epoch > 0 && typeof pool.ready === 'boolean', '三canvas+一scratch预算不符');
    ensure(Array.isArray(pool.painted_ranges) && pool.painted_ranges.length === 3 && new Set(pool.painted_ranges.map(value => value.tile)).size === 3
      && pool.painted_ranges.every(value => Number.isSafeInteger(value.tile) && value.tile >= 0 && value.tile < 32 && value.first_row === value.tile * 1000 && value.row_count === 1000 && Number.isSafeInteger(value.revision) && value.revision > 0), 'painted ranges不符合有界pool');
    ensure(pool.visible_world_range && pool.visible_world_range.first_row === state.scroll_top * 1.25 && pool.visible_world_range.row_count === 640 && state.cycle === currentCycle, 'world range/cycle 不一致');
    ensure(Array.isArray(state.text_rects) && state.text_rects.length === 0 && state.text_outside_oracle === true, '本轮文本必须在 oracle ROI 外');
    if (fingerprint !== null) ensure(pageFingerprint(state) === fingerprint, 'document/layout/DPR/标记合同变化，旧 lease 永久失效');
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
    const state = await page.evaluate(() => window.__rustshotLongBrowserStressQa.measure());
    return checkState(state);
  };
  const settled = async (previous, movement) => {
    assertNotFatal();
    const state = await page.evaluate(({ previous, movement }) => window.__rustshotLongBrowserStressQa.settled(previous, movement), { previous, movement });
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
    const before = await settled(null, false);
    ensure(snapshotIndex < maxRendererSnapshots, 'own renderer checkpoint超过18张预算');
    const pngPath = artifact(`renderer-${String(++snapshotIndex).padStart(3, '0')}.png`);
    const pngBytes = await page.locator('#scroller').screenshot({ scale: 'device', animations: 'disabled', caret: 'hide', omitBackground: false, timeout: 5000 });
    ensure(pngBytes.length > 0 && pngBytes.length <= 8 * 1024 * 1024, 'renderer PNG 超过有界上限');
    fs.writeFileSync(pngPath, pngBytes, { flag: 'wx' });
    const after = await checkSurface();
    ensure(before.scroll_top === after.scroll_top && before.scroll_epoch === after.scroll_epoch && before.pool.paint_epoch === after.pool.paint_epoch && after.pool.ready, 'renderer捕获期间滚动/paint改变，不能作为settled oracle');
    const decoded = await decodePng(pngPath);
    ensure(decoded.width === 960 && decoded.height === 640 && decoded.transparent === 0, 'renderer PNG 并非exact opaque 960×640物理scroller');
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
    totalLeaseMeasurements++;
    if (leaseMeasurements.length === 1024) leaseMeasurements.shift();
    leaseMeasurements.push({ seq: leaseSequence + 1, observed_unix_ms: now, scroll_top: state.scroll_top, layout_epoch: state.layout_epoch, cycle: currentCycle });
    await send({ type: install ? 'new_lease' : 'lease', protocol, seq: ++leaseSequence, valid: true,
      lease_id: currentLease.lease_id, document_nonce: state.document_nonce, target_id: targetId,
      layout_epoch: state.layout_epoch, dpr: state.dpr,
      visual_viewport: state.visual_viewport, scrollport: state.scrollport,
      observed_unix_ms: now });
  };
  const newLease = async () => {
    assertNotFatal();
    ensure(!browserClosed, 'Browser.close 后不得创建新 lease');
    ensure((!lease || !lease.valid) && currentCycle === leasedCycle + 1 && currentCycle <= 3, '每轮仅显式安装一次fresh lease；不能复活旧门');
    await revoke('显式安装新的采集 lease');
    lease = { lease_id: crypto.randomUUID(), valid: true };
    lastLeaseAt = null;
    await sendLease(true);
    leasedCycle = currentCycle;
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
    const processes = (await browserCdp.send('SystemInfo.getProcessInfo')).processInfo;
    const browserProcesses = processes.filter(value => value.type === 'browser');
    ensure(browserProcesses.length === 1 && browserProcesses[0].id === owner.browser_pid, 'CDP browser PID 与 supervisor 原生身份不等');
    const pages = browser.contexts().flatMap(context => context.pages());
    ensure(pages.length === 1, '自有 Chrome 必须只有唯一 page，不能选择其中之一继续');
    page = pages[0];
    ensure(page.url() === ownUrl.href, '唯一 page URL 不属于本次文档');
    pageCdp = await page.context().newCDPSession(page);
    const target = (await pageCdp.send('Target.getTargetInfo')).targetInfo;
    targetId = target.targetId;
    ensure(target.type === 'page' && target.url === ownUrl.href && targetId, '页面 target 身份无效');
    if (owner.expected_target_id) ensure(owner.expected_target_id === targetId, '预定 target ID 已变化');
    const apiReady = await page.evaluate(() => Boolean(window.__rustshotLongBrowserStressQa));
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
    const resolvedConfig = { ...config, artifact_root: artifactRoot, own_url: ownUrl.href,
      native_owner: { ...owner, expected_target_id: targetId },
      page_state: initial.state, initial_renderer: initial.renderer,
      scope, stress_spec: { width: 960, viewport_height: 640, document_height: 32000, tile_height: 1000, dpr: 1.25 },
      heartbeat_interval_ms: heartbeatMs, late_lease_gap_ms: 100, max_lease_gap_ms: 500,
      max_requests: maxRequests, max_renderer_snapshots: maxRendererSnapshots, lease_trace_capacity: 1024 };
    const resolvedPath = artifact('node-resolved-config.json');
    fs.writeFileSync(resolvedPath, canonicalJson(resolvedConfig), { flag: 'wx' });
    const ready = { type: 'ready', protocol, pid: process.pid, browser_pid: owner.browser_pid,
      target_id: targetId, document_nonce: config.nonce, state: initial.state, renderer: initial.renderer,
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
      const appData = artifact('app-data');
      fs.mkdirSync(appData, { recursive: true });
      const appDataPath = fs.realpathSync(appData);
      ensure(inside(appDataPath, artifactRoot), 'stress设置/history隔离目录越界');
      const rustExe = fs.realpathSync(config.rust_exe);
      ensure(path.isAbsolute(rustExe) && path.basename(rustExe).toLowerCase() === 'raishot-gpui.exe', 'rust_exe 必须为 final raishot-gpui.exe');
      stdoutLog = fs.openSync(artifact('rust-stdout.jsonl'), 'wx');
      stderrLog = fs.openSync(artifact('rust-stderr.log'), 'wx');
      protocolLog = fs.openSync(artifact('node-to-rust.jsonl'), 'wx');
      rust = spawn(rustExe, ['--long-browser-stress-smoke'], { windowsHide: true, cwd: path.resolve(__dirname, '..'),
        stdio: ['pipe', 'pipe', 'pipe'], env: { ...process.env,
          RUSTSHOT_LONG_BROWSER_CONFIG: resolvedPath, RUSTSHOT_LONG_BROWSER_QA_DIR: artifactRoot,
          RUSTSHOT_DATA_DIR: appDataPath, RUSTSHOT_QA_DIR: artifactRoot, RUSTSHOT_QA_NO_RESIDENT_NOTIFY: '1' } });
      report.rust_started = true;
      ensure(Number.isSafeInteger(rust.pid) && rust.pid > 0, '无法启动自有 Rust QA 子进程');
      writeJson('rust-process.json', { pid: rust.pid, parent_pid: process.pid, exe: rustExe, args: ['--long-browser-stress-smoke'], started_unix_ms: Date.now() });
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
              case 'checkpoint': payload = await snapshot(); break;
              case 'observe': payload = { state: await settled(null, false) }; break;
              case 'wheel': {
                const before = await checkSurface();
                ensure(lease && lease.valid && leasedCycle === currentCycle, '真实增长wheel必须属于本轮已安装lease');
                const delta = request.delta_y === undefined ? 200 : request.delta_y;
                ensure(delta === 200 || delta === 88, 'stress wheel仅允许200 CSS步进与最后88 CSS');
                const rect = before.scrollport;
                await page.mouse.move(rect.left + rect.width / 2, rect.top + rect.height / 2);
                await page.mouse.wheel(0, delta);
                const actual = await settled(before.scroll_top, true);
                ensure(Math.sign(actual.scroll_top - before.scroll_top) === Math.sign(delta), 'actual DOM scrollTop 与 wheel 方向不一致');
                ensure(actual.pool.ready, 'wheel完成时实际pool尚未paintReady');
                payload = { state: actual };
                payload.wheel = { delta_y: delta, before_scroll_top: before.scroll_top,
                  after_scroll_top: payload.state.scroll_top, actual_delta_css: payload.state.scroll_top - before.scroll_top,
                  settled_by_actual_scroll_and_paint_ready_and_two_raf: true };
                break;
              }
              case 'reset': {
                ensure(lease && !lease.valid && leasedCycle === currentCycle && currentCycle < 3 && request.cycle === currentCycle + 1, 'reset仅允许已revoke旧lease之后的连续轮间准备');
                currentCycle = request.cycle;
                const resetState = await page.evaluate(cycle => window.__rustshotLongBrowserStressQa.reset(cycle), currentCycle);
                await checkState(resetState);
                const state = await checkSurface();
                ensure(state.scroll_top === 0 && state.pool.ready, '轮间reset未实际稳定回到world0');
                payload = { state, reset_is_fixture_preparation: true };
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
      ensure(nativeReport.suite === 'long-browser-stress' && nativeReport.pid === rust.pid && nativeReport.ok === true, `Rust native report失败/身份不符：${String(nativeReport.error || '')}`);
      ensure(Array.isArray(nativeReport.groups) && nativeReport.groups.length === 1 && nativeReport.groups[0].name === 'browser-stress-three-cycles' && nativeReport.groups[0].ok === true && leasedCycle === 3 && currentCycle === 3, 'Rust native实际三轮验收组不符');
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
    report.completed_lease_cycles = leasedCycle;
    report.last_document_cycle = currentCycle;
    report.protocol_log_bytes = bytesProtocol;
    report.max_live_lease_gap_ms = maxLeaseGap;
    report.heartbeat_target_interval_ms = heartbeatMs;
    report.late_live_lease_updates = lateLeaseUpdates;
    report.late_live_lease_gaps = lateLeaseGaps;
    report.lease_freshness_limit_ms = 500;
    report.renderer_snapshots = snapshotIndex;
    report.fatal_guard_reason = fatal;
    logsClosed = true;
    if (stdoutLog !== null) fs.closeSync(stdoutLog);
    if (stderrLog !== null) fs.closeSync(stderrLog);
    if (protocolLog !== null) fs.closeSync(protocolLog);
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
