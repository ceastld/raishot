// 先运行本文件生成各尺寸 PNG，再用 render.py 生成对照图；需要 sharp 与 Pillow。
const path = require('node:path');
const sharp = require('sharp');

(async () => {
  const selected = process.argv.slice(2);
  for (const name of selected.length ? selected : ['take-piece', 'paper-cut', 'paper-k', 'take-piece-mono']) {
    for (const size of [24, 32, 48, 64, 128, 164, 256, 512]) {
      await sharp(path.join(__dirname, `${name}.svg`), { density: 384 })
        .resize(size, size)
        .png()
        .toFile(path.join(__dirname, `${name}-${size}.png`));
    }
  }
})().catch(error => { console.error(error); process.exitCode = 1; });
