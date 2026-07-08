import { useEffect, useState } from "react";
import { Models } from "./settings/Models";
import { Paths } from "./settings/Paths";
import { Transcription } from "./settings/Transcription";
import {
  getBackends,
  getConfig,
  setConfig,
  type BackendInfo,
  type SettingsDto,
} from "./lib/api";

// corti's Settings window. Sections edit the persisted config; Save writes config.toml and re-applies on the
// next recording. PR4 adds the Path + Model sections to the same container.
export default function Settings() {
  const [cfg, setCfg] = useState<SettingsDto | null>(null);
  const [backends, setBackends] = useState<BackendInfo[]>([]);
  const [status, setStatus] = useState("");
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    document.title = "Settings — Corti";
    Promise.all([getConfig(), getBackends()])
      .then(([c, b]) => {
        setCfg(c);
        setBackends(b);
      })
      .catch((e) => setStatus(`Failed to load settings: ${e}`));
  }, []);

  if (!cfg) {
    return (
      <div className="app">
        <header className="app-header">
          <h1>Settings</h1>
        </header>
        <main className="tab-content">
          <p className="muted">{status || "Loading…"}</p>
        </main>
      </div>
    );
  }

  async function save() {
    if (!cfg) return;
    setSaving(true);
    setStatus("");
    try {
      await setConfig(cfg);
      setStatus("Saved.");
    } catch (e) {
      setStatus(`Save failed: ${e}`);
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="app">
      <header className="app-header">
        <h1>Settings</h1>
        <p className="subtitle">
          corti's configuration — persisted to <code>~/.local/share/corti/config.toml</code>.
        </p>
      </header>

      <main className="tab-content">
        <Transcription cfg={cfg} backends={backends} onChange={setCfg} />
        <Paths cfg={cfg} onChange={setCfg} />

        <div className="settings-actions">
          <button className="btn-add" onClick={save} disabled={saving}>
            {saving ? "Saving…" : "Save"}
          </button>
          {status && <span className="muted small">{status}</span>}
        </div>

        <Models />
      </main>
    </div>
  );
}
