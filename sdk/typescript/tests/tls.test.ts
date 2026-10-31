import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type Server } from "node:https";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { Controller, ControllerError } from "../src/controller.js";
import { Sandbox } from "../src/sandbox.js";

// The same shape as a master's own PKI: a private CA over a leaf for
// 127.0.0.1, ECDSA, with each authority key id set.
const OPENSSL_CONFIG = `
[req]
distinguished_name = dn
[dn]
[ca]
basicConstraints = critical, CA:TRUE, pathlen:0
keyUsage = critical, keyCertSign, cRLSign
subjectKeyIdentifier = hash
[leaf]
subjectAltName = IP:127.0.0.1, DNS:localhost
extendedKeyUsage = serverAuth
authorityKeyIdentifier = keyid
`;

function openssl(cwd: string, ...args: string[]) {
  execFileSync("openssl", args, { cwd, stdio: "ignore" });
}

// A named curve: `req -newkey ec` on LibreSSL writes explicit parameters,
// which OpenSSL then refuses.
function ecKey(dir: string, name: string) {
  openssl(dir, "ecparam", "-name", "prime256v1", "-genkey", "-noout", "-out", `${name}-key.pem`);
}

function makeCa(dir: string, name: string) {
  ecKey(dir, name);
  openssl(dir, "req", "-new", "-x509", "-key", `${name}-key.pem`, "-sha256", "-config", "openssl.cnf",
    "-extensions", "ca", "-subj", `/CN=${name}`, "-days", "2", "-out", `${name}.pem`);
}

function issueLeaf(dir: string, ca: string) {
  ecKey(dir, "leaf");
  openssl(dir, "req", "-new", "-key", "leaf-key.pem", "-sha256", "-config", "openssl.cnf",
    "-subj", "/CN=sandstorm master", "-out", "leaf.csr");
  openssl(dir, "x509", "-req", "-in", "leaf.csr", "-CA", `${ca}.pem`, "-CAkey", `${ca}-key.pem`,
    "-CAcreateserial", "-sha256", "-days", "2", "-extfile", "openssl.cnf", "-extensions", "leaf", "-out", "leaf.pem");
}

