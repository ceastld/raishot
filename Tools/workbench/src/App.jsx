import React, { useEffect, useState } from "react";
import TranslateBench from "./tools/TranslateBench.jsx";
import DesignViewer from "./tools/DesignViewer.jsx";
import CodecLab from "./tools/CodecLab.jsx";

// 新测试界面在这里登记一行即可出现在侧栏；路由为 #/<id>。
const TOOLS = [
  { id: "translate", label: "翻译测速", hint: "真实调用 · 整段／流式／分段并发", Component: TranslateBench },
  { id: "codec", label: "录屏编解码", hint: "码率控制 · PSNR · 1:1 画面对比", Component: CodecLab },
];

function useHashRoute() {
  const read = () => window.location.hash.replace(/^#\/?/, "") || "translate";
  const [route, setRoute] = useState(read);
  useEffect(() => {
    const onHash = () => setRoute(read());
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);
  return route;
}

export default function App() {
  const route = useHashRoute();
  const [designs, setDesigns] = useState([]);
  useEffect(() => {
    fetch("/api/designs")
      .then((r) => r.json())
      .then(setDesigns)
      .catch(() => setDesigns([]));
  }, []);

  const tool = TOOLS.find((t) => t.id === route);
  const designFile = route.startsWith("design/") ? route.slice("design/".length) : null;

  return (
    <div className="shell">
      <nav className="side" aria-label="测试界面">
        <div className="brand">RustShot 测试台</div>
        <div className="group">测试</div>
        {TOOLS.map((t) => (
          <a key={t.id} href={`#/${t.id}`} className="nav" aria-current={route === t.id ? "page" : undefined}>
            <span>{t.label}</span>
            <small>{t.hint}</small>
          </a>
        ))}
        <div className="group">设计原型 · docs/design/</div>
        {designs.length === 0 && <p className="design-empty">未找到设计原型。原型随源码保存在 docs/design/。</p>}
        {designs.map((d) => (
          <a
            key={d.file}
            href={`#/design/${d.file}`}
            className="nav"
            aria-current={designFile === d.file ? "page" : undefined}
          >
            <span>{d.title}</span>
            <small>{d.file}</small>
          </a>
        ))}
      </nav>
      <main className="main">
        {tool ? <tool.Component /> : designFile ? <DesignViewer file={designFile} /> : <TranslateBench />}
      </main>
    </div>
  );
}
