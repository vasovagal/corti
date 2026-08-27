import { useCallback, useEffect, useState, type KeyboardEvent } from "react";
import { Models } from "./settings/Models";
import { Paths } from "./settings/Paths";
import { Transcription } from "./settings/Transcription";
import HostedPreferences, {
  type HostedPreferencesSection,
} from "./settings/HostedPreferences";
import {
  getBackends,
  getConfig,
  onPreferencesNavigationRequested,
  setConfig,
  takePreferencesSectionRequest,
  type BackendInfo,
  type SettingsDto,
} from "./lib/api";

type SettingsSection =
  | "transcription"
  | "hosted"
  | "hosted-provider"
  | "hosted-routing"
  | "hosted-language"
  | "hosted-advanced"
  | "storage";

type SettingsSectionSpec = {
  id: SettingsSection;
  title: string;
  heading: string;
  description: string;
  icon: "microphone" | "sparkles" | "connection" | "routing" | "language" | "diagnostics" | "storage";
};

const SETTINGS_GROUPS: { label: string; sections: SettingsSectionSpec[] }[] = [
  {
    label: "Audio",
    sections: [
      {
        id: "transcription",
        title: "Transcription",
        heading: "Transcription",
        description: "Choose how Corti turns recordings into private, durable text.",
        icon: "microphone",
      },
    ],
  },
  {
    label: "Hosted rewrite",
    sections: [
      {
        id: "hosted",
        title: "Overview",
        heading: "Hosted rewrite",
        description: "Optional paid text cleanup after local transcription, with an explicit privacy boundary.",
        icon: "sparkles",
      },
      {
        id: "hosted-provider",
        title: "Provider",
        heading: "Provider connection",
        description: "Set up the one hosted service you plan to use. You can switch providers at any time.",
        icon: "connection",
      },
      {
        id: "hosted-routing",
        title: "Rewrite modes",
        heading: "Rewrite modes",
        description: "Start with final cleanup, then add live cleanup or transcript questions only if useful.",
        icon: "routing",
      },
      {
        id: "hosted-language",
        title: "Language & vocabulary",
        heading: "Language & vocabulary",
        description: "Teach Corti preferred spellings and reusable guidance without changing transcription itself.",
        icon: "language",
      },
      {
        id: "hosted-advanced",
        title: "Diagnostics",
        heading: "Diagnostics & guarantees",
        description: "Control content-free diagnostics and review what provider catalogs can—and cannot—promise.",
        icon: "diagnostics",
      },
    ],
  },
  {
    label: "On this Mac",
    sections: [
      {
        id: "storage",
        title: "Storage & models",
        heading: "Storage & local models",
        description: "Manage recording retention, local paths, and offline transcription models.",
        icon: "storage",
      },
    ],
  },
];

const ALL_SECTIONS = SETTINGS_GROUPS.flatMap((group) => group.sections);
const HOSTED_SECTIONS: Partial<Record<SettingsSection, HostedPreferencesSection>> = {
  hosted: "overview",
  "hosted-provider": "provider",
  "hosted-routing": "routing",
  "hosted-language": "language",
  "hosted-advanced": "advanced",
};
const SECTION_FOR_HOSTED_AREA: Record<HostedPreferencesSection, SettingsSection> = {
  overview: "hosted",
  provider: "hosted-provider",
  routing: "hosted-routing",
  language: "hosted-language",
  advanced: "hosted-advanced",
};

