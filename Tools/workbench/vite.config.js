// 本地测试台：开发服务器负责两件事
// 1. /relay：把浏览器请求原样转发到 x-target-url，并逐块回传响应体（流式不缓冲），绕过 CORS；
// 2. /design/*、/api/designs：直接提供仓库 design/ 下的 HTML 原型，测试台里统一入口查看。
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { readdir, readFile } from "node:fs/promises";
import { execFileSync } from "node:child_process";
import { Readable } from "node:stream";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Agent, ProxyAgent, fetch as undiciFetch } from "undici";

const here = path.dirname(fileURLToPath(import.meta.url));
const designDir = path.resolve(here, "../../design");
// 录屏编解码实验输出（cargo test --lib recording_quality_lab -- --ignored）
const codecLabDir = path.resolve(here, "../../target/qa/codec-lab");
const CODEC_TYPES = { ".json": "application/json; charset=utf-8", ".png": "image/png", ".mp4": "video/mp4" };

// 与 RustShot 的 download_agent 一致：先看环境代理（ALL > HTTPS > HTTP），否则读 Windows 手动代理。
function systemProxy() {
  for (const name of ["ALL_PROXY", "all_proxy", "HTTPS_PROXY", "https_proxy", "HTTP_PROXY", "http_proxy"]) {
    const value = process.env[name]?.trim();
    if (value) return value.includes("://") ? value : `http://${value}`;
  }
  if (process.platform !== "win32") return null;
  try {
    const out = execFileSync("reg", ["query", "HKCU\\Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings"], { encoding: "utf8" });
    if (!/ProxyEnable\s+REG_DWORD\s+0x1/.test(out)) return null;
    const server = out.match(/ProxyServer\s+REG_SZ\s+(\S+)/)?.[1];
    if (!server) return null;
    // 形如 http=host:port;https=host:port 时取 https
    const pick = server.includes("=") ? server.split(";").find((s) => s.startsWith("https="))?.slice(6) ?? server.split(";")[0].split("=")[1] : server;
    return pick ? `http://${pick}` : null;
  } catch {
    return null;
  }
}
const proxyUrl = systemProxy();
const proxyAgent = proxyUrl ? new ProxyAgent(proxyUrl) : null;
const directAgent = new Agent({ connect: { timeout: 15_000 } });
const proxyLabel = proxyUrl ? proxyUrl.replace(/\/\/[^@/]*@/, "//***@") : "";

function readBody(req) {
  return new Promise((resolve, reject) => {
    const chunks = [];
    req.on("data", (c) => chunks.push(c));
    req.on("end", () => resolve(Buffer.concat(chunks)));
    req.on("error", reject);
  });
}

function workbenchServer() {
  return {
    name: "rustshot-workbench-server",
    configureServer(server) {
      server.middlewares.use("/relay", async (req, res) => {
        const target = req.headers["x-target-url"];
        let url;
        try {
          url = new URL(String(target));
          if (!/^https?:$/.test(url.protocol)) throw new Error("scheme");
        } catch {
          res.statusCode = 400;
          res.end("x-target-url 必须是 http(s) 地址");
          return;
        }
        const controller = new AbortController();
        // 浏览器停止或刷新时同时中断上游请求
        res.on("close", () => controller.abort());
        const headers = { "content-type": "application/json" };
        if (req.headers.authorization) headers.authorization = req.headers.authorization;
        // 默认与 App 同路由；x-relay-direct: 1 强制直连做对比
        const direct = req.headers["x-relay-direct"] === "1" || !proxyAgent;
        res.setHeader("x-relay-route", direct ? "direct" : `proxy ${proxyLabel}`);
        try {
          const body = await readBody(req);
          const upstream = await undiciFetch(url, {
            method: "POST",
            headers,
            body,
            signal: controller.signal,
            dispatcher: direct ? directAgent : proxyAgent,
          });
          res.statusCode = upstream.status;
          const type = upstream.headers.get("content-type");
          if (type) res.setHeader("content-type", type);
          res.setHeader("cache-control", "no-cache");
          res.flushHeaders();
          if (!upstream.body) return res.end();
          Readable.fromWeb(upstream.body)
            .on("error", () => res.destroy())
            .pipe(res);
        } catch (error) {
          if (controller.signal.aborted) return;
          res.statusCode = 502;
          res.setHeader("content-type", "text/plain; charset=utf-8");
          res.end(`转发失败：${error?.cause?.code ?? error?.message ?? error}`);
        }
      });

      server.middlewares.use("/api/relay-info", (_req, res) => {
        res.setHeader("content-type", "application/json; charset=utf-8");
        res.end(JSON.stringify({ proxy: proxyLabel || null }));
      });

      server.middlewares.use("/api/designs", async (_req, res) => {
        const files = (await readdir(designDir)).filter((f) => f.endsWith(".html")).sort();
        const list = await Promise.all(
          files.map(async (file) => {
            const head = (await readFile(path.join(designDir, file), "utf8")).slice(0, 8192);
            const title = head.match(/<title>([^<]*)<\/title>/i)?.[1]?.trim() || file;
            return { file, title };
          }),
        );
        res.setHeader("content-type", "application/json; charset=utf-8");
        res.end(JSON.stringify(list));
      });

      server.middlewares.use("/codec-lab", async (req, res, next) => {
        const name = decodeURIComponent((req.url || "/").split("?")[0]).replace(/^\/+/, "");
        const type = CODEC_TYPES[path.extname(name)];
        if (!/^[\w.-]+$/.test(name) || !type) return next();
        try {
          const body = await readFile(path.join(codecLabDir, name));
          res.setHeader("content-type", type);
          res.setHeader("cache-control", "no-store");
          res.end(body);
        } catch {
          res.statusCode = 404;
          res.end();
        }
      });

      server.middlewares.use("/design", async (req, res, next) => {
        const name = decodeURIComponent((req.url || "/").split("?")[0]).replace(/^\/+/, "");
        if (!/^(?:[\w.-]+\.(?:html|svg|js|css)|icons\/[\w.-]+\.svg)$/.test(name)) return next();
        try {
          const assetPath = name.startsWith("icons/")
            ? path.resolve(designDir, "../assets", name)
            : path.join(designDir, name);
          const html = await readFile(assetPath, "utf8");
          const assetType = { ".svg": "image/svg+xml", ".js": "text/javascript", ".css": "text/css" }[path.extname(name)];
          if (assetType) {
            res.setHeader("content-type", `${assetType}; charset=utf-8`);
            res.end(html);
            return;
          }
          res.setHeader("content-type", "text/html; charset=utf-8");
          // 原型按无 doctype 片段编写，补上骨架避免怪异模式
          res.end(/^\s*<!doctype/i.test(html) ? html : `<!doctype html><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">${html}`);
        } catch {
          next();
        }
      });
    },
  };
}

export default defineConfig({
  plugins: [react(), workbenchServer()],
  clearScreen: false,
});
