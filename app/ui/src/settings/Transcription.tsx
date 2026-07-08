import { useEffect, useState } from "react";
import {
  getAwsStatus,
  getEmbeddingModels,
  verifyAws,
  type AwsStatus,
  type BackendInfo,
  type ModelStatus,
  type SettingsDto,
} from "../lib/api";

interface Props {
  cfg: SettingsDto;
  backends: BackendInfo[];
  onChange: (next: SettingsDto) => void;
}

// Common AWS commercial regions for the selector; the current value is injected if it isn't in the list.
const AWS_REGIONS = [
  "us-east-1",
  "us-east-2",
  "us-west-1",
  "us-west-2",
  "ca-central-1",
  "eu-west-1",
  "eu-west-2",
  "eu-central-1",
  "eu-north-1",
  "ap-south-1",
  "ap-southeast-1",
  "ap-southeast-2",
  "ap-northeast-1",
  "ap-northeast-2",
  "sa-east-1",
];

// The Transcription section: a direct editor over the persisted AppConfig fields. A field pinned by a
// `CORTI_*` env var is shown disabled with a "managed by env" badge. The AWS sub-section additionally
// reflects the AWS-native credential chain (profile/region from ~/.aws or the environment).
export function Transcription({ cfg, backends, onChange }: Props) {
  const isEnv = (field: string) => cfg.env_managed.includes(field);
  const set = <K extends keyof SettingsDto>(key: K, value: SettingsDto[K]) =>
    onChange({ ...cfg, [key]: value });

  const envBadge = (field: string) =>
    isEnv(field) ? <span className="env-badge">managed by env</span> : null;

  // AWS credential-chain status (profiles, env reflections, lock flags). Null when unavailable (e.g. a
  // build without the AWS backend) — the section then falls back to the plain bucket/language inputs.
  const [aws, setAws] = useState<AwsStatus | null>(null);
  const [verifying, setVerifying] = useState(false);
  const [verifyMsg, setVerifyMsg] = useState<string | null>(null);

  // The selectable English speaker-embedding models for the far-end-diarization dropdown (with their
  // installed/size state). Empty on a build without the local backend — the dropdown then renders nothing.
  const [embeddingModels, setEmbeddingModels] = useState<ModelStatus[]>([]);

  useEffect(() => {
    getAwsStatus()
      .then(setAws)
      .catch(() => setAws(null));
    getEmbeddingModels()
      .then(setEmbeddingModels)
      .catch(() => setEmbeddingModels([]));
  }, []);

  async function testCredentials() {
    setVerifying(true);
    setVerifyMsg(null);
    try {
      const id = await verifyAws();
      setVerifyMsg(`Authenticated — account ${id.account ?? "?"}${id.arn ? `, ${id.arn}` : ""}`);
    } catch (e) {
      setVerifyMsg(`Failed: ${e}`);
    } finally {
      setVerifying(false);
    }
  }

  const profileOptions =
    aws && aws.selected_profile && !aws.profiles.includes(aws.selected_profile)
      ? [aws.selected_profile, ...aws.profiles]
      : (aws?.profiles ?? []);

  const currentRegion = aws?.region_locked ? aws.env_region : cfg.aws_region;
  const regionOptions =
    currentRegion && !AWS_REGIONS.includes(currentRegion)
      ? [currentRegion, ...AWS_REGIONS]
      : AWS_REGIONS;

  return (
    <section className="card">
      <h2>Transcription</h2>

      <div className="settings-field">
        <label>Backend{envBadge("transcribe_backend")}</label>
        <div className="radio-row">
          {backends.map((b) => (
            <label key={b.id} className="radio">
              <input
                type="radio"
                name="backend"
                value={b.id}
                checked={cfg.transcribe_backend === b.id}
                disabled={!b.compiled_in || isEnv("transcribe_backend")}
                onChange={() => set("transcribe_backend", b.id)}
              />
              {b.label}
              {!b.compiled_in && " (not in this build)"}
            </label>
          ))}
        </div>
      </div>

      {cfg.transcribe_backend === "aws" && (
        <>
          {aws && (
            <>
              <p className="callout">{aws.source}</p>
              {aws.env_access_key_id && (
                <p className="muted small">
                  Access key {aws.env_access_key_id} · secret{" "}
                  {aws.env_has_secret ? "set" : "missing"}
                  {aws.env_session_token ? " · session token set" : ""}
                </p>
              )}

              <div className="settings-field">
                <label>Profile</label>
                <select
                  className="jselect"
                  value={aws.profile_locked ? (aws.selected_profile ?? "") : (cfg.aws_profile ?? "")}
                  disabled={aws.profile_locked}
                  onChange={(e) => set("aws_profile", e.target.value || null)}
                >
                  <option value="">Default / credential chain</option>
                  {profileOptions.map((p) => (
                    <option key={p} value={p}>
                      {p}
                    </option>
                  ))}
                </select>
                {aws.profile_locked && (
                  <p className="muted small">
                    Locked —{" "}
                    {aws.env_profile
                      ? `AWS_PROFILE=${aws.env_profile}`
                      : "environment access key"}{" "}
                    in use.
                  </p>
                )}
              </div>

              <div className="settings-field">
                <label>Region</label>
                <select
                  className="jselect"
                  value={aws.region_locked ? (aws.env_region ?? "") : (cfg.aws_region ?? "")}
                  disabled={aws.region_locked}
                  onChange={(e) => set("aws_region", e.target.value || null)}
                >
                  <option value="">Default / credential chain</option>
                  {regionOptions.map((r) => (
                    <option key={r} value={r}>
                      {r}
                    </option>
                  ))}
                </select>
                {aws.region_locked ? (
                  <p className="muted small">Locked — AWS_REGION={aws.env_region} in use.</p>
                ) : (
                  <p className="muted small">
                    Must match your S3 bucket's region — Transcribe is regional.
                  </p>
                )}
              </div>
            </>
          )}

          <div className="settings-field">
            <label>S3 bucket{envBadge("aws_bucket")}</label>
            <input
              type="text"
              value={cfg.aws_bucket ?? ""}
              placeholder="my-corti-bucket"
              disabled={isEnv("aws_bucket")}
              onChange={(e) => set("aws_bucket", e.target.value || null)}
            />
          </div>
          <div className="settings-field">
            <label>Language{envBadge("language")}</label>
            <input
              type="text"
              value={cfg.language}
              placeholder="en-US"
              disabled={isEnv("language")}
              onChange={(e) => set("language", e.target.value)}
            />
            <p className="muted small">BCP-47 tag, e.g. en-US, fr-FR.</p>
          </div>

          <div className="settings-field">
            <button className="btn-add" disabled={verifying} onClick={testCredentials}>
              {verifying ? "Testing…" : "Test credentials"}
            </button>
            <p className="muted small">Tests the saved configuration — Save first if you just changed the profile or region.</p>
            {verifyMsg && <p className="callout">{verifyMsg}</p>}
          </div>
        </>
      )}

      {cfg.transcribe_backend === "local" && (
        <>
          <p className="callout">
            On-device engine: NVIDIA Parakeet-TDT — runs fully offline; nothing leaves your Mac.
          </p>
          <div className="settings-field">
            <label>ONNX threads{envBadge("local_threads")}</label>
            <input
              type="number"
              min={1}
              value={cfg.local_threads}
              disabled={isEnv("local_threads")}
              onChange={(e) => set("local_threads", Math.max(1, Number(e.target.value) || 1))}
            />
          </div>
          <label className="radio">
            <input
              type="checkbox"
              checked={cfg.local_diarize_far_end}
              disabled={isEnv("local_diarize_far_end")}
              onChange={(e) => set("local_diarize_far_end", e.target.checked)}
            />
            Diarize the far end (split “Them” into per-speaker labels)
            {envBadge("local_diarize_far_end")}
          </label>
          <p className="muted small">
            Off by default. Splits the far end using the English embedding model below.
          </p>

          {embeddingModels.length > 0 && (
            <div className="settings-field">
              <label>Far-end embedding model{envBadge("local_embedding_model")}</label>
              <select
                className="jselect"
                value={cfg.local_embedding_model}
                disabled={!cfg.local_diarize_far_end || isEnv("local_embedding_model")}
                onChange={(e) => set("local_embedding_model", e.target.value)}
              >
                {embeddingModels.map((m) => (
                  <option key={m.id} value={m.id}>
                    {m.label} {m.present ? "· installed" : `· ~${Math.round(m.download_bytes / 1_000_000)} MB`}
                  </option>
                ))}
              </select>
              <p className="muted small">
                English (VoxCeleb-trained) model that separates far-end speakers. Download the selected one in
                Settings → Models. If it over-clusters, lower <code>CORTI_LOCAL_DIARIZE_THRESHOLD</code> (issue #18).
              </p>
            </div>
          )}
        </>
      )}

      <label className="radio">
        <input
          type="checkbox"
          checked={cfg.aec_enabled}
          disabled={isEnv("aec_enabled")}
          onChange={(e) => set("aec_enabled", e.target.checked)}
        />
        Acoustic echo cancellation
        {envBadge("aec_enabled")}
      </label>
      <p className="muted small">Clean speaker bleed from the mic track before transcription.</p>

      <label className="radio">
        <input
          type="checkbox"
          checked={cfg.live_filing}
          disabled={isEnv("live_filing")}
          onChange={(e) => set("live_filing", e.target.checked)}
        />
        Live inbox filing
        {envBadge("live_filing")}
      </label>
      <p className="muted small">
        Write the note while the call is still recording (local backend with installed models; falls
        back to normal end-of-call transcription otherwise).
      </p>

      <p className="callout">Changes apply to the next recording.</p>
    </section>
  );
}
