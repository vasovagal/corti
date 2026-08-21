import { useEffect, useMemo, useState, type FormEvent } from "react";
import type { HostedPatchInput, HostedSettingsDto } from "../lib/api";
import {
  filterWordEntries,
  removeWordEntry,
  replaceWordEntry,
  splitBulkEntries,
} from "../lib/hosted";
import { HostedDialog, HostedSwitch } from "./HostedCommon";

interface LanguageActions {
  busy: boolean;
  onSteering: (text: string) => Promise<boolean>;
  onWordBank: (entries: string[]) => Promise<boolean>;
  onPinned: (template: string) => Promise<boolean>;
  onPatch: (patch: HostedPatchInput, success: string) => Promise<boolean>;
}

export function HostedLanguagePreferences({
  settings,
  actions,
}: {
  settings: HostedSettingsDto;
  actions: LanguageActions;
}) {
  return (
    <section aria-labelledby="hosted-language-heading">
      <div className="hosted-section-heading">
        <div>
          <p className="hosted-eyebrow">Prompt inputs</p>
          <h2 id="hosted-language-heading">Language preferences</h2>
        </div>
        <p>Changes are persisted first, then fence the next request in every affected lane.</p>
      </div>
      <div className="hosted-language-grid">
        <SteeringDefault settings={settings} actions={actions} />
        <WordBank settings={settings} actions={actions} />
        <PinnedQuestion settings={settings} actions={actions} />
      </div>
    </section>
  );
}

function SteeringDefault({
  settings,
  actions,
}: {
  settings: HostedSettingsDto;
  actions: LanguageActions;
}) {
  const [draft, setDraft] = useState(settings.default_steering);
  useEffect(() => setDraft(settings.default_steering), [settings.default_steering]);
  const changed = draft !== settings.default_steering;

  function submit(event: FormEvent) {
    event.preventDefault();
    if (!changed) return;
    void actions.onSteering(draft);
  }

  return (
    <article className="card hosted-language-card hosted-steering-card">
      <header className="hosted-card-head">
        <div>
          <h3>Default steering</h3>
          <p>One persistent instruction for live, final, and question requests.</p>
        </div>
        <span className="hosted-revision">rev {settings.control.steering_revision}</span>
      </header>
      <form onSubmit={submit}>
        <label className="settings-field">
          <span>Instruction</span>
          <input
            type="text"
            value={draft}
            maxLength={256 * 1024}
            placeholder="No extra steering"
            onChange={(event) => setDraft(event.target.value)}
          />
        </label>
        <p className="muted small">
          Steering is treated as untrusted user policy, leaves this Mac with selected transcript text, and is
          part of the exact cache key. It never grants tools, files, shell, or web access.
        </p>
        <div className="other-row">
          <button className="btn-primary" type="submit" disabled={!changed || actions.busy}>
            Save default
          </button>
          <button
            className="btn-quiet"
            type="button"
            disabled={!draft || actions.busy}
            onClick={() => setDraft("")}
          >
            Clear field
          </button>
        </div>
      </form>
    </article>
  );
}

