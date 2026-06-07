export type TabId = "legality" | "calculator" | "ethics" | "culture" | "voiceprint";

const TABS: { id: TabId; label: string }[] = [
  { id: "legality", label: "Legality" },
  { id: "calculator", label: "Consent Calculator" },
  { id: "ethics", label: "Ethics vs. Legality" },
  { id: "culture", label: "Cultural Norms" },
  { id: "voiceprint", label: "Voiceprinting" },
];

export function Tabs({ active, onChange }: { active: TabId; onChange: (t: TabId) => void }) {
  return (
    <nav className="tabs" role="tablist" aria-label="Guide sections">
      {TABS.map((t) => (
        <button
          key={t.id}
          role="tab"
          aria-selected={active === t.id}
          className={active === t.id ? "tab tab-active" : "tab"}
          onClick={() => onChange(t.id)}
        >
          {t.label}
        </button>
      ))}
    </nav>
  );
}
