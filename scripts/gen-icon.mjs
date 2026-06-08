// Generates app icons (PNG set + ICO) with no external deps.
// Draws a teal rounded square with a white crosshair/aperture mark.
import { deflateSync } from "node:zlib";
import { writeFileSync, mkdirSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const OUT = join(dirname(fileURLToPath(import.meta.url)), "..", "src-tauri", "icons");
mkdirSync(OUT, { recursive: true });

const crcTable = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();
function crc32(buf) {
  let c = 0xffffffff;
  for (let i = 0; i < buf.length; i++) c = crcTable[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}
function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length, 0);
  const tb = Buffer.from(type, "ascii");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([tb, data])), 0);
  return Buffer.concat([len, tb, data, crc]);
}
function encodePng(size, rgba) {
  const sig = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8;  // bit depth
  ihdr[9] = 6;  // RGBA
  // rest 0
  const stride = size * 4;
  const raw = Buffer.alloc((stride + 1) * size);
  for (let y = 0; y < size; y++) {
    raw[y * (stride + 1)] = 0; // filter none
    rgba.copy(raw, y * (stride + 1) + 1, y * stride, y * stride + stride);
  }
  const idat = deflateSync(raw, { level: 9 });
  return Buffer.concat([sig, chunk("IHDR", ihdr), chunk("IDAT", idat), chunk("IEND", Buffer.alloc(0))]);
}

function draw(size) {
  const buf = Buffer.alloc(size * size * 4);
  const r = Math.round(size * 0.22); // corner radius
  const cx = size / 2, cy = size / 2;
  const ringOuter = size * 0.30, ringInner = size * 0.20;
  const cross = size * 0.42, crossW = Math.max(1, size * 0.035);
  const inRounded = (x, y) => {
    if (x >= r && x <= size - r) return y >= 0 && y < size;
    if (y >= r && y <= size - r) return x >= 0 && x < size;
    const corners = [[r, r], [size - r, r], [r, size - r], [size - r, size - r]];
    for (const [px, py] of corners) {
      if ((x < r || x > size - r) && (y < r || y > size - r)) {
        return Math.hypot(x - px, y - py) <= r;
      }
    }
    return true;
  };
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      const i = (y * size + x) * 4;
      if (!inRounded(x + 0.5, y + 0.5)) { buf[i + 3] = 0; continue; }
      // teal gradient background
      const t = y / size;
      let R = Math.round(20 + 10 * t), G = Math.round(150 - 30 * t), B = Math.round(150 - 20 * t);
      let A = 255;
      const d = Math.hypot(x - cx, y - cy);
      // white aperture ring
      if (d <= ringOuter && d >= ringInner) { R = G = B = 245; }
      // crosshair lines
      const onV = Math.abs(x - cx) <= crossW && Math.abs(y - cy) <= cross;
      const onH = Math.abs(y - cy) <= crossW && Math.abs(x - cx) <= cross;
      if (onV || onH) { R = G = B = 245; }
      buf[i] = R; buf[i + 1] = G; buf[i + 2] = B; buf[i + 3] = A;
    }
  }
  return buf;
}

function makeIco(size) {
  const png = encodePng(size, draw(size));
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0); header.writeUInt16LE(1, 2); header.writeUInt16LE(1, 4);
  const entry = Buffer.alloc(16);
  entry[0] = size >= 256 ? 0 : size;
  entry[1] = size >= 256 ? 0 : size;
  entry[2] = 0; entry[3] = 0;
  entry.writeUInt16LE(1, 4); entry.writeUInt16LE(32, 6);
  entry.writeUInt32LE(png.length, 8);
  entry.writeUInt32LE(6 + 16, 12);
  return Buffer.concat([header, entry, png]);
}

const sizes = { "32x32.png": 32, "128x128.png": 128, "128x128@2x.png": 256, "icon.png": 512 };
for (const [name, s] of Object.entries(sizes)) {
  writeFileSync(join(OUT, name), encodePng(s, draw(s)));
  console.log("wrote", name);
}
writeFileSync(join(OUT, "icon.ico"), makeIco(256));
console.log("wrote icon.ico");
