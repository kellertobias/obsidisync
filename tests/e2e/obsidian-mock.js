// Minimal stand-in for the `obsidian` module so gitService/vaultState can run under Node.
const fs = require("node:fs");
const path = require("node:path");

const notices = [];
class Notice {
  constructor(message) {
    this.message = message;
    this.messageEl = { style: {}, onclick: null, onkeydown: null };
    notices.push(String(message));
  }
  setMessage(message) {
    this.message = message;
    notices.push(String(message));
  }
  hide() {}
}

function normalizePath(p) {
  return String(p).replace(/\\/g, "/").replace(/\/+/g, "/").replace(/^\/+|\/+$/g, "");
}

class TFile {
  constructor(vaultRoot, relPath) {
    this.path = relPath;
    this.name = path.basename(relPath);
    const stat = fs.statSync(path.join(vaultRoot, relPath));
    this.stat = { mtime: Math.floor(stat.mtimeMs), ctime: Math.floor(stat.ctimeMs), size: stat.size };
  }
}

class Vault {
  constructor(root, name = "harness") {
    this.root = root;
    this.name = name;
    fs.mkdirSync(root, { recursive: true });
    const abs = (p) => path.join(root, normalizePath(p));
    const toArrayBuffer = (buf) => buf.buffer.slice(buf.byteOffset, buf.byteOffset + buf.byteLength);
    this.adapter = {
      readBinary: async (p) => toArrayBuffer(fs.readFileSync(abs(p))),
      writeBinary: async (p, data) => {
        fs.mkdirSync(path.dirname(abs(p)), { recursive: true });
        fs.writeFileSync(abs(p), Buffer.from(data));
      },
      write: async (p, text) => {
        fs.mkdirSync(path.dirname(abs(p)), { recursive: true });
        fs.writeFileSync(abs(p), text, "utf8");
      },
      read: async (p) => fs.readFileSync(abs(p), "utf8"),
      exists: async (p) => fs.existsSync(abs(p)),
      remove: async (p) => fs.rmSync(abs(p), { force: true }),
      mkdir: async (p) => fs.mkdirSync(abs(p), { recursive: true }),
      stat: async (p) => {
        if (!fs.existsSync(abs(p))) return null;
        const s = fs.statSync(abs(p));
        return { type: s.isDirectory() ? "folder" : "file", mtime: Math.floor(s.mtimeMs), ctime: Math.floor(s.ctimeMs), size: s.size };
      }
    };
  }
  getName() {
    return this.name;
  }
  getFiles() {
    const out = [];
    const walk = (dir) => {
      for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) walk(full);
        else out.push(new TFile(this.root, path.relative(this.root, full).split(path.sep).join("/")));
      }
    };
    walk(this.root);
    return out;
  }
  getAbstractFileByPath(p) {
    const full = path.join(this.root, normalizePath(p));
    return fs.existsSync(full) && fs.statSync(full).isFile() ? new TFile(this.root, normalizePath(p)) : null;
  }
  async cachedRead(file) {
    return fs.readFileSync(path.join(this.root, file.path), "utf8");
  }
}

async function requestUrl(options) {
  const headers = { ...(options.headers || {}) };
  if (options.contentType) headers["Content-Type"] = options.contentType;
  const response = await fetch(options.url, { method: options.method || "GET", headers, body: options.body });
  const buffer = Buffer.from(await response.arrayBuffer());
  const text = buffer.toString("utf8");
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch {
    json = null;
  }
  const result = {
    status: response.status,
    headers: Object.fromEntries(response.headers.entries()),
    text,
    json,
    arrayBuffer: buffer.buffer.slice(buffer.byteOffset, buffer.byteOffset + buffer.byteLength)
  };
  if (options.throw !== false && response.status >= 400) {
    const error = new Error(`Request failed, status ${response.status}`);
    error.status = response.status;
    throw error;
  }
  return result;
}

class Generic {
  constructor() {}
}
const Platform = { isMobile: false, isDesktop: true, isIosApp: false, isAndroidApp: false, isMacOS: true };

module.exports = new Proxy(
  { Notice, notices, normalizePath, TFile, Vault, requestUrl, Platform, setIcon: () => {} },
  {
    get(target, prop) {
      if (prop in target) return target[prop];
      // Any other class (Modal, Setting, Plugin, PluginSettingTab, ItemView, ...) is a no-op stand-in.
      return Generic;
    }
  }
);