describe("verifying a master under its own CA", () => {
  let dir: string;
  let server: Server;
  let baseUrl: string;
  let caFile: string;
  let otherCaFile: string;
  let seen: Array<{ method?: string; url?: string; auth?: string; body: string }>;
  const envCa = process.env.SANDSTORM_CA;

  beforeAll(async () => {
    dir = mkdtempSync(join(tmpdir(), "sandstorm-tls-"));
    writeFileSync(join(dir, "openssl.cnf"), OPENSSL_CONFIG);
    makeCa(dir, "ca");
    makeCa(dir, "other-ca");
    issueLeaf(dir, "ca");
    caFile = join(dir, "ca.pem");
    otherCaFile = join(dir, "other-ca.pem");
    server = createServer(
      {
        key: readFileSync(join(dir, "leaf-key.pem")),
        cert: readFileSync(join(dir, "leaf.pem")),
      },
      (req, res) => {
        let body = "";
        req.on("data", (chunk) => (body += chunk));
        req.on("end", () => {
          seen.push({ method: req.method, url: req.url, auth: req.headers.authorization, body });
          if (req.url === "/v1/snapshots") {
            res.writeHead(200, { "content-type": "application/json" });
            res.end(JSON.stringify([{ tag: "py" }]));
          } else if (req.url === "/v1/sandboxes/sb-slow") {
            // Never answers: the client has to give up on its own.
          } else if (req.method === "DELETE") {
            res.writeHead(204);
            res.end();
          } else {
            res.writeHead(404, { "content-type": "application/json" });
            res.end(JSON.stringify({ error: "not found" }));
          }
        });
      },
    );
    await new Promise<void>((done) => server.listen(0, "127.0.0.1", done));
    baseUrl = `https://127.0.0.1:${(server.address() as AddressInfo).port}`;
  });

  afterAll(async () => {
    server.closeAllConnections();
    await new Promise((done) => server.close(done));
    rmSync(dir, { recursive: true, force: true });
  });

  beforeEach(() => {
    seen = [];
    delete process.env.SANDSTORM_CA;
  });

  afterEach(() => {
    if (envCa === undefined) delete process.env.SANDSTORM_CA;
    else process.env.SANDSTORM_CA = envCa;
    vi.unstubAllEnvs();
  });

  it("reaches it with caFile, and sends the token", async () => {
    const c = new Controller({ baseUrl, caFile, token: "api-token" });
    expect(await c.listSnapshots()).toEqual([{ tag: "py" }]);
    expect(seen[0]!.auth).toBe("Bearer api-token");
  });

  it("takes SANDSTORM_CA from the environment", async () => {
    process.env.SANDSTORM_CA = caFile;
    const c = new Controller({ baseUrl });
    expect(c.caFile).toBe(caFile);
    expect(await c.listSnapshots()).toEqual([{ tag: "py" }]);
  });

  it("prefers caFile to SANDSTORM_CA", async () => {
    process.env.SANDSTORM_CA = otherCaFile;
    const c = new Controller({ baseUrl, caFile });
    expect(await c.listSnapshots()).toEqual([{ tag: "py" }]);
  });

  it("refuses it with no CA, before the token is sent", async () => {
    const c = new Controller({ baseUrl, token: "api-token" });
    await expect(c.listSnapshots()).rejects.toThrow();
    expect(seen).toEqual([]);
  });

  it("refuses a master another CA signed, before the token is sent", async () => {
    const c = new Controller({ baseUrl, caFile: otherCaFile, token: "api-token" });
    await expect(c.listSnapshots()).rejects.toThrow(/self.signed|issuer|verify/i);
    expect(seen).toEqual([]);
  });

  it("is not turned off by NODE_TLS_REJECT_UNAUTHORIZED", async () => {
    vi.stubEnv("NODE_TLS_REJECT_UNAUTHORIZED", "0");
    const c = new Controller({ baseUrl, caFile: otherCaFile, token: "api-token" });
    await expect(c.listSnapshots()).rejects.toThrow(/self.signed|issuer|verify/i);
    expect(seen).toEqual([]);
  });

  it("fails on a CA file that is missing", async () => {
    const c = new Controller({ baseUrl, caFile: join(dir, "absent.pem") });
    await expect(c.listSnapshots()).rejects.toThrow(/ENOENT/);
    expect(seen).toEqual([]);
  });

  it("fails on a CA file with no certificate, not falling back to the public roots", async () => {
    const empty = join(dir, "empty.pem");
    writeFileSync(empty, "");
    const c = new Controller({ baseUrl, caFile: empty });
    await expect(c.listSnapshots()).rejects.toThrow(/no certificate/);
    expect(seen).toEqual([]);
  });

  it("notices a CA file that appears after a failed read", async () => {
    const late = join(dir, "late.pem");
    const c = new Controller({ baseUrl, caFile: late });
    await expect(c.listSnapshots()).rejects.toThrow(/ENOENT/);
    writeFileSync(late, readFileSync(caFile));
    expect(await c.listSnapshots()).toEqual([{ tag: "py" }]);
  });

  it("reaches it through Sandbox.create", async () => {
    // Refused by the stand-in, which is the point: the connection verified.
    await expect(Sandbox.create({ baseUrl, caFile, template: "python" })).rejects.toBeInstanceOf(
      ControllerError,
    );
    expect(seen[0]).toMatchObject({ method: "POST", body: JSON.stringify({ template: "python" }) });
  });

  it("sends a body, and raises ControllerError on a refusal", async () => {
    const c = new Controller({ baseUrl, caFile });
    await expect(c.branchSandbox("sb-1", { tag: "t" })).rejects.toBeInstanceOf(ControllerError);
    expect(seen[0]).toMatchObject({
      method: "POST",
      url: "/v1/sandboxes/sb-1/branch",
      body: JSON.stringify({ tag: "t" }),
    });
  });

  it("takes an empty 204", async () => {
    const c = new Controller({ baseUrl, caFile });
    await expect(c.killSandbox("sb-1")).resolves.toBeUndefined();
    expect(seen[0]!.method).toBe("DELETE");
  });

  it("still times out", async () => {
    const c = new Controller({ baseUrl, caFile, timeoutMs: 50 });
    await expect(c.getSandbox("sb-slow")).rejects.toThrow(/abort/i);
  });
});

describe("Controller options", () => {
  const stubFetch = (async () => new Response("[]")) as typeof fetch;

  it("refuses caFile with a custom fetch, which would ignore it", () => {
    expect(
      () => new Controller({ caFile: "ca.pem", fetch: stubFetch }),
    ).toThrow(/caFile/);
  });

  it("uses a custom fetch over SANDSTORM_CA", async () => {
    const orig = process.env.SANDSTORM_CA;
    process.env.SANDSTORM_CA = "/does/not/exist.pem";
    try {
      const c = new Controller({ baseUrl: "https://x", fetch: stubFetch });
      expect(await c.listSnapshots()).toEqual([]);
    } finally {
      if (orig === undefined) delete process.env.SANDSTORM_CA;
      else process.env.SANDSTORM_CA = orig;
    }
  });

  it("does not read the CA file for a plain http master", async () => {
    // As the Python Controller: the CA only means anything over https.
    const seen: string[] = [];
    const orig = globalThis.fetch;
    globalThis.fetch = (async (input: string) => {
      seen.push(input);
      return new Response("[]");
    }) as typeof fetch;
    try {
      const c = new Controller({ baseUrl: "http://x", caFile: "/does/not/exist.pem" });
      expect(await c.listSnapshots()).toEqual([]);
      expect(seen).toEqual(["http://x/v1/snapshots"]);
    } finally {
      globalThis.fetch = orig;
    }
  });
});
