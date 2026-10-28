import React, { useEffect, useMemo, useRef, useState } from "react";

// 录屏编解码实验结果：target/qa/codec-lab/report.json 由
// RUSTSHOT_LAB_SOURCE=shot.png cargo test --lib recording_quality_lab -- --ignored 生成。
// 图片按物理像素 1:1 显示（CSS 尺寸除以 devicePixelRatio），放大用最近邻，避免浏览器自身缩放把差异抹平。

const BASE = "/codec-lab/";
const MODES = [
  { id: "slider", label: "滑动对比" },
  { id: "side", label: "并排" },
  { id: "flick", label: "闪切" },
  { id: "diff", label: "误差 ×4" },
];
const ZOOMS = [1, 2, 3, 4];
const DECODERS = { media_foundation: "预览（软件 XVP）", media_foundation_d3d11: "显卡解码", ffmpeg: "ffmpeg 精确" };
const GROUP_TAG = { "x264 基线": "parallel", "RustShot 候选": "stream", 码率曲线: "stream" };

function useDpr() {
  const [dpr, setDpr] = useState(window.devicePixelRatio || 1);
  useEffect(() => {
    let media;
    const watch = () => {
      setDpr(window.devicePixelRatio || 1);
      media?.removeEventListener("change", watch);
      media = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
      media.addEventListener("change", watch);
    };
    watch();
    return () => media?.removeEventListener("change", watch);
  }, []);
  return dpr;
}

const num = (v, digits = 1) => (v == null ? "—" : Number(v).toFixed(digits));

function bestOf(variants, pick, higher = true) {
  const values = variants.map(pick).filter((v) => v != null);
  if (!values.length) return null;
  return higher ? Math.max(...values) : Math.min(...values);
}

function Pix({ src, width, height, dpr, zoom, alt }) {
  // 物理 1:1：CSS 尺寸 = 像素 / dpr × 放大倍数
  return (
    <img
      src={src}
      alt={alt}
      draggable={false}
      className="pix"
      style={{ width: (width / dpr) * zoom, height: (height / dpr) * zoom }}
    />
  );
}

function Compare({ a, b, shot, mode, zoom, dpr, crop, full }) {
  const [split, setSplit] = useState(50);
  const [showB, setShowB] = useState(true);
  const box = useRef(null);
  const w = full ? 1012 : crop[2];
  const h = full ? 876 : crop[3];
  const kind = full ? "full" : "crop";
  const src = (v) => `${BASE}${v.id}-f${shot}-${kind}.png`;

  useEffect(() => {
    if (mode !== "flick") return;
    const timer = setInterval(() => setShowB((s) => !s), 700);
    return () => clearInterval(timer);
  }, [mode]);

  const cssW = (w / dpr) * zoom;
  const cssH = (h / dpr) * zoom;

  if (mode === "side") {
    return (
      <div className="cmp-side">
        {[a, b].map((v) => (
          <figure key={v.id}>
            <figcaption>{v.label}</figcaption>
            <div className="cmp-scroll">
              <Pix src={src(v)} width={w} height={h} dpr={dpr} zoom={zoom} alt={v.label} />
            </div>
          </figure>
        ))}
      </div>
    );
  }
  if (mode === "diff") {
    const diffs = [a, b].filter((v) => v.id !== "source");
    return (
      <div className="cmp-side">
        {diffs.length === 0 && <p className="muted">原图没有误差图，换一个编码方案。</p>}
        {diffs.map((v) => (
          <figure key={v.id}>
            <figcaption>{v.label} · 越亮误差越大</figcaption>
            <div className="cmp-scroll">
              <Pix
                src={`${BASE}${v.id}-f${shot}-diff.png`}
                width={crop[2]}
                height={crop[3]}
                dpr={dpr}
                zoom={zoom}
                alt={`${v.label} 误差`}
              />
            </div>
          </figure>
        ))}
      </div>
    );
  }
  if (mode === "flick") {
    const v = showB ? b : a;
    return (
      <div>
        <div className="flick-label">
          <span className={showB ? "" : "on"}>A · {a.label}</span>
          <span className={showB ? "on" : ""}>B · {b.label}</span>
        </div>
        <div className="cmp-scroll">
          <div className="flick" style={{ width: cssW, height: cssH }}>
            <Pix src={src(a)} width={w} height={h} dpr={dpr} zoom={zoom} alt={a.label} />
            <div style={{ opacity: showB ? 1 : 0 }}>
              <Pix src={src(b)} width={w} height={h} dpr={dpr} zoom={zoom} alt={b.label} />
            </div>
          </div>
        </div>
        <p className="muted small">每 0.7 秒切换一次，盯住同一处文字看边缘变化。当前：{v.label}</p>
      </div>
    );
  }
  const move = (e) => {
    const rect = box.current?.getBoundingClientRect();
    if (!rect) return;
    const x = (e.touches ? e.touches[0].clientX : e.clientX) - rect.left;
    setSplit(Math.min(100, Math.max(0, (x / rect.width) * 100)));
  };
  return (
    <div className="cmp-scroll">
      <div
        ref={box}
        className="slider"
        style={{ width: cssW, height: cssH }}
        onMouseMove={(e) => e.buttons === 1 && move(e)}
        onMouseDown={move}
        onTouchMove={move}
        role="slider"
        aria-label="对比分割位置"
        aria-valuenow={Math.round(split)}
        tabIndex={0}
        onKeyDown={(e) => {
          if (e.key === "ArrowLeft") setSplit((s) => Math.max(0, s - 2));
          if (e.key === "ArrowRight") setSplit((s) => Math.min(100, s + 2));
        }}
      >
        <Pix src={src(b)} width={w} height={h} dpr={dpr} zoom={zoom} alt={b.label} />
        <div className="slider-a" style={{ clipPath: `inset(0 ${100 - split}% 0 0)` }}>
          <Pix src={src(a)} width={w} height={h} dpr={dpr} zoom={zoom} alt={a.label} />
        </div>
        <div className="slider-bar" style={{ left: `${split}%` }} />
        <span className="slider-tag left">A · {a.label}</span>
        <span className="slider-tag right">B · {b.label}</span>
      </div>
    </div>
  );
}

