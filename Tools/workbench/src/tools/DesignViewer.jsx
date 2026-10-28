import React from "react";

export default function DesignViewer({ file }) {
  const src = `/design/${encodeURIComponent(file)}`;
  return (
    <div className="design">
      <div className="design-bar">
        <code>design/{file}</code>
        <a href={src} target="_blank" rel="noreferrer">
          新标签页打开
        </a>
      </div>
      <iframe key={file} title={file} src={src} />
    </div>
  );
}
