// ---- a template from an image ---------------------------------------------------
// Answered at once: the pull goes on in the table behind this dialog.
import { useEffect, useState, type FormEvent } from "react";
import { createPortal } from "react-dom";
import { api } from "../api";
import { useEscape } from "../components/hooks";

export function FromImage({ open, onClose, onStarted }: { open: boolean; onClose: () => void; onStarted: () => void }) {
  const [name, setName] = useState(""), [image, setImage] = useState("");
  const [memory, setMemory] = useState("512"), [port, setPort] = useState(""), [args, setArgs] = useState("");
  const [err, setErr] = useState(""), [busy, setBusy] = useState(false);
  useEscape(open, onClose);
  useEffect(() => { if (open) { setErr(""); setBusy(false); } }, [open]);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setErr(""); setBusy(true);
    const body: { name: string; image: string; memory_max_mib: number; port?: number; args?: string[] } = { name: name.trim(), image: image.trim(), memory_max_mib: Number(memory) || 512 };
    if (port) body.port = Number(port);
    if (args.trim()) body.args = args.trim().split(/\s+/);
    try {
      await api("/v1/templates/from-image", body);
      setName(""); setImage(""); setPort(""); setArgs("");
      onStarted(); onClose();
    } catch (x) { setErr(String((x as Error).message || x)); setBusy(false); }
  };

  return createPortal(
    <div className="gate" id="tplDlg" hidden={!open} onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <form className="wide" autoComplete="off" onSubmit={submit}>
        <h1>Template from an image</h1>
        <button className="x" type="button" aria-label="Close" onClick={onClose}>×</button>
        <p>Every node pulls it. It runs what the image says to run, unless you say otherwise. Public images only for now.</p>
        <label htmlFor="tplImage">Image</label><input id="tplImage" required placeholder="python:3.12-slim" value={image} onChange={(e) => setImage(e.target.value)} />
        <label htmlFor="tplName">Template name</label><input id="tplName" required placeholder="python" value={name} onChange={(e) => setName(e.target.value)} />
        <label htmlFor="tplMemory">Memory limit, MiB</label><input id="tplMemory" type="number" min="16" value={memory} onChange={(e) => setMemory(e.target.value)} />
        <label htmlFor="tplPort">Port it serves on</label><input id="tplPort" type="number" min="1" max="65535" placeholder="the one the image exposes" value={port} onChange={(e) => setPort(e.target.value)} />
        <label htmlFor="tplArgs">Command</label><input id="tplArgs" placeholder="the image's" value={args} onChange={(e) => setArgs(e.target.value)} />
        <div className="note">On Firecracker nodes the image needs bash and python3.</div>
        <button type="submit" className="go" disabled={busy}>Pull it</button>
        <div className="err">{err}</div>
      </form>
    </div>,
    document.body,
  );
}
