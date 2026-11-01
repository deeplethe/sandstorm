// ---- add a node ---------------------------------------------------------------
// Two ways in, because a node either can or cannot be reached from here. One
// it cannot reach runs a single-use command that dials in; one it can is
// installed listening and handed over with its pairing code. Either way the
// backend is chosen here and carried into the command, and the installer
// checks the machine can run it before it installs anything.
import { useState, type FormEvent, type ReactNode } from "react";
import { createPortal } from "react-dom";
import type { AddedNode, BootstrapView } from "../types";
import { api, ApiError } from "../api";
import { BACKEND_LABEL, bootstrapFields, installNote, listenCommand, pathProblem, type BackendChoice, type InstallOpts } from "../backends";
import { fmt } from "../format";
import { refresh } from "../state";
import { useEscape } from "../components/hooks";

// Field problems are said under the field, before anything is sent; what
// only the master can tell -- it could not reach the node, the node is
// already someone else's -- is said once, with what to try.
const PAIRING = /^[0-9a-f]{64}\.\S+$/i;
const CHOICES: [BackendChoice, string][] = [["auto", "Auto"], ["gvisor", BACKEND_LABEL.gvisor], ["firecracker", BACKEND_LABEL.firecracker]];

function addError(e: unknown): ReactNode {
  const msg = String((e as Error).message || e), status = (e as ApiError).status;
  let title = "The master could not add it.", why = "";
  if (status === 502 || /could not take over/.test(msg)) {
    title = "The master could not reach the node.";
    why = "Check it was installed with SANDSTORM_LISTEN, that it is running (systemctl status sandstorm-node), and that the address and port are open from the master.";
  } else if (status === 409) {
    title = "That node already belongs to a fleet.";
  } else if (/pairing_code/.test(msg)) {
    title = "The master did not accept that pairing code.";
    why = "Copy it again, whole, from /var/lib/sandstorm/pki/node/pairing-code on the node.";
  }
  return <><b>{title}</b>{why && <span className="why">{why}</span>}<details><summary>Details</summary><code>{msg}</code></details></>;
}

