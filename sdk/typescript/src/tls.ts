import type { Agent } from "node:https";

/**
 * A `fetch` that verifies `https://` against the PEM bundle at `caFile`
 * and nothing else.
 *
 * Node's global fetch has no option for this. Its `dispatcher` wants an
 * Agent from the `undici` package, a dependency this SDK does not otherwise
 * have, and `NODE_EXTRA_CA_CERTS` is read once at startup and adds to the
 * public roots rather than replacing them. `node:https` takes `ca` directly,
 * and replacing is what we want: the master's CA is the only issuer that
 * should be able to sign for it.
 *
 * Only the part of fetch `Controller` uses. Imported lazily so that the
 * SDK still loads where there is no `node:https`, which is also why a bad
 * `caFile` surfaces on the first request rather than in the constructor.
 */
export function fetchWithCa(caFile: string): typeof fetch {
  let agent: Promise<Agent> | undefined;
  return (async (input, init) => {
    // Not kept on failure: the file may yet appear, and a long-lived
    // Controller should not need a restart to notice.
    agent ??= loadAgent(caFile).catch((e: unknown) => {
      agent = undefined;
      throw e;
    });
    const { request } = await import("node:https");
    const ready = await agent;
    return new Promise<Response>((resolve, reject) => {
      const req = request(
        String(input),
        {
          agent: ready,
          method: init?.method,
          headers: init?.headers as Record<string, string> | undefined,
          signal: init?.signal ?? undefined,
        },
        (res) => {
          const chunks: Buffer[] = [];
          res.on("data", (chunk: Buffer) => chunks.push(chunk));
          res.on("error", reject);
          res.on("end", () => {
            const body = Buffer.concat(chunks);
            // A Response refuses a body, even an empty one, on a 204.
            resolve(
              new Response(body.length > 0 ? body : null, {
                status: res.statusCode ?? 502,
              }),
            );
          });
        },
      );
      req.on("error", reject);
      req.end(init?.body as string | undefined);
    });
  }) as typeof fetch;
}

async function loadAgent(caFile: string): Promise<Agent> {
  const [{ readFile }, { Agent }] = await Promise.all([
    import("node:fs/promises"),
    import("node:https"),
  ]);
  const ca = await readFile(caFile, "utf8");
  // An empty `ca` would quietly mean Node's public roots.
  if (!ca.includes("BEGIN CERTIFICATE")) {
    throw new Error(`Controller: no certificate in CA file ${caFile}`);
  }
  // rejectUnauthorized stated because its default is not fixed: it reads
  // NODE_TLS_REJECT_UNAUTHORIZED, and this is not to be turned off.
  return new Agent({ ca, rejectUnauthorized: true, keepAlive: true });
}
