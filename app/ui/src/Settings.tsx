import { useEffect, useState, type KeyboardEvent } from "react";
import { Models } from "./settings/Models";
import { Paths } from "./settings/Paths";
import { Transcription } from "./settings/Transcription";
import HostedPreferences from "./settings/HostedPreferences";
import {
  getBackends,
  getConfig,
  setConfig,
  type BackendInfo,
  type SettingsDto,
} from "./lib/api";

type SettingsSection = "transcription" | "hosted" | "storage";

function initialSection(): SettingsSection {
  const requested = new URLSearchParams(window.location.search).get("section");
  return requested === "hosted" || requested === "storage" ? requested : "transcription";
}

// Transcription/storage retain their explicit bottom Save. Hosted rewrite has a separate persisted document
// and uses immediate, revision-checked commands so connecting a provider can never enable egress by accident.
export default function Settings() {
  const [section, setSection] = useState<SettingsSection>(initialSection);
  const [cfg, setCfg] = useState<SettingsDto | null>(null);
  const [backends, setBackends] = useState<BackendInfo[]>([]);
  const [status, setStatus] = useState("");
  const [saving, setSaving] = useState(false);

  useEffect(() => {
    document.title = "Settings — Corti";
    Promise.all([getConfig(), getBackends()])
      .then(([nextConfig, nextBackends]) => {
        setCfg(nextConfig);
        setBackends(nextBackends);
      })
      .catch((error) => setStatus(`Failed to load transcription settings: ${String(error)}`));
  }, []);

  function chooseSection(next: SettingsSection) {
    setSection(next);
    const url = new URL(window.location.href);
    url.searchParams.set("section", next);
    window.history.replaceState(null, "", url);
  }

  function moveTab(event: KeyboardEvent<HTMLElement>) {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    event.preventDefault();
    const sections: SettingsSection[] = ["transcription", "hosted", "storage"];
    const current = sections.indexOf(section);
    const next =
      event.key === "Home"
        ? sections[0]
        : event.key === "End"
          ? sections[sections.length - 1]
          : sections[(current + (event.key === "ArrowRight" ? 1 : -1) + sections.length) % sections.length];
    chooseSection(next);
    document.getElementById(`settings-tab-${next}`)?.focus();
  }

  async function save() {
    if (!cfg) return;
    setSaving(true);
    setStatus("");
    try {
      await setConfig(cfg);
      setStatus("Saved. Changes apply to the next recording.");
    } catch (error) {
      setStatus(`Save failed: ${String(error)}`);
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="app settings-app">
      <header className="app-header">
        <h1>Settings</h1>
        <p className="subtitle">
          Transcription, hosted rewrite, and local storage keep separate, truthful control boundaries.
        </p>
      </header>

      <nav
        className="tabs settings-tabs"
        role="tablist"
        aria-label="Settings sections"
        onKeyDown={moveTab}
      >
        <button
          id="settings-tab-transcription"
          className={`tab${section === "transcription" ? " tab-active" : ""}`}
          type="button"
          role="tab"
          aria-selected={section === "transcription"}
          aria-controls="settings-panel"
          tabIndex={section === "transcription" ? 0 : -1}
          onClick={() => chooseSection("transcription")}
        >
          Transcription
        </button>
        <button
          id="settings-tab-hosted"
          className={`tab${section === "hosted" ? " tab-active" : ""}`}
          type="button"
          role="tab"
          aria-selected={section === "hosted"}
          aria-controls="settings-panel"
          tabIndex={section === "hosted" ? 0 : -1}
          onClick={() => chooseSection("hosted")}
        >
          Hosted rewrite
        </button>
        <button
          id="settings-tab-storage"
          className={`tab${section === "storage" ? " tab-active" : ""}`}
          type="button"
          role="tab"
          aria-selected={section === "storage"}
          aria-controls="settings-panel"
          tabIndex={section === "storage" ? 0 : -1}
          onClick={() => chooseSection("storage")}
        >
          Storage &amp; local models
        </button>
      </nav>

      <main
        id="settings-panel"
        className="tab-content settings-tabpanel"
        role="tabpanel"
        aria-labelledby={`settings-tab-${section}`}
      >
        {section === "hosted" ? (
          <HostedPreferences />
        ) : !cfg ? (
          <section className="card" aria-live="polite">
            <p className="muted">{status || "Loading transcription settings…"}</p>
          </section>
        ) : section === "transcription" ? (
          <>
            <Transcription cfg={cfg} backends={backends} onChange={setCfg} />
            <SettingsSave saving={saving} status={status} onSave={() => void save()} />
          </>
        ) : (
          <>
            <Paths cfg={cfg} onChange={setCfg} />
            <Models asrEngine={cfg.local_asr_engine} />
            <p className="callout small">
              Hosted rewrite models are paid provider catalog entries. They are never downloaded into the
              local models directory shown here.
            </p>
            <SettingsSave saving={saving} status={status} onSave={() => void save()} />
          </>
        )}
      </main>
    </div>
  );
}

function SettingsSave({
  saving,
  status,
  onSave,
}: {
  saving: boolean;
  status: string;
  onSave: () => void;
}) {
  return (
    <div className="settings-actions" aria-live="polite">
      <button className="btn-add" type="button" onClick={onSave} disabled={saving}>
        {saving ? "Saving…" : "Save"}
      </button>
      {status && <span className="muted small">{status}</span>}
    </div>
  );
}