function WordBank({
  settings,
  actions,
}: {
  settings: HostedSettingsDto;
  actions: LanguageActions;
}) {
  const entries = settings.word_bank.entries;
  const [remember, setRemember] = useState("");
  const [bulk, setBulk] = useState("");
  const [search, setSearch] = useState("");
  const [editing, setEditing] = useState<string | null>(null);
  const [editValue, setEditValue] = useState("");
  const [confirmClear, setConfirmClear] = useState(false);
  const visible = useMemo(() => filterWordEntries(entries, search), [entries, search]);

  async function addOne(event: FormEvent) {
    event.preventDefault();
    if (!remember.trim()) return;
    if (await actions.onWordBank([...entries, remember])) setRemember("");
  }

  async function addBulk(event: FormEvent) {
    event.preventDefault();
    const additions = splitBulkEntries(bulk);
    if (additions.length === 0) return;
    if (await actions.onWordBank([...entries, ...additions])) setBulk("");
  }

  async function saveEdit(event: FormEvent) {
    event.preventDefault();
    if (!editing || !editValue.trim()) return;
    if (await actions.onWordBank(replaceWordEntry(entries, editing, editValue))) {
      setEditing(null);
      setEditValue("");
    }
  }

  return (
    <article className="card hosted-language-card hosted-word-bank-card">
      <header className="hosted-card-head">
        <div>
          <h3>Unique-word bank</h3>
          <p>Canonical spellings in the stable prompt prefix; Corti never auto-learns provider output.</p>
        </div>
        <span className="hosted-revision">
          {entries.length} / 5,000 · rev {settings.word_bank.revision}
        </span>
      </header>

      <form className="hosted-remember-row" onSubmit={(event) => void addOne(event)}>
        <label htmlFor="hosted-remember-spelling">Spelling to remember</label>
        <div className="other-row">
          <input
            id="hosted-remember-spelling"
            type="text"
            maxLength={128}
            value={remember}
            placeholder="Add one exact spelling"
            onChange={(event) => setRemember(event.target.value)}
          />
          <button className="btn-primary" type="submit" disabled={!remember.trim() || actions.busy}>
            Remember spelling
          </button>
        </div>
      </form>

      <details className="hosted-bulk-editor">
        <summary>Bulk add spellings</summary>
        <form onSubmit={(event) => void addBulk(event)}>
          <label htmlFor="hosted-bulk-spellings">One entry per line</label>
          <textarea
            id="hosted-bulk-spellings"
            rows={5}
            value={bulk}
            placeholder={"Corti\nParakeet\nVagus"}
            onChange={(event) => setBulk(event.target.value)}
          />
          <button
            className="btn-secondary"
            type="submit"
            disabled={splitBulkEntries(bulk).length === 0 || actions.busy}
          >
            Add bulk entries
          </button>
        </form>
      </details>

      <label className="settings-field hosted-bank-search">
        <span>Search saved spellings</span>
        <input
          type="search"
          value={search}
          placeholder="Filter the bank"
          onChange={(event) => setSearch(event.target.value)}
        />
      </label>

      {entries.length === 0 ? (
        <p className="hosted-empty-state">No saved spellings. Provider output is never learned automatically.</p>
      ) : visible.length === 0 ? (
        <p className="hosted-empty-state">No saved spelling matches this search.</p>
      ) : (
        <ul className="hosted-word-list" aria-label="Saved unique-word spellings">
          {visible.map((entry) => (
            <li key={entry}>
              {editing === entry ? (
                <form className="hosted-word-edit" onSubmit={(event) => void saveEdit(event)}>
                  <label className="sr-only" htmlFor="hosted-word-edit-input">
                    Edit saved spelling
                  </label>
                  <input
                    id="hosted-word-edit-input"
                    type="text"
                    maxLength={128}
                    value={editValue}
                    autoFocus
                    onChange={(event) => setEditValue(event.target.value)}
                  />
                  <button className="btn-primary" type="submit" disabled={!editValue.trim() || actions.busy}>
                    Save
                  </button>
                  <button
                    className="btn-quiet"
                    type="button"
                    onClick={() => {
                      setEditing(null);
                      setEditValue("");
                    }}
                  >
                    Cancel
                  </button>
                </form>
              ) : (
                <>
                  <span>{entry}</span>
                  <span className="hosted-word-actions">
                    <button
                      className="btn-quiet"
                      type="button"
                      disabled={actions.busy}
                      aria-label={`Edit ${entry}`}
                      onClick={() => {
                        setEditing(entry);
                        setEditValue(entry);
                      }}
                    >
                      Edit
                    </button>
                    <button
                      className="btn-quiet hosted-remove-word"
                      type="button"
                      disabled={actions.busy}
                      aria-label={`Remove ${entry}`}
                      onClick={() => void actions.onWordBank(removeWordEntry(entries, entry))}
                    >
                      Remove
                    </button>
                  </span>
                </>
              )}
            </li>
          ))}
        </ul>
      )}

      <div className="hosted-bank-footer">
        <p className="muted small">
          The backend normalizes NFC, collapses Unicode whitespace, case-folds duplicates, sorts, validates,
          and revisions the complete bank before use.
        </p>
        <button
          className="btn-danger-subtle"
          type="button"
          disabled={entries.length === 0 || actions.busy}
          onClick={() => setConfirmClear(true)}
        >
          Clear…
        </button>
      </div>

      <HostedDialog
        open={confirmClear}
        title="Clear the unique-word bank?"
        confirmLabel="Clear all spellings"
        confirmTone="danger"
        busy={actions.busy}
        onCancel={() => setConfirmClear(false)}
        onConfirm={() => {
          void actions.onWordBank([]).then((saved) => {
            if (saved) setConfirmClear(false);
          });
        }}
      >
        <p>
          This changes the stable prompt prefix, invalidates affected exact-cache keys, and fences in-flight
          results. Provider-held cache cannot be purged by Corti.
        </p>
      </HostedDialog>
    </article>
  );
}

