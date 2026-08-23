// Generate a minimal valid 512x512 RGBA PNG icon for Tauri (solid teal).
const fs = require("fs");
const zlib = require("zlib");
const path = require("path");

const SIZE = 512;
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
  const typeBuf = Buffer.from(type, "ascii");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([typeBuf, data])), 0);
  return Buffer.concat([len, typeBuf, data, crc]);
}

// IHDR
const ihdr = Buffer.alloc(13);
ihdr.writeUInt32BE(SIZE, 0);
ihdr.writeUInt32BE(SIZE, 4);
ihdr[8] = 8; // bit depth
ihdr[9] = 6; // color type RGBA
ihdr[10] = 0;
ihdr[11] = 0;
ihdr[12] = 0;

// Raw image: solid teal (#0F766E) with a subtle lighter "M" bar
const raw = Buffer.alloc((SIZE * 4 + 1) * SIZE);
for (let y = 0; y < SIZE; y++) {
  const off = y * (SIZE * 4 + 1);
  raw[off] = 0; // filter none
  for (let x = 0; x < SIZE; x++) {
    const o = off + 1 + x * 4;
    const inM = x > SIZE * 0.3 && x < SIZE * 0.7 && y > SIZE * 0.25 && y < SIZE * 0.75;
    raw[o] = inM ? 0x2d : 0x0f;     // R
    raw[o + 1] = inM ? 0xb0 : 0x76; // G
    raw[o + 2] = inM ? 0xa8 : 0x6e; // B
    raw[o + 3] = 0xff;              // A
  }
}
const idat = zlib.deflateSync(raw);

const png = Buffer.concat([
  Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
  chunk("IHDR", ihdr),
  chunk("IDAT", idat),
  chunk("IEND", Buffer.alloc(0)),
]);

const outDir = path.join(__dirname, "src-tauri", "icons");
fs.mkdirSync(outDir, { recursive: true });
fs.writeFileSync(path.join(outDir, "icon.png"), png);
console.log("wrote", path.join(outDir, "icon.png"), png.length, "bytes");
