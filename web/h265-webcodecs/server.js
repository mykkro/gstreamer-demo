// H.265 relay: producer (MPEG-TS over TCP) -> Node -> WebSocket -> browser WebCodecs.
//
//   producer :500x ──TCP──► ChannelRelay (one per channel, shared) ──WS──► browser 1
//                           demux TS → H.265 access units          ├──WS──► browser 2
//                           GOP cache for instant join             └──WS──► ...
//
// The H.265 bitstream is passed through untouched. There's no transcoding, so 100 viewers cost one
// upstream connection plus bandwidth.
//
// Usage: node server.js [--producer 127.0.0.1] [--control-port 5000] [--port 8080] [--https]

import http from "node:http";
import https from "node:https";
import net from "node:net";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { parseArgs } from "node:util";
import { WebSocketServer } from "ws";
import { TsDemuxer, analyzeAccessUnit } from "./ts.js";

const { values: opt } = parseArgs({
  options: {
    producer: { type: "string", default: "127.0.0.1" },
    "control-port": { type: "string", default: "5000" },
    port: { type: "string", default: "8080" },
    https: { type: "boolean", default: false },
  },
});
const CONTROL_PORT = Number(opt["control-port"]);
const PUBLIC = path.join(path.dirname(fileURLToPath(import.meta.url)), "public");
const MAX_BUFFERED = 4 * 1024 * 1024; // per-client send backlog before we drop frames until the next keyframe

// ---- producer control protocol (same as python/common.py) ------------------------------------
function producerSend(msg, timeoutMs = 3000) {
  return new Promise((resolve, reject) => {
    const sock = net.connect(CONTROL_PORT, opt.producer, () => sock.write(JSON.stringify(msg) + "\n"));
    let buf = "";
    sock.setTimeout(timeoutMs, () => sock.destroy(new Error("timeout")));
    sock.on("data", (d) => {
      buf += d;
      const nl = buf.indexOf("\n");
      if (nl >= 0) { sock.end(); resolve(JSON.parse(buf.slice(0, nl))); }
    });
    sock.on("error", reject);
  });
}
const producerRequest = (cmd) => producerSend({ cmd });

// Channel changes pushed by the producer ("watch"), forwarded to browsers as Server-Sent Events.
const eventClients = new Set();
function broadcastEvent(ev) {
  for (const res of eventClients) res.write(`data: ${JSON.stringify(ev)}\n\n`);
}
function watchProducer() {
  const sock = net.connect(CONTROL_PORT, opt.producer, () => sock.write(JSON.stringify({ cmd: "watch" }) + "\n"));
  let buf = "", first = true;
  sock.on("data", (d) => {
    buf += d;
    let nl;
    while ((nl = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, nl);
      buf = buf.slice(nl + 1);
      if (first) { first = false; broadcastEvent({ event: "connected" }); continue; } // initial list
      const ev = JSON.parse(line);
      if (ev.event === "channel_removed") dropRelay(ev.id);
      console.log(`producer: ${ev.event} ${ev.id ?? ev.channel?.id}`);
      broadcastEvent(ev);
    }
  });
  sock.on("error", () => {});
  sock.on("close", () => setTimeout(watchProducer, 2000)); // producer restarted / not up yet
}

// ---- one upstream connection per channel, fanned out to all browser clients ----------------
class ChannelRelay {
  constructor(channel) {
    this.channel = channel;
    this.clients = new Set();
    this.gop = [];          // access units since the last keyframe (sent to joining clients)
    this.config = null;     // {type:"config", codec, ...} derived from the SPS
    this.sock = null;
    this.frames = 0;
  }

  add(ws) {
    this.clients.add(ws);
    ws.waitKey = false;
    if (this.config) ws.send(JSON.stringify(this.config));
    for (const au of this.gop) ws.send(au);  // decoder catches up from the last IDR → instant picture
    if (!this.sock) this.connect();
  }

  remove(ws) {
    this.clients.delete(ws);
    if (this.clients.size === 0) this.disconnect();
  }

  connect() {
    const { port, id } = this.channel;
    console.log(`[${id}] upstream connect ${opt.producer}:${port}`);
    const demux = new TsDemuxer((data, pts) => this.onAccessUnit(data, pts));
    this.sock = net.connect(port, opt.producer);
    this.sock.setNoDelay(true);
    this.sock.on("data", (chunk) => demux.push(chunk));
    this.sock.on("error", (e) => console.log(`[${id}] upstream error: ${e.message}`));
    this.sock.on("close", () => {
      this.sock = null;
      this.gop = [];
      if (this.clients.size) {
        console.log(`[${id}] upstream closed, reconnecting in 2 s`);
        setTimeout(() => this.clients.size && !this.sock && this.connect(), 2000);
      }
    });
  }

  disconnect() {
    console.log(`[${this.channel.id}] no viewers left, closing upstream`);
    this.sock?.destroy();
    this.sock = null;
    this.gop = [];
  }