function PinnedQuestion({
  settings,
  actions,
}: {
  settings: HostedSettingsDto;
  actions: LanguageActions;
}) {
  const [template, setTemplate] = useState("");
  const [acknowledging, setAcknowledging] = useState(false);
  const templatePresent = settings.control.pinned_question_revision > 0;
  const laneReady = settings.control.questions.enabled && Boolean(settings.control.questions.selection.model);

  async function saveTemplate(event: FormEvent) {
    event.preventDefault();
    if (!template.trim()) return;
    if (await actions.onPinned(template)) setTemplate("");
  }

  return (
    <article className="card hosted-language-card hosted-pinned-card">
      <header className="hosted-card-head">
        <div>
          <h3>Pinned question</h3>
          <p>Exactly one saved template; its content is not returned in the Settings DTO.</p>
        </div>
        <span className={templatePresent ? "hosted-configured" : "muted"}>
          {templatePresent ? "Template saved" : "No template"}
        </span>
      </header>

      <form onSubmit={(event) => void saveTemplate(event)}>
        <label className="settings-field">
          <span>{templatePresent ? "Replace saved template" : "New template"}</span>
          <input
            type="text"
            value={template}
            maxLength={32 * 1024}
            placeholder={templatePresent ? "Enter a complete replacement" : "Ask about the current transcript"}
            onChange={(event) => setTemplate(event.target.value)}
          />
        </label>
        <div className="other-row">
          <button className="btn-primary" type="submit" disabled={!template.trim() || actions.busy}>
            {templatePresent ? "Replace template" : "Save template"}
          </button>
          <button
            className="btn-quiet"
            type="button"
            disabled={!templatePresent || actions.busy}
            onClick={() => void actions.onPinned("")}
          >
            Clear saved template
          </button>
        </div>
      </form>

      <HostedSwitch
        label="Run pinned question automatically"
        description={
          settings.control.pinned_auto_enabled
            ? "Acknowledged; meaningful transcript progress can trigger another run."
            : "Off by default; enabling requires a repeated-cost acknowledgement."
        }
        checked={settings.control.pinned_auto_enabled}
        disabled={actions.busy || (!settings.control.pinned_auto_enabled && (!templatePresent || !laneReady))}
        onChange={(enabled) => {
          if (enabled) {
            setAcknowledging(true);
          } else {
            void actions.onPatch(
              { kind: "set_pinned_auto", enabled: false, acknowledged: false },
              "Automatic pinned questions are off.",
            );
          }
        }}
      />
      {!laneReady && (
        <p className="muted small">Enable the Questions lane with an exact catalog model before auto-run.</p>
      )}
      <p className="muted small">
        A run becomes eligible after 40 new word tokens or 30 seconds of covered speech, then a 750 ms quiet
        period. Edits debounce for 500 ms and coalesce at most one dirty rerun.
      </p>

      <HostedDialog
        open={acknowledging}
        title="Allow repeated paid questions?"
        confirmLabel="Acknowledge repeated cost"
        busy={actions.busy}
        onCancel={() => setAcknowledging(false)}
        onConfirm={() => {
          void actions.onPatch(
            { kind: "set_pinned_auto", enabled: true, acknowledged: true },
            "Automatic pinned questions enabled.",
          ).then((saved) => {
            if (saved) setAcknowledging(false);
          });
        }}
      >
        <p>
          Each meaningful transcript update can create another paid provider request. An exact local cache hit
          may avoid a provider request, but is not guaranteed. Cancellation after dispatch may still be billed.
        </p>
        <p>
          Automatic answers use the Questions lane's separately selected provider and model. Turning on this
          switch does not change Master or that lane.
        </p>
      </HostedDialog>
    </article>
  );
}