export function AddNode({ open, onClose }: { open: boolean; onClose: () => void }) {
  const [listen, setListen] = useState(false);
  const [err, setErr] = useState<ReactNode>("");
  const [mint, setMint] = useState<BootstrapView | null>(null);
  const [addr, setAddr] = useState(""), [code, setCode] = useState("");
  const [addrHint, setAddrHint] = useState(""), [codeHint, setCodeHint] = useState("");
  const [ok, setOk] = useState(""), [busy, setBusy] = useState(false);
  const [backend, setBackend] = useState<BackendChoice>("auto");
  const [fcKernel, setFcKernel] = useState(""), [stateDir, setStateDir] = useState("");
  useEscape(open, onClose);

  const opts: InstallOpts = { backend, fcKernel: fcKernel.trim(), stateDir: stateDir.trim() };
  const kernelHint = pathProblem(opts.fcKernel), dirHint = pathProblem(opts.stateDir);
  const pathsOk = !kernelHint && !dirHint;
  const pairingFile = `${opts.stateDir || "/var/lib/sandstorm"}/pki/node/pairing-code`;
  // A command already made says what was chosen when it was made.
  const choose = (f: () => void) => { f(); setMint(null); };
  const tab = (l: boolean) => { setListen(l); setErr(""); };
  const makeCommand = async () => {
    setErr("");
    try { setMint(await api<BootstrapView>("/v1/node-bootstrap", { ttl_secs: 900, master_url: location.origin, ...bootstrapFields(opts) })); }
    catch (e) { setErr(<><b>Could not make a command.</b><details><summary>Details</summary><code>{String((e as Error).message || e)}</code></details></>); }
  };
  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setErr(""); setOk("");
    const address = addr.trim(), c = code.trim();
    const a = address ? "" : "Where the master can reach the node: a host or IP, with :port if it is not 80.";
    const p = !c
      ? `Run sudo cat ${pairingFile} on the node and paste what it prints.`
      : PAIRING.test(c) ? ""
      : "That is not a pairing code. It is 64 hex characters, a dot, then a secret, all on one line -- the whole of the pairing-code file.";
    setAddrHint(a); setCodeHint(p);
    if (a || p) return;
    setBusy(true);
    try {
      const r = await api<AddedNode>("/v1/nodes", { address, pairing_code: c });
      setCode("");
      setOk(`Added ${r.id} at ${r.address}, room for ${fmt(r.max_sandboxes)} sandboxes.`);
      void refresh(false);
    } catch (x) { setErr(addError(x)); }
    finally { setBusy(false); }
  };

  return createPortal(
    <div className="gate" id="addDlg" hidden={!open} onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <form className="wide" autoComplete="off" onSubmit={submit}>
        <h1>Add a node</h1>
        <button className="x" type="button" aria-label="Close" onClick={onClose}>×</button>
        <div className="tabs" role="tablist">
          <button type="button" role="tab" aria-selected={!listen} onClick={() => tab(false)}>It reaches the master</button>
          <button type="button" role="tab" aria-selected={listen} onClick={() => tab(true)}>The master reaches it</button>
        </div>
        <label id="addBackendLabel">Backend</label>
        <div className="tabs" role="radiogroup" aria-labelledby="addBackendLabel">
          {CHOICES.map(([v, l]) => <button type="button" role="radio" key={v} data-v={v} aria-checked={backend === v} onClick={() => choose(() => setBackend(v))}>{l}</button>)}
        </div>
        <div className="note">{installNote(backend)}</div>
        <div hidden={backend === "gvisor"}>
          <label htmlFor="addKernel">Guest kernel on the node (optional)</label>
          <input id="addKernel" className={kernelHint ? "invalid" : undefined} placeholder="looked for in the work dir and /var/lib/sandstorm/kernels" value={fcKernel} onChange={(e) => choose(() => setFcKernel(e.target.value))} />
          <div className="hint" hidden={!kernelHint}>{kernelHint}</div>
        </div>
        <label htmlFor="addDir">Work dir on the node (optional)</label>
        <input id="addDir" className={dirHint ? "invalid" : undefined} placeholder="/var/lib/sandstorm" value={stateDir} onChange={(e) => choose(() => setStateDir(e.target.value))} />
        <div className="hint" hidden={!dirHint}>{dirHint}</div>
        <div hidden={listen}>
          <p>For a machine the master cannot connect to, behind NAT or a firewall. Run one line on it; it dials in and joins.</p>
          <button type="button" className="go" disabled={!pathsOk} onClick={makeCommand}>Make the command</button>
          <pre hidden={!mint}>{mint?.install_command}</pre>
          <div className="note" hidden={!mint}>
            {mint && <>Single use, valid until {new Date(mint.expires_at * 1000).toLocaleTimeString()}. The node appears below once it has run.
              {mint.first_fetch_is_unverified && <> The first download is not verified (<code>-k</code>); everything after it is checked against the CA fingerprint inside the command.</>}</>}
          </div>
        </div>
        <div hidden={!listen}>
          <p>For a machine the master can connect to. Install the node listening on it, then give the master its address and pairing code.</p>
          <pre>{pathsOk ? listenCommand(location.origin, opts) : "Fix the path above to see the command."}</pre>
          <div className="note">The install prints where the pairing code is: <code>sudo cat {pairingFile}</code></div>
          <label htmlFor="addAddr">Address the master can reach</label>
          <input id="addAddr" className={addrHint ? "invalid" : undefined} placeholder="10.0.0.5 or 10.0.0.5:80" value={addr} onChange={(e) => { setAddr(e.target.value); setAddrHint(""); }} />
          <div className="hint" hidden={!addrHint}>{addrHint}</div>
          <label htmlFor="addCode">Pairing code</label>
          <input id="addCode" className={codeHint ? "invalid" : undefined} type="password" placeholder="64 hex characters, a dot, then the secret" value={code} onChange={(e) => { setCode(e.target.value); setCodeHint(""); }} />
          <div className="hint" hidden={!codeHint}>{codeHint}</div>
          <button type="submit" className="go" disabled={busy}>{busy ? "Adding…" : "Add it"}</button>
          <div className="ok" hidden={!ok}>{ok}</div>
        </div>
        <div className="err">{err}</div>
      </form>
    </div>,
    document.body,
  );
}