export default function CodecLab() {
  const dpr = useDpr();
  const [report, setReport] = useState(null);
  const [error, setError] = useState("");
  const [shot, setShot] = useState(null);
  const [aId, setA] = useState("source");
  const [bId, setB] = useState("mf_compact_cbr");
  const [mode, setMode] = useState("slider");
  const [zoom, setZoom] = useState(2);
  const [full, setFull] = useState(false);
  const [video, setVideo] = useState("mf_compact_cbr");

  useEffect(() => {
    fetch(`${BASE}report.json`, { cache: "no-store" })
      .then((r) => (r.ok ? r.json() : Promise.reject(new Error(`${r.status}`))))
      .then((data) => {
        setReport(data);
        setShot(data.shots?.[1]?.frame ?? data.shots?.[0]?.frame);
      })
      .catch((e) => setError(`读不到 report.json（${e.message}）：先运行实验命令。`));
  }, []);

  const variants = useMemo(() => (report?.variants ?? []).filter((v) => !v.error), [report]);
  const failed = useMemo(() => (report?.variants ?? []).filter((v) => v.error), [report]);
  const options = useMemo(
    () => [{ id: "source", label: "原图（无损）", group: "基准" }, ...variants],
    [variants],
  );
  const a = options.find((v) => v.id === aId) ?? options[0];
  const b = options.find((v) => v.id === bId) ?? options[1] ?? options[0];

  if (error) {
    return (
      <div className="bench">
        <div className="bench-head">
          <h1>录屏编解码实验</h1>
        </div>
        <p className="err small">{error}</p>
        <pre className="cmd">RUSTSHOT_LAB_SOURCE=target/qa/codec-lab/shot.png cargo test --lib recording_quality_lab -- --ignored --nocapture</pre>
      </div>
    );
  }
  if (!report) return <div className="bench muted">加载中…</div>;

  const best = {
    kbps: bestOf(variants, (v) => v.kbps, false),
    still: bestOf(variants, (v) => v.quality?.psnr?.static),
    scroll: bestOf(variants, (v) => v.quality?.psnr?.scroll),
    min: bestOf(variants, (v) => v.quality?.psnr?.min),
    lumaStill: bestOf(variants, (v) => v.quality?.luma_psnr?.static),
    lumaScroll: bestOf(variants, (v) => v.quality?.luma_psnr?.scroll),
  };
  const maxKbps = Math.max(...variants.map((v) => v.kbps ?? 0), 1);
  const src = report.source;
  const crop = report.crop;
  const cell = (value, top, digits = 1) => (
    <td className={`num${value != null && value === top ? " best" : ""}`}>{num(value, digits)}</td>
  );

  return (
    <div className="bench lab">
      <div className="bench-head">
        <h1>录屏编解码实验</h1>
        <p className="muted">
          同一段无损素材 {src.size[0]}×{src.size[1]} @ {src.fps}fps：前 {src.static_frames} 帧静止，后{" "}
          {src.scroll_frames} 帧每帧滚动 {src.scroll_px_per_frame}px。RustShot 方案走产品同一 GPU 编码路径（BGRA→NV12→
          {variants.find((v) => v.encoder?.name)?.encoder?.name ?? "系统编码器"}），全部方案都用预览同一解码路径（Media
          Foundation）解码后与原图逐帧比较。
        </p>
        <p className="muted small">
          画面按屏幕物理像素 1:1 显示（当前缩放 {Math.round(dpr * 100)}%），放大为最近邻。生成于{" "}
          {new Date(report.generated * 1000).toLocaleString()}。
        </p>
      </div>

      <Findings variants={report.variants} />

      <section className="history">
        <div className="history-head">
          <h2>指标</h2>
          <span className="muted small">
            PSNR 越高越接近原图，每差 3 dB 误差约翻倍。亮度 PSNR 决定文字边缘是否发糊；RGB 与亮度的差距来自色度（4:2:0、解码转换）。点行设为 B。
          </span>
        </div>
        <div className="table-wrap">
          <table>
            <thead>
              <tr>
                <th>方案</th>
                <th>分组</th>
                <th className="num">码率 kbps</th>
                <th>体积</th>
                <th className="num">RGB 静止</th>
                <th className="num">RGB 滚动</th>
                <th className="num">RGB 最差帧</th>
                <th className="num">亮度 静止</th>
                <th className="num">亮度 滚动</th>
                <th>解码</th>
                <th>说明</th>
              </tr>
            </thead>
            <tbody>
              {variants.map((v) => (
                <tr key={v.id} className={v.id === bId ? "sel" : ""} onClick={() => setB(v.id)}>
                  <td>
                    <strong>{v.label}</strong>
                  </td>
                  <td>
                    <span className={`tag ${GROUP_TAG[v.group] ?? ""}`}>{v.group}</span>
                  </td>
                  {cell(v.kbps, best.kbps, 0)}
                  <td>
                    <span className="bar">
                      <i style={{ width: `${((v.kbps ?? 0) / maxKbps) * 100}%` }} />
                    </span>
                  </td>
                  {cell(v.quality?.psnr?.static, best.still)}
                  {cell(v.quality?.psnr?.scroll, best.scroll)}
                  {cell(v.quality?.psnr?.min, best.min)}
                  {cell(v.quality?.luma_psnr?.static, best.lumaStill)}
                  {cell(v.quality?.luma_psnr?.scroll, best.lumaScroll)}
                  <td className="muted">{DECODERS[v.decode] ?? v.decode}</td>
                  <td className="muted clip" title={encoderNote(v)}>
                    {v.note}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        {failed.map((v) => (
          <p key={v.id} className="err small">
            {v.label}：{v.error}
          </p>
        ))}
      </section>

      <section className="history">
        <div className="history-head">
          <h2>画面对比</h2>
          <div className="seg" role="tablist" aria-label="帧">
            {report.shots.map((s) => (
              <button key={s.frame} className={shot === s.frame ? "on" : ""} onClick={() => setShot(s.frame)}>
                {s.label} · 第 {s.frame} 帧
              </button>
            ))}
          </div>
          <div className="seg" role="tablist" aria-label="方式">
            {MODES.map((m) => (
              <button key={m.id} className={mode === m.id ? "on" : ""} onClick={() => setMode(m.id)}>
                {m.label}
              </button>
            ))}
          </div>
        </div>
        <div className="lab-controls">
          <label className="f">
            <span>A</span>
            <select value={a.id} onChange={(e) => setA(e.target.value)}>
              {options.map((v) => (
                <option key={v.id} value={v.id}>
                  {v.label}
                </option>
              ))}
            </select>
          </label>
          <button className="btn" onClick={() => (setA(b.id), setB(a.id))} title="交换 A／B">
            ⇄
          </button>
          <label className="f">
            <span>B</span>
            <select value={b.id} onChange={(e) => setB(e.target.value)}>
              {options.map((v) => (
                <option key={v.id} value={v.id}>
                  {v.label}
                </option>
              ))}
            </select>
          </label>
          <div className="f">
            <span>放大</span>
            <div className="seg">
              {ZOOMS.map((z) => (
                <button key={z} className={zoom === z ? "on" : ""} onClick={() => setZoom(z)}>
                  {z}×
                </button>
              ))}
            </div>
          </div>
          <label className="check">
            <input type="checkbox" checked={full} disabled={mode === "diff"} onChange={(e) => setFull(e.target.checked)} />
            整帧（默认只看文字最密的 {crop[2]}×{crop[3]}）
          </label>
        </div>
        {shot != null && <Compare a={a} b={b} shot={shot} mode={mode} zoom={zoom} dpr={dpr} crop={crop} full={full && mode !== "diff"} />}
      </section>

      <section className="history">
        <div className="history-head">
          <h2>播放</h2>
          <span className="muted small">浏览器解码，按物理像素 1:1 显示；用来感受滚动时的整体观感。</span>
          <select className="vsel" value={video} onChange={(e) => setVideo(e.target.value)} aria-label="播放方案">
            {variants.map((v) => (
              <option key={v.id} value={v.id}>
                {v.label}
              </option>
            ))}
          </select>
        </div>
        <div className="cmp-scroll">
          <video
            key={video}
            src={`${BASE}${video}.mp4`}
            controls
            loop
            muted
            autoPlay
            style={{ width: src.size[0] / dpr, height: src.size[1] / dpr }}
          />
        </div>
      </section>
    </div>
  );
}

function encoderNote(v) {
  const e = v.encoder ?? {};
  const parts = [];
  if (e.name) parts.push(e.name);
  if (e.target_bitrate) parts.push(`目标 ${Math.round(e.target_bitrate / 1000)} kbps`);
  if (e.read_rate_control != null) parts.push(`实际码控 ${e.read_rate_control}`);
  if (e.read_quality_vs_speed != null) parts.push(`速度/质量 ${e.read_quality_vs_speed}`);
  if (e.crf != null) parts.push(`CRF ${e.crf}`);
  return parts.join(" · ");
}

// 结论随 report.json 的数值更新；缺少某个方案时对应条目不显示。
function Findings({ variants }) {
  const by = Object.fromEntries(variants.filter((v) => !v.error).map((v) => [v.id, v]));
  const l = (id, k = "static") => by[id]?.quality?.luma_psnr?.[k];
  const r = (id, k = "static") => by[id]?.quality?.psnr?.[k];
  const kb = (id) => by[id]?.kbps;
  const f = (x) => (x == null ? "—" : Number(x).toFixed(1));
  const items = [];
  if (by.mf_compact_cbr && by.x264_crf23)
    items.push(
      <li key="enc">
        <strong>主因是编码。</strong>当前“压缩 MP4”静止画面亮度 {f(l("mf_compact_cbr"))} dB、滚动 {f(l("mf_compact_cbr", "scroll"))} dB；
        x264 CRF 23 基线为 {f(l("x264_crf23"))}／{f(l("x264_crf23", "scroll"))} dB，码率却只有 {f(kb("x264_crf23"))} kbps（当前 {f(kb("mf_compact_cbr"))}）。
        首帧最差 {f(r("mf_compact_cbr", "min"))} dB：CBR 下 I 帧比特不够，静止画面之后几乎不再补细节。
      </li>,
    );
  if (by.mf_hq_vbr || by.mf_vbr_bpp12)
    items.push(
      <li key="vbr">
        <strong>有效的旋钮只有码率控制模式和码率。</strong>峰值 VBR：压缩档 {f(l("mf_compact_vbr"))}／{f(l("mf_compact_vbr", "scroll"))} dB，
        0.12 bit/像素 {f(l("mf_vbr_bpp12"))}／{f(l("mf_vbr_bpp12", "scroll"))} dB（{f(kb("mf_vbr_bpp12"))} kbps），
        高清档 {f(l("mf_hq_vbr"))}／{f(l("mf_hq_vbr", "scroll"))} dB（{f(kb("mf_hq_vbr"))} kbps），已达到或超过 x264 CRF 23。
        关低延迟、加大缓冲、质量优先对 NVENC 无效（输出逐字节相同）；Quality 模式反而更差。
      </li>,
    );
  if (by.mf_hq_cbr_ffdec && by.mf_hq_cbr_gpudec)
    items.push(
      <li key="dec">
        <strong>次因是预览解码的色度插值。</strong>同一高清文件：预览软件 XVP RGB {f(r("mf_hq_cbr"))} dB，显卡解码 {f(r("mf_hq_cbr_gpudec"))} dB，
        ffmpeg 精确插值 {f(r("mf_hq_cbr_ffdec"))} dB；亮度三者相同，差别在彩色边缘（ClearType 彩边、图标）。
      </li>,
    );
  if (by.x264_lossless420_ffdec)
    items.push(
      <li key="cap">
        <strong>4:2:0 上限。</strong>无损编码只做色度下采样也只有 RGB {f(r("x264_lossless420_ffdec"))} dB（亮度 {f(l("x264_lossless420_ffdec"))}）；
        MP4／H.264 4:2:0 都受此限制。
      </li>,
    );
  if (!items.length) return null;
  return (
    <section className="history findings">
      <div className="history-head">
        <h2>结论</h2>
      </div>
      <ol>{items}</ol>
    </section>
  );
}
