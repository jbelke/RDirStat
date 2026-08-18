#!/usr/bin/env node
/**
 * Stellar RDIRSTAT — brand asset generator.
 *
 * Renders every shipped icon from one geometric definition, with no image
 * dependencies at all: shapes are signed-distance fields evaluated per pixel,
 * and the PNG/ICO containers are written by hand on top of `node:zlib`. The
 * point is reproducibility — `node scripts/generate-icons.mjs` produces
 * byte-identical output on a developer Mac, in the Docker `assets` profile,
 * and in CI, so "are these still the branded icons?" is a question `git
 * status` can answer.
 *
 * The `.icns` is the one exception: it is assembled by `iconutil`, which only
 * exists on macOS. Elsewhere the iconset directory is left on disk and the
 * existing `.icns` is untouched rather than replaced with something wrong.
 *
 * Nothing here reads the Tauri v2 template icons. That is the whole point:
 * every file under src-tauri/icons/ is generated from THIS file.
 */

import { Buffer } from "node:buffer";
import { spawnSync } from "node:child_process";
import { deflateSync } from "node:zlib";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "..");
const ICONS = join(ROOT, "src-tauri", "icons");
const PUBLIC = join(ROOT, "public");

// ---------------------------------------------------------------------------
// Brand
//
// The mark is a treemap: one dominant tile and three that divide the remainder,
// which is literally what the application draws. The plate behind it is the
// "stellar" half of the name — a deep indigo-to-black vertical gradient rather
// than the flat near-black the first pass used.
// ---------------------------------------------------------------------------

const BRAND = {
  plateTop: [0x1c, 0x15, 0x44],
  plateBottom: [0x07, 0x05, 0x11],
  tiles: [
    // x0, y0, x1, y1 in tile-field units, radius as a fraction of the field.
    { rect: [0.0, 0.0, 0.5797, 1.0], radius: 0.043, color: [0x94, 0x5f, 0xf9] },
    { rect: [0.6099, 0.0, 1.0, 0.5205], radius: 0.043, color: [0xf0, 0xab, 0x55] },
    { rect: [0.6099, 0.5507, 0.8071, 1.0], radius: 0.039, color: [0x7c, 0xca, 0x7f] },
    { rect: [0.8373, 0.5507, 1.0, 1.0], radius: 0.039, color: [0xea, 0x68, 0x78] },
  ],
};

/**
 * Apple's macOS icon grid: the art occupies 824 of a 1024 canvas, centred,
 * with a 185.4pt corner radius. Full-bleed square art — which is what a naive
 * SVG export gives you — reads as obviously foreign in the Dock, so neither
 * the margin nor the radius is decoration.
 *
 * A true superellipse was tried and rejected: matching Apple's diagonal extent
 * needs an exponent near 9, which visibly flattens the sides. The circular
 * corner at the documented radius is the closer approximation.
 */
const PLATE_EXTENT = 824 / 1024;
const PLATE_RADIUS = 185.4 / 824;

/** Gutter between the plate edge and the treemap, as a fraction of the plate. */
const TILE_INSET = 0.105;

// ---------------------------------------------------------------------------
// A very small renderer: RGBA float canvas, source-over, SDF coverage.
// ---------------------------------------------------------------------------

function canvas(width, height) {
  return { width, height, data: new Float64Array(width * height * 4) };
}

/**
 * Composites a shape over the canvas.
 *
 * `sdf(x, y)` returns the signed distance to the shape edge in pixels
 * (negative inside); coverage is the clamped half-pixel band around zero,
 * which is the standard analytic antialiasing for distance fields and is why
 * a 16x16 icon rendered here is crisper than a 1024 downsample.
 *
 * `paint(x, y)` returns `[r, g, b, a]` with channels in 0..1.
 */
