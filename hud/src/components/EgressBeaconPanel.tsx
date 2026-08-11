import type { EgressBeaconAlert } from "../core/events";
import Frame from "./Frame";

/**
 * EGRESS // BEACON WATCH — suspected C2-style callbacks from the longitudinal
 * egress sentinel (daemon/src/egress_beacon.rs -> egress.beacon). A row means a
 * process contacted one host on a metronome-regular interval: at least six
 * fresh-connection rising edges whose cadence sits in the plausible callback
 * band with a coefficient of variation <= 0.15. That is real signal — benign
 * metronomes exist (a mail poller), but the row names the PROCESS, so the owner
 * can recognize one at a glance and act on the one they don't.
 *
 * SAFETY CONTRACT (do not regress — same posture as the TCC sentinel panel):
 *   - PROPOSE-ONLY. Each row carries the rendered pf block rule as TEXT (hover:
 *     the row's tooltip). DARWIN never runs pfctl and never mutates the
 *     firewall; applying anything is the owner's own sudo, or nobody's.
 *   - SECRET-FREE. Process names + bare host IPs + measured numbers only.
 *   - HONEST. Attribution is UID-scoped (unprivileged lsof cannot see other
 *     users' processes) — stated below, not hidden.
 *
 * Renders nothing until a beacon has actually been flagged: an empty watch is
 * not a panel's worth of reassurance.
 */
export default function EgressBeaconPanel({ beacons }: { beacons: EgressBeaconAlert[] }) {
  if (beacons.length === 0) return null;

  return (
    <div className="tcc-panel">
      <Frame title="EGRESS // BEACON WATCH" tag="PROPOSE ONLY">
        <div className="tcc-body">
          <div className="tcc-anomalies">
            <div className="tcc-anomalies-title">SUSPECTED BEACONS</div>
            {beacons.map((b) => (
              <div
                key={b.key}
                className="tcc-anomaly warn"
                title={b.proposal || b.line}
              >
                {b.line}
              </div>
            ))}
          </div>
          <div className="tcc-note dim-note">
            A regular callback cadence is the classic phone-home signature. Hover
            a row for the propose-only pf block rule — review it and apply it
            yourself with sudo; I never touch the firewall. Same-user processes
            only (unprivileged lsof cannot attribute other users&apos;
            connections).
          </div>
        </div>
      </Frame>
    </div>
  );
}
