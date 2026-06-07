import { useState } from "react";
import { Disclaimer } from "./components/Disclaimer";
import { Tabs, type TabId } from "./components/Tabs";
import { Legality } from "./tabs/Legality";
import { ConsentCalculator } from "./tabs/ConsentCalculator";
import { EthicsVsLegality } from "./tabs/EthicsVsLegality";
import { CulturalNorms } from "./tabs/CulturalNorms";
import { Voiceprinting } from "./tabs/Voiceprinting";
import { SOURCES } from "./lib/data";

export default function App() {
  const [tab, setTab] = useState<TabId>("legality");

  return (
    <div className="app">
      <header className="app-header">
        <h1>Ethics &amp; Legality Guide</h1>
        <p className="subtitle">Recording people responsibly — the law, the ethics, and the norms.</p>
      </header>

      <Disclaimer />
      <Tabs active={tab} onChange={setTab} />

      <main className="tab-content" role="tabpanel">
        {tab === "legality" && <Legality />}
        {tab === "calculator" && <ConsentCalculator />}
        {tab === "ethics" && <EthicsVsLegality />}
        {tab === "culture" && <CulturalNorms />}
        {tab === "voiceprint" && <Voiceprinting />}
      </main>

      <footer className="app-footer">
        <h3>Sources</h3>
        <p className="muted small">
          Links are shown as text so they don't navigate this window — copy them into a browser.
        </p>
        <ul className="sources">
          {SOURCES.map((s) => (
            <li key={s.id}>
              <span className="source-title">{s.title}</span>
              <span className="source-cat">{s.category}</span>
              <code className="source-url">{s.url}</code>
            </li>
          ))}
        </ul>
        <p className="footer-note">Informational only · not legal advice · verify locally before recording.</p>
      </footer>
    </div>
  );
}