function fill(target, sdf, paint) {
  const { width, height, data } = target;
  for (let y = 0; y < height; y += 1) {
    for (let x = 0; x < width; x += 1) {
      const px = x + 0.5;
      const py = y + 0.5;
      const distance = sdf(px, py);
      if (distance > 1) continue;
      const coverage = Math.min(1, Math.max(0, 0.5 - distance));
      if (coverage <= 0) continue;
      const [r, g, b, a] = paint(px, py);
      const alpha = a * coverage;
      if (alpha <= 0) continue;
      const i = (y * width + x) * 4;
      const inverse = 1 - alpha;
      data[i] = r * alpha + data[i] * inverse;
      data[i + 1] = g * alpha + data[i + 1] * inverse;
      data[i + 2] = b * alpha + data[i + 2] * inverse;
      data[i + 3] = alpha + data[i + 3] * inverse;
    }
  }
}

function roundedRect(x0, y0, x1, y1, radius) {
  const cx = (x0 + x1) / 2;
  const cy = (y0 + y1) / 2;
  const hw = (x1 - x0) / 2;
  const hh = (y1 - y0) / 2;
  const r = Math.min(radius, hw, hh);
  return (px, py) => {
    const qx = Math.abs(px - cx) - (hw - r);
    const qy = Math.abs(py - cy) - (hh - r);
    const outside = Math.hypot(Math.max(qx, 0), Math.max(qy, 0));
    return outside + Math.min(Math.max(qx, qy), 0) - r;
  };
}

const solid = ([r, g, b], alpha = 1) => () => [r / 255, g / 255, b / 255, alpha];

/** Composites `source` onto `target` at `(x, y)`, scaled by `alpha`. */
function compose(target, source, x, y, alpha) {
  for (let sy = 0; sy < source.height; sy += 1) {
    const ty = y + sy;
    if (ty < 0 || ty >= target.height) continue;
    for (let sx = 0; sx < source.width; sx += 1) {
      const tx = x + sx;
      if (tx < 0 || tx >= target.width) continue;
      const si = (sy * source.width + sx) * 4;
      const a = source.data[si + 3] * alpha;
      if (a <= 0) continue;
      const ti = (ty * target.width + tx) * 4;
      const inverse = 1 - a;
      for (let channel = 0; channel < 3; channel += 1) {
        target.data[ti + channel] =
          source.data[si + channel] * a + target.data[ti + channel] * inverse;
      }
      target.data[ti + 3] = a + target.data[ti + 3] * inverse;
    }
  }
}

function verticalGradient(top, bottom, y0, y1) {
  return (_px, py) => {
    const t = Math.min(1, Math.max(0, (py - y0) / (y1 - y0)));
    return [
      (top[0] + (bottom[0] - top[0]) * t) / 255,
      (top[1] + (bottom[1] - top[1]) * t) / 255,
      (top[2] + (bottom[2] - top[2]) * t) / 255,
      1,
    ];
  };
}

// ---------------------------------------------------------------------------
// PNG / ICO containers
// ---------------------------------------------------------------------------

const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c;
  }
  return table;
})();