function initialSection(): SettingsSection {
  const requested = new URLSearchParams(window.location.search).get("section");
  return ALL_SECTIONS.some((section) => section.id === requested)
    ? (requested as SettingsSection)
    : "transcription";
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
    document.title = "Preferences — Corti";
    Promise.all([getConfig(), getBackends()])
      .then(([nextConfig, nextBackends]) => {
        setCfg(nextConfig);
        setBackends(nextBackends);
      })
      .catch((error) => setStatus(`Failed to load transcription settings: ${String(error)}`));
  }, []);

  const chooseSection = useCallback((next: SettingsSection) => {
    setSection(next);
    const url = new URL(window.location.href);
    url.searchParams.set("section", next);
    window.history.replaceState(null, "", url);
  }, []);

  useEffect(() => {
    let active = true;
    let stop: (() => void) | undefined;
    const installPendingDestination = async () => {
      try {
        const requested = await takePreferencesSectionRequest();
        if (
          active &&
          requested &&
          ALL_SECTIONS.some((candidate) => candidate.id === requested)
        ) {
          chooseSection(requested as SettingsSection);
        }
      } catch {
        // A newly opened window still receives the target through its query string.
      }
    };
    onPreferencesNavigationRequested(() => void installPendingDestination())
      .then((unlisten) => {
        if (active) stop = unlisten;
        else unlisten();
      })
      .catch(() => {
        // The backend-owned mount-time read below still repairs an event-subscription failure.
      });
    void installPendingDestination();
    return () => {
      active = false;
      stop?.();
    };
  }, [chooseSection]);

  function moveSection(event: KeyboardEvent<HTMLElement>) {
    if (!["ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) {
      return;
    }
    event.preventDefault();
    const sections = ALL_SECTIONS.map((candidate) => candidate.id);
    const current = sections.indexOf(section);
    const forward = event.key === "ArrowDown" || event.key === "ArrowRight";
    const next =
      event.key === "Home"
        ? sections[0]
        : event.key === "End"
          ? sections[sections.length - 1]
          : sections[(current + (forward ? 1 : -1) + sections.length) % sections.length];
    chooseSection(next);
    document.getElementById(`settings-section-${next}`)?.focus();
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

  const active = ALL_SECTIONS.find((candidate) => candidate.id === section) ?? ALL_SECTIONS[0];
  const hostedSection = HOSTED_SECTIONS[section];

  return (
    <div className="settings-app">
      <aside className="settings-sidebar">
        <header className="settings-sidebar-header">
          <span className="settings-app-mark" aria-hidden="true">
            C
          </span>
          <div>
            <span className="settings-product-name">Corti</span>
            <h1>Preferences</h1>
          </div>
        </header>

        <nav
          className="settings-nav"
          aria-label="Preference sections"
          onKeyDown={moveSection}
        >
          <div className="settings-nav-groups">
            {SETTINGS_GROUPS.map((group) => (
              <div className="settings-nav-group" key={group.label}>
                <p className="settings-nav-group-label">{group.label}</p>
                <div className="settings-nav-list">
                  {group.sections.map((candidate) => {
                    const selected = candidate.id === section;
                    return (
                      <button
                        id={`settings-section-${candidate.id}`}
                        className={`settings-nav-item${selected ? " settings-nav-item-active" : ""}`}
                        type="button"
                        aria-current={selected ? "page" : undefined}
                        onClick={() => chooseSection(candidate.id)}
                        key={candidate.id}
                      >
                        <span className="settings-nav-icon">
                          <SettingsSectionIcon name={candidate.icon} />
                        </span>
                        <span>{candidate.title}</span>
                      </button>
                    );
                  })}
                </div>
              </div>
            ))}
          </div>
        </nav>

        <p className="settings-sidebar-note">
          Transcription and storage save together. Hosted rewrite saves each change immediately and never
          enables text egress just by connecting a provider.
        </p>
      </aside>

      <main className="settings-main">
        <div className="settings-content">
          <header className="settings-content-header">
            <h2>{active.heading}</h2>
            <p>{active.description}</p>
          </header>

          <div className="settings-panel">
            {hostedSection ? (
              <HostedPreferences
                section={hostedSection}
                onNavigate={(area) => chooseSection(SECTION_FOR_HOSTED_AREA[area])}
              />
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
          </div>
        </div>
      </main>
    </div>
  );
}

function SettingsSectionIcon({ name }: { name: SettingsSectionSpec["icon"] }) {
  const common = {
    viewBox: "0 0 24 24",
    width: 16,
    height: 16,
    fill: "none",
    stroke: "currentColor",
    strokeWidth: 1.9,
    strokeLinecap: "round" as const,
    strokeLinejoin: "round" as const,
    "aria-hidden": true,
  };
  switch (name) {
    case "microphone":
      return (
        <svg {...common}>
          <rect x="9" y="3" width="6" height="11" rx="3" />
          <path d="M6.5 11.5a5.5 5.5 0 0 0 11 0M12 17v4M9 21h6" />
        </svg>
      );
    case "sparkles":
      return (
        <svg {...common}>
          <path d="m12 3 1.35 4.15L17.5 8.5l-4.15 1.35L12 14l-1.35-4.15L6.5 8.5l4.15-1.35L12 3Z" />
          <path d="m18.5 14 .7 2.3 2.3.7-2.3.7-.7 2.3-.7-2.3-2.3-.7 2.3-.7.7-2.3Z" />
        </svg>
      );
    case "connection":
      return (
        <svg {...common}>
          <path d="M8.5 15.5 6 18a3.5 3.5 0 1 1-5-5l3-3a3.5 3.5 0 0 1 5 0" />
          <path d="m15.5 8.5 2.5-2.5a3.5 3.5 0 1 1 5 5l-3 3a3.5 3.5 0 0 1-5 0M8 16l8-8" />
        </svg>
      );
    case "routing":
      return (
        <svg {...common}>
          <path d="M4 5h6a4 4 0 0 1 4 4v10M18 15l-4 4-4-4M4 12h4" />
          <circle cx="4" cy="5" r="1.5" />
          <circle cx="4" cy="12" r="1.5" />
        </svg>
      );
    case "language":
      return (
        <svg {...common}>
          <path d="M4 5h9M8.5 3v2c0 4-2 7-5 9M6 10c1.2 1.6 2.8 2.8 5 3.7M14 20l3.5-9 3.5 9M15.2 17h4.6" />
        </svg>
      );
    case "diagnostics":
      return (
        <svg {...common}>
          <path d="M4 19V9M10 19V5M16 19v-7M22 19V3M2 19h22" />
        </svg>
      );
    case "storage":
      return (
        <svg {...common}>
          <ellipse cx="12" cy="5" rx="8" ry="3" />
          <path d="M4 5v7c0 1.7 3.6 3 8 3s8-1.3 8-3V5M4 12v7c0 1.7 3.6 3 8 3s8-1.3 8-3v-7" />
        </svg>
      );
  }
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
      {status && <span className="muted small">{status}</span>}
      <button className="btn-primary" type="button" onClick={onSave} disabled={saving}>
        {saving ? "Saving…" : "Save changes"}
      </button>
    </div>
  );
}
