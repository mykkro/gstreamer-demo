// Minimal MPEG-TS demuxer for one H.265 elementary stream + H.265 access-unit helpers.
// Enough for what GStreamer's mpegtsmux produces (PSI sections fit in one packet).

const TS = 188;
const STREAM_TYPE_HEVC = 0x24;

export class TsDemuxer {
  constructor(onAccessUnit) {
    this.onAccessUnit = onAccessUnit; // (Buffer annexB, pts90k|null) => void
    this.rest = Buffer.alloc(0);
    this.pmtPid = -1;
    this.videoPid = -1;
    this.pes = [];
    this.pesLen = 0;
    this.pesExpected = 0; // 0 = unbounded (usual for video), flush on next PUSI
  }

  push(chunk) {
    let buf = this.rest.length ? Buffer.concat([this.rest, chunk]) : chunk;
    let off = 0;
    while (buf.length - off >= TS) {
      if (buf[off] !== 0x47) { off++; continue; } // resync
      this.packet(buf.subarray(off, off + TS));
      off += TS;
    }
    this.rest = Buffer.from(buf.subarray(off));
  }

  packet(p) {
    const pusi = (p[1] & 0x40) !== 0;
    const pid = ((p[1] & 0x1f) << 8) | p[2];
    const afc = (p[3] >> 4) & 3;
    if (!(afc & 1)) return; // no payload
    let off = 4;
    if (afc & 2) off += 1 + p[4];
    if (off >= TS) return;
    const payload = p.subarray(off);

    if (pid === 0 && pusi) this.parsePat(payload);
    else if (pid === this.pmtPid && pusi) this.parsePmt(payload);
    else if (pid === this.videoPid) {
      if (pusi) {
        this.flush();
        if (payload.length >= 6) {
          const len = payload.readUInt16BE(4);
          this.pesExpected = len ? len + 6 : 0;
        }
      }
      if (pusi || this.pes.length) {
        this.pes.push(payload);
        this.pesLen += payload.length;
        if (this.pesExpected && this.pesLen >= this.pesExpected) this.flush();
      }
    }
  }

  parsePat(pl) {
    const s = 1 + pl[0]; // skip pointer_field
    const sectionLen = ((pl[s + 1] & 0x0f) << 8) | pl[s + 2];
    for (let i = s + 8; i < s + 3 + sectionLen - 4; i += 4) {
      const program = pl.readUInt16BE(i);
      if (program !== 0) { this.pmtPid = pl.readUInt16BE(i + 2) & 0x1fff; return; }
    }
  }

  parsePmt(pl) {
    const s = 1 + pl[0];
    const sectionLen = ((pl[s + 1] & 0x0f) << 8) | pl[s + 2];
    const progInfoLen = pl.readUInt16BE(s + 10) & 0x0fff;
    const end = s + 3 + sectionLen - 4;
    for (let i = s + 12 + progInfoLen; i + 5 <= end; ) {
      const type = pl[i];
      const pid = pl.readUInt16BE(i + 1) & 0x1fff;
      const esInfoLen = pl.readUInt16BE(i + 3) & 0x0fff;
      if (type === STREAM_TYPE_HEVC) { this.videoPid = pid; return; }
      i += 5 + esInfoLen;
    }
  }

  flush() {
    if (!this.pes.length) return;
    const pes = Buffer.concat(this.pes, this.pesLen);
    this.pes = [];
    this.pesLen = 0;
    if (pes.length < 9 || pes[0] !== 0 || pes[1] !== 0 || pes[2] !== 1) return;
    const hdrLen = pes[8];
    let pts = null;
    if (pes[7] & 0x80) {
      const b = pes.subarray(9, 14);
      pts = (b[0] & 0x0e) * 536870912 + b[1] * 4194304 + (b[2] & 0xfe) * 16384 + b[3] * 128 + (b[4] >> 1);
    }
    this.onAccessUnit(pes.subarray(9 + hdrLen), pts);
  }
}

// Split an Annex-B buffer into NAL units (without start codes).
function* nalUnits(buf) {
  let i = 0, start = -1;
  while (i + 3 <= buf.length) {
    if (buf[i] === 0 && buf[i + 1] === 0 && (buf[i + 2] === 1 || (buf[i + 2] === 0 && buf[i + 3] === 1))) {
      const sc = buf[i + 2] === 1 ? 3 : 4;
      if (start >= 0) yield buf.subarray(start, i);
      i += sc;
      start = i;
    } else i++;
  }
  if (start >= 0) yield buf.subarray(start);
}

// Remove emulation-prevention bytes (00 00 03 -> 00 00).
function rbsp(nal, max = 40) {
  const out = [];
  for (let i = 0; i < nal.length && out.length < max; i++) {
    if (i >= 2 && nal[i] === 3 && nal[i - 1] === 0 && nal[i - 2] === 0) continue;
    out.push(nal[i]);
  }
  return Buffer.from(out);
}

// RFC 6381 / ISO 14496-15 codec string from an H.265 SPS, e.g. "hev1.1.6.L120.90".
function codecFromSps(nal) {
  const r = rbsp(nal);
  const ptl = 3; // 2-byte NAL header + 1 byte (vps id, max_sub_layers, nesting flag)
  const space = r[ptl] >> 6;
  const tier = (r[ptl] >> 5) & 1;
  const profile = r[ptl] & 0x1f;
  let compat = r.readUInt32BE(ptl + 1);
  let rev = 0;
  for (let i = 0; i < 32; i++) { rev = (rev << 1) | (compat & 1); compat >>>= 1; }
  const constraints = [...r.subarray(ptl + 5, ptl + 11)];
  while (constraints.length && constraints.at(-1) === 0) constraints.pop();
  const level = r[ptl + 11];
  return [
    "hev1",
    ["", "A", "B", "C"][space] + profile,
    (rev >>> 0).toString(16).toUpperCase(),
    (tier ? "H" : "L") + level,
    ...constraints.map((b) => b.toString(16).toUpperCase()),
  ].join(".");
}

// Look at an access unit: is it a keyframe (IRAP), and which codec string does its SPS give?
export function analyzeAccessUnit(au) {
  let key = false, codec = null;
  for (const nal of nalUnits(au)) {
    if (nal.length < 2) continue;
    const type = (nal[0] >> 1) & 0x3f;
    if (type >= 16 && type <= 21) key = true; // BLA / IDR / CRA
    else if (type === 33 && nal.length > 16) codec = codecFromSps(nal);
  }
  return { key, codec };
}
