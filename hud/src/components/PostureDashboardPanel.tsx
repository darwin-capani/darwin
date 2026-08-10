import type { PostureSnapshot, PostureVerdict, UpdatesVerdict } from "../core/events";
import Frame from "./Frame";

/**
 * POSTURE // THIS MAC — the machine security dashboard (daemon posture.rs ->
 * `posture.snapshot`, 30-minute cadence). Four READ-ONLY checks: FileVault,
 * application firewall, System Integrity Protection, pending software updates.
 *
 * HONESTY CONTRACT (do not regress):
 *   - PROTECTED (green) only for a verdict the daemon actually read; anything
 *     unrecognized shows UNCLEAR and an unrunnable check shows UNREADABLE —
 *     the board never fabricates a green shield.
 *   - OFF is the exposure state (amber) with the plain remediation hint; the
 *     daemon only reports — turning a protection on is the user's own action
 *     in System Settings (no remediation path exists, not even a gated one).
 *   - TCC grants + micro-app introspection live on their own panels beside
 *     this one (their own events); this board is ONLY the machine checks.
 */
export default function PostureDashboardPanel({ posture }: { posture: PostureSnapshot | null }) {
  // Render nothing until the first frame — never a fabricated board.
  if (posture === null) return null;

  return (
    <div className="posture-panel">
      <Frame title="POSTURE // THIS MAC" tag="AEGIS">
        <div className="posture-body">
          <PostureRow label="FileVault (disk encryption)" verdict={posture.filevault} />
          <PostureRow label="Application firewall" verdict={posture.firewall} />
          <PostureRow label="System Integrity Protection" verdict={posture.sip} />
          <UpdatesRow verdict={posture.updates} pending={posture.updatesPending} />
          <ScannerNotes lines={posture.scanners} />
          <div className="posture-foot dim-note">
            Read-only: Aegis reports where you stand; turning a protection on is
            yours to do in System Settings.
            {checkedLabel(posture.checkedTs)}
          </div>
        </div>
      </Frame>
    </div>
  );
}

/**
 * The AMBIENT SCANNERS block: persistence (autostart surfaces), inbound exposure
 * (listening sockets), traffic interception (proxy / hosts / root CAs / DNS /
 * profiles).
 *
 * WHY IT IS HERE. Each of those three scanners emits a full finding frame the
 * HUD reducer has no case for, so the findings reached no pixel; their summaries
 * reached the owner ONLY through the SPOKEN posture, i.e. only if the owner
 * thought to ask. A rogue trusted root CA silently breaks every TLS connection
 * on the machine — that is not a thing to hold until asked. The daemon folds the
 * three one-liners onto this board, so no new panel exists for them.
 *
 * HONESTY: these lines carry their OWN scanners' cadence and are NOT dated by
 * `checkedTs` (which belongs to the four machine checks above) — hence the
 * separate heading and no timestamp here. Renders NOTHING when no scanner has
 * reported: absence is honest, an empty "all quiet" block would not be.
 */
function ScannerNotes({ lines }: { lines: string[] }) {
  if (lines.length === 0) return null;
  return (
    <div className="posture-scanners">
      <div className="posture-scanners-head dim-note">
        AMBIENT SCANNERS — read-only, each on its own cadence
      </div>
      {lines.map((line) => (
        <div className="posture-scanner-line" key={line}>
          {line}
        </div>
      ))}
    </div>
  );
}

/** One protection row: label + verdict pill. */
function PostureRow({ label, verdict }: { label: string; verdict: PostureVerdict }) {
  return (
    <div className="posture-row">
      <span className="posture-label">{label}</span>
      <span className={`posture-pill ${pillClass(verdict)}`}>{pillText(verdict)}</span>
    </div>
  );
}

/** The pending-updates row: up-to-date / N pending / can't-confirm. */
function UpdatesRow({ verdict, pending }: { verdict: UpdatesVerdict; pending: number }) {
  const cls =
    verdict === "up_to_date" ? "protected" : verdict === "pending" ? "exposed" : "unknown";
  const text =
    verdict === "up_to_date"
      ? "UP TO DATE"
      : verdict === "pending"
        ? `${pending} PENDING`
        : verdict === "unreadable"
          ? "UNREADABLE"
          : "UNCLEAR";
  return (
    <div className="posture-row">
      <span className="posture-label">Software updates</span>
      <span className={`posture-pill ${cls}`}>{text}</span>
    </div>
  );
}

function pillClass(v: PostureVerdict): string {
  switch (v) {
    case "on":
      return "protected";
    case "off":
      return "exposed";
    default:
      return "unknown";
  }
}

/** Honest data-age note: the checks ran at checkedTs (the daemon re-broadcasts
 *  a cached snapshot between 30-min scans, so the frame's arrival time is not
 *  the data's age). Silent when the stamp is absent or unparsable. */
function checkedLabel(checkedTs: string): string {
  if (checkedTs === "") return "";
  const t = new Date(checkedTs);
  if (Number.isNaN(t.getTime())) return "";
  const hh = String(t.getHours()).padStart(2, "0");
  const mm = String(t.getMinutes()).padStart(2, "0");
  return ` Checked ${hh}:${mm}.`;
}

function pillText(v: PostureVerdict): string {
  switch (v) {
    case "on":
      return "ON";
    case "off":
      return "OFF";
    case "unreadable":
      return "UNREADABLE";
    default:
      return "UNCLEAR";
  }
}