  onAccessUnit(data, pts) {
    const info = analyzeAccessUnit(data);
    if (info.codec && info.codec !== this.config?.codec) {
      this.config = { type: "config", codec: info.codec, channel: this.channel };
      console.log(`[${this.channel.id}] stream codec ${info.codec}`);
      for (const ws of this.clients) ws.send(JSON.stringify(this.config));
    }
    // binary message: [flags u8][timestamp µs f64 BE][Annex-B access unit]
    const msg = Buffer.allocUnsafe(9 + data.length);
    msg.writeUInt8(info.key ? 1 : 0, 0);
    msg.writeDoubleBE(pts === null ? 0 : (pts * 1e6) / 90000, 1);
    data.copy(msg, 9);

    if (info.key) this.gop = [];
    if (info.key || this.gop.length) this.gop.push(msg);  // only cache once we have a keyframe
    if (this.gop.length > 600) this.gop = [];             // safety cap (GOP way longer than expected)

    for (const ws of this.clients) {
      if (ws.readyState !== 1) continue;
      // slow client: drop frames until its backlog drains, then resume at a keyframe
      if (ws.bufferedAmount > MAX_BUFFERED) { ws.waitKey = true; continue; }
      if (ws.waitKey && !info.key) continue;
      ws.waitKey = false;
      ws.send(msg);
    }
  }
}

const relays = new Map(); // channel id -> ChannelRelay

/** A channel was removed by the producer: tell its viewers and forget the relay. */
function dropRelay(id) {
  const relay = relays.get(id);
  if (!relay) return;
  relays.delete(id);
  for (const ws of relay.clients) {
    ws.send(JSON.stringify({ type: "gone", id }));
    ws.close();
  }
  relay.clients.clear();
  relay.disconnect();
}

// ---- HTTP: static files + /api/channels -------------------------------------------------------
const MIME = { ".html": "text/html; charset=utf-8", ".js": "text/javascript", ".css": "text/css" };

async function readBody(req) {
  let body = "";
  for await (const chunk of req) {
    body += chunk;
    if (body.length > 10000) throw new Error("body too large");
  }
  return body ? JSON.parse(body) : {};
}

function sendJson(res, status, obj) {
  res.writeHead(status, { "content-type": "application/json" });
  res.end(JSON.stringify(obj));
}

async function onRequest(req, res) {
  const url = new URL(req.url, "http://x");
  if (url.pathname === "/api/events") {
    res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-cache", connection: "keep-alive" });
    res.write(": channel events\n\n");
    eventClients.add(res);
    req.on("close", () => eventClients.delete(res));
    return;
  }
  // add / remove cameras on producers that support it (bevy-streamer)
  const cam = url.pathname.match(/^\/api\/cameras(?:\/([a-z0-9_]+))?$/);
  if (cam) {
    try {
      let msg;
      if (req.method === "POST" && !cam[1]) msg = { ...(await readBody(req)), cmd: "add_camera" };
      else if (req.method === "DELETE" && cam[1]) msg = { cmd: "remove_camera", name: cam[1] };
      else return sendJson(res, 405, { ok: false, error: "use POST /api/cameras or DELETE /api/cameras/<name>" });
      const reply = await producerSend(msg, 8000);
      return sendJson(res, reply.ok ? 200 : 400, reply);
    } catch (e) {
      return sendJson(res, 502, { ok: false, error: e.message });
    }
  }
  if (url.pathname === "/api/channels") {
    try {
      const info = await producerRequest("list");
      const viewers = Object.fromEntries([...relays].map(([id, r]) => [id, r.clients.size]));
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ ...info, viewers }));
    } catch (e) {
      res.writeHead(502, { "content-type": "application/json" });
      res.end(JSON.stringify({ ok: false, error: `producer not reachable: ${e.message}` }));
    }
    return;
  }
  const file = path.join(PUBLIC, url.pathname === "/" ? "index.html" : path.normalize(url.pathname));
  if (!file.startsWith(PUBLIC) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
    res.writeHead(404).end("not found");
    return;
  }
  res.writeHead(200, { "content-type": MIME[path.extname(file)] || "application/octet-stream" });
  fs.createReadStream(file).pipe(res);
}

async function createServer() {
  if (!opt.https) return http.createServer(onRequest);
  // WebCodecs needs a secure context: localhost is fine, other machines need HTTPS.
  const { default: selfsigned } = await import("selfsigned");
  const pems = await selfsigned.generate([{ name: "commonName", value: "gstreamer-demo" }], { days: 365, keySize: 2048 });
  console.log("HTTPS with a self-signed certificate: the browser will warn once, accept it to continue.");
  return https.createServer({ key: pems.private, cert: pems.cert }, onRequest);
}

const server = await createServer();
const wss = new WebSocketServer({ noServer: true });

server.on("upgrade", async (req, socket, head) => {
  const url = new URL(req.url, "http://x");
  if (url.pathname !== "/ws") return socket.destroy();
  let channels;
  try {
    channels = (await producerRequest("list")).channels;
  } catch {
    return socket.destroy();
  }
  const ch = channels.find((c) => c.id === url.searchParams.get("channel")) ?? channels[0];
  wss.handleUpgrade(req, socket, head, (ws) => {
    if (!relays.has(ch.id) || relays.get(ch.id).channel.port !== ch.port) relays.set(ch.id, new ChannelRelay(ch));
    const relay = relays.get(ch.id);
    relay.add(ws);
    console.log(`[${ch.id}] viewer joined (${relay.clients.size} watching)`);
    ws.on("close", () => {
      relay.remove(ws);
      console.log(`[${ch.id}] viewer left (${relay.clients.size} watching)`);
    });
  });
});

watchProducer();
server.listen(Number(opt.port), () => {
  const scheme = opt.https ? "https" : "http";
  console.log(`H.265 WebCodecs relay for producer ${opt.producer}:${CONTROL_PORT}`);
  console.log(`  open ${scheme}://localhost:${opt.port}/`);
});