function crc32(buffer) {
  let c = 0xffffffff;
  for (let i = 0; i < buffer.length; i += 1) c = CRC_TABLE[(c ^ buffer[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function pngChunk(type, body) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(body.length, 0);
  const typed = Buffer.concat([Buffer.from(type, "ascii"), body]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(typed), 0);
  return Buffer.concat([length, typed, crc]);
}

function encodePng(target) {
  const { width, height, data } = target;
  // Filter type 0 on every scanline. The art is large flat areas, so the
  // adaptive filters buy almost nothing and cost determinism.
  const raw = Buffer.alloc(height * (1 + width * 4));
  let offset = 0;
  for (let y = 0; y < height; y += 1) {
    raw[offset] = 0;
    offset += 1;
    for (let x = 0; x < width; x += 1) {
      const i = (y * width + x) * 4;
      for (let channel = 0; channel < 4; channel += 1) {
        raw[offset] = Math.max(0, Math.min(255, Math.round(data[i + channel] * 255)));
        offset += 1;
      }
    }
  }

  const header = Buffer.alloc(13);
  header.writeUInt32BE(width, 0);
  header.writeUInt32BE(height, 4);
  header[8] = 8; // bit depth
  header[9] = 6; // colour type: RGBA
  header[10] = 0; // deflate
  header[11] = 0; // adaptive filtering
  header[12] = 0; // no interlace

  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", header),
    pngChunk("IDAT", deflateSync(raw, { level: 9 })),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

/** PNG-compressed ICO. Supported by every Windows the Tauri bundler targets. */
function encodeIco(entries) {
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2);
  header.writeUInt16LE(entries.length, 4);

  const directory = Buffer.alloc(16 * entries.length);
  let offset = header.length + directory.length;
  for (const [index, entry] of entries.entries()) {
    const at = index * 16;
    directory[at] = entry.size >= 256 ? 0 : entry.size;
    directory[at + 1] = entry.size >= 256 ? 0 : entry.size;
    directory[at + 2] = 0;
    directory[at + 3] = 0;
    directory.writeUInt16LE(1, at + 4);
    directory.writeUInt16LE(32, at + 6);
    directory.writeUInt32LE(entry.png.length, at + 8);
    directory.writeUInt32LE(offset, at + 12);
    offset += entry.png.length;
  }

  return Buffer.concat([header, directory, ...entries.map((entry) => entry.png)]);
}

// ---------------------------------------------------------------------------
// The mark itself
// ---------------------------------------------------------------------------

/**
 * Draws the application icon at `size`, rendered natively rather than
 * downsampled, so 16px stays legible.
 *
 * `bleed` draws the plate edge-to-edge instead of on Apple's 824/1024 grid —
 * correct for Windows tiles and the web favicon, wrong for `.icns`.
 */
function drawMark(size, { bleed = false } = {}) {
  const target = canvas(size, size);
  const extent = bleed ? 1 : PLATE_EXTENT;
  const half = (size * extent) / 2;
  const center = size / 2;

  fill(
    target,
    roundedRect(
      center - half,
      center - half,
      center + half,
      center + half,
      half * 2 * PLATE_RADIUS,
    ),
    verticalGradient(BRAND.plateTop, BRAND.plateBottom, center - half, center + half),
  );

  const field = size * extent * (1 - TILE_INSET * 2);
  const origin = center - field / 2;
  for (const tile of BRAND.tiles) {
    const [x0, y0, x1, y1] = tile.rect;
    fill(
      target,
      roundedRect(
        origin + x0 * field,
        origin + y0 * field,
        origin + x1 * field,
        origin + y1 * field,
        tile.radius * field,
      ),
      solid(tile.color),
    );
  }

  return target;
}

/**
 * The menu-bar icon: black ink on a transparent field, three tiles rather than
 * four because the fourth is a smudge at 22 points. macOS inverts a template
 * image for the active appearance, so any colour here would be a bug.
 */
function drawTray(size) {
  const target = canvas(size, size);
  const unit = size / 88;
  const ink = solid([0, 0, 0]);
  const bars = [
    [6, 6, 50, 82, 6],
    [56, 6, 82, 48, 6],
    [56, 54, 82, 82, 6],
  ];
  for (const [x0, y0, x1, y1, r] of bars) {
    fill(target, roundedRect(x0 * unit, y0 * unit, x1 * unit, y1 * unit, r * unit), ink);
  }
  return target;
}

/**
 * The disk-image backdrop. Sized to `bundle.macOS.dmg.windowSize`, with the
 * chevrons pointing from `appPosition` to `applicationFolderPosition` — change
 * one and you must change the other, which is why both live in
 * tauri.conf.json and are read from there by eye, not guessed.
 */
function drawDmgBackground(width, height) {
  const target = canvas(width, height);
  fill(
    target,
    () => -1,
    verticalGradient([0x14, 0x0f, 0x33], [0x06, 0x04, 0x0e], 0, height),
  );

  // Chevrons in the gap between the app (x=180) and the Applications alias
  // (x=480), brightening toward the destination. Both x values come from
  // `bundle.macOS.dmg` in tauri.conf.json — move an icon there and the
  // pointing gets wrong here.
  const midY = 168;
  const arm = 16;
  const thickness = 6;
  for (let index = 0; index < 3; index += 1) {
    const x = 298 + index * 27;
    const alpha = 0.18 + index * 0.14;
    const sdf = (px, py) => {
      const dx = px - x;
      const dy = Math.abs(py - midY);
      // Distance to the segment (0, arm)->(arm, 0), mirrored about the axis,
      // which is one chevron.
      const vx = arm;
      const vy = -arm;
      const wx = dx;
      const wy = dy - arm;
      const t = Math.min(1, Math.max(0, (wx * vx + wy * vy) / (vx * vx + vy * vy)));
      return Math.hypot(wx - vx * t, wy - vy * t) - thickness / 2;
    };
    fill(target, sdf, solid([0xc9, 0xb8, 0xff], alpha));
  }

  // A muted echo of the mark, bottom-left, so the window is recognisably ours
  // before the volume icon renders. Composited as a finished icon rather than
  // drawn as translucent tiles: alpha-blending the tiles individually lets the
  // gradient through each one and turns the palette to mud.
  compose(target, drawMark(76), 28, height - 76 - 28, 0.62);

  return target;
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/**
 * `--check` renders everything in memory and compares it against what is
 * committed instead of writing. That turns "we are not shipping the Tauri v2
 * template icons" from a claim into a gate: the committed bytes have to be the
 * bytes this file describes, so a stray drag-and-drop into src-tauri/icons/
 * fails the build rather than shipping.
 *
 * `--out <dir>` writes the tree somewhere else, which is how the check
 * compares the `.icns` without disturbing the working copy.
 */
const CHECK = process.argv.includes("--check");
const OUT_FLAG = process.argv.indexOf("--out");
const OUT_ROOT = OUT_FLAG === -1 ? ROOT : resolve(process.argv[OUT_FLAG + 1]);

const written = [];
const drifted = [];
const skipped = [];

const relative = (path) => path.replace(`${ROOT}/`, "");
const relocate = (path) => (OUT_ROOT === ROOT ? path : join(OUT_ROOT, relative(path)));

function emit(path, buffer) {
  if (CHECK) {
    let current;
    try {
      current = readFileSync(path);
    } catch {
      drifted.push(`${relative(path)} — missing`);
      return;
    }
    if (current.equals(buffer)) {
      written.push(relative(path));
    } else {
      drifted.push(
        `${relative(path)} — ${current.length} bytes committed, ${buffer.length} generated`,
      );
    }
    return;
  }
  const target = relocate(path);
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, buffer);
  written.push(relative(path));
}

function emitPng(path, target) {
  emit(path, encodePng(target));
}

/**
 * The favicon and the in-app mark. Hand-written rather than traced from the
 * raster so it stays a few hundred bytes and scales in the webview; the
 * numbers are the same brand constants used above.
 */
function markSvg({ size, radius }) {
  const field = 1024 * (1 - TILE_INSET * 2);
  const origin = (1024 - field) / 2;
  const rects = BRAND.tiles
    .map((tile) => {
      const [x0, y0, x1, y1] = tile.rect;
      const x = origin + x0 * field;
      const y = origin + y0 * field;
      const w = (x1 - x0) * field;
      const h = (y1 - y0) * field;
      const r = tile.radius * field;
      const hex = tile.color.map((c) => c.toString(16).padStart(2, "0")).join("");
      return `  <rect x="${x.toFixed(0)}" y="${y.toFixed(0)}" width="${w.toFixed(0)}" height="${h.toFixed(0)}" rx="${r.toFixed(0)}" fill="#${hex}"/>`;
    })
    .join("\n");
  const top = BRAND.plateTop.map((c) => c.toString(16).padStart(2, "0")).join("");
  const bottom = BRAND.plateBottom.map((c) => c.toString(16).padStart(2, "0")).join("");
  return `<svg xmlns="http://www.w3.org/2000/svg" width="${size}" height="${size}" viewBox="0 0 1024 1024" fill="none" role="img" aria-label="Stellar RDIRSTAT">
  <defs>
    <linearGradient id="plate" x1="0" y1="0" x2="0" y2="1">
      <stop offset="0" stop-color="#${top}"/>
      <stop offset="1" stop-color="#${bottom}"/>
    </linearGradient>
  </defs>
  <rect width="1024" height="1024" rx="${radius}" fill="url(#plate)"/>
${rects}
</svg>
`;
}

function traySvg() {
  return `<svg xmlns="http://www.w3.org/2000/svg" width="88" height="88" viewBox="0 0 88 88" fill="none" role="img" aria-label="Stellar RDIRSTAT">
  <!-- Menu-bar template: black ink, transparent field. macOS inverts it. -->
  <rect x="6" y="6" width="44" height="76" rx="6" fill="#000000"/>
  <rect x="56" y="6" width="26" height="42" rx="6" fill="#000000"/>
  <rect x="56" y="54" width="26" height="28" rx="6" fill="#000000"/>
</svg>
`;
}

function buildIcns() {
  // Always assembled in a temp directory and then handed to `emit`, so
  // `--check` and `--out` behave for the .icns exactly as they do for every
  // other asset instead of needing a second code path.
  const workdir = mkdtempSync(join(tmpdir(), "stellar-icons-"));

  try {
    if (process.platform !== "darwin") {
      skipped.push("src-tauri/icons/icon.icns (macOS packaging is unavailable)");
      return;
    }

    // Tauri's icon packager accepts the renderer's PNG directly and produces
    // a valid ICNS on current macOS releases, whose iconutil rejects these
    // otherwise-valid hand-written PNGs as an iconset.
    const source = join(workdir, "icon.png");
    const output = join(workdir, "tauri");
    writeFileSync(source, encodePng(drawMark(1024)));
    const result = spawnSync(
      "pnpm",
      ["exec", "tauri", "icon", "--output", output, source],
      { stdio: "inherit" },
    );
    if (result.status !== 0) {
      throw new Error(`Tauri icon packaging failed with status ${result.status}`);
    }
    emit(join(ICONS, "icon.icns"), readFileSync(join(output, "icon.icns")));
  } finally {
    rmSync(workdir, { recursive: true, force: true });
  }
}

function main() {
  // macOS grid: the plate sits on Apple's 824/1024 margin.
  for (const size of [32, 64, 128, 256, 512, 1024]) {
    const name =
      size === 256 ? "128x128@2x.png" : size === 1024 ? "icon.png" : `${size}x${size}.png`;
    emitPng(join(ICONS, name), drawMark(size));
  }

  // Windows Store tiles and the .ico: full bleed, because the platform masks.
  for (const size of [30, 44, 71, 89, 107, 142, 150, 284, 310]) {
    emitPng(join(ICONS, `Square${size}x${size}Logo.png`), drawMark(size, { bleed: true }));
  }
  emitPng(join(ICONS, "StoreLogo.png"), drawMark(50, { bleed: true }));
  emit(
    join(ICONS, "icon.ico"),
    encodeIco(
      [16, 32, 48, 64, 128, 256].map((size) => ({
        size,
        png: encodePng(drawMark(size, { bleed: true })),
      })),
    ),
  );

  // Menu bar.
  emitPng(join(ICONS, "tray.png"), drawTray(44));
  emitPng(join(ICONS, "tray@2x.png"), drawTray(88));

  // Installer.
  emitPng(join(ICONS, "dmg-background.png"), drawDmgBackground(660, 400));

  // Vector: the webview favicon and the source of truth for anyone drawing
  // the mark by hand.
  emit(join(ICONS, "stellar-rdirstat-mark.svg"), Buffer.from(markSvg({ size: 1024, radius: 220 })));
  emit(join(ICONS, "stellar-rdirstat-tray.svg"), Buffer.from(traySvg()));
  emit(join(PUBLIC, "stellar-rdirstat.svg"), Buffer.from(markSvg({ size: 32, radius: 220 })));

  buildIcns();

  for (const note of skipped) {
    console.warn(`! skipped ${note}`);
  }

  if (!CHECK) {
    console.log(`Generated ${written.length} brand assets under ${OUT_ROOT}:`);
    for (const path of written) console.log(`  ${path}`);
    return;
  }

  if (drifted.length > 0) {
    console.error(
      `\n${drifted.length} brand asset(s) do not match scripts/generate-icons.mjs:`,
    );
    for (const note of drifted) console.error(`  ${note}`);
    console.error(
      "\nThe committed icons are not the ones the source describes. Either run" +
        "\n`./rush.sh icons` to regenerate them, or revert whatever replaced them." +
        "\nThis check exists so the Tauri v2 template icons cannot come back.",
    );
    process.exitCode = 1;
    return;
  }

  console.log(
    `OK — ${written.length} committed brand assets match scripts/generate-icons.mjs.`,
  );
}

main();
