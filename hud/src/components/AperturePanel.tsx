import { type ApertureStatus, formatApertureDuration } from "../core/events";
import Frame from "./Frame";

/**
 * APERTURE (daemon aperture.rs). A private, owner-gated, ON-DEVICE activity
 * timeline ("Recall done right"): which app was frontmost + its window title + for
 * how long, so the owner can ask "what was I working on around 3pm" on-device.
 *
 * SHIPS OFF (opt-in): an activity timeline is privacy-sensitive, so until the owner
 * enables it nothing is polled or stored and the panel says so honestly.
 *
 * HONEST ABOUT COVERAGE: Aperture records the app name + window TITLE + time — NOT
 * screen pixels. Every title shown here was PII-REDACTED at capture (emails /
 * secrets / card + phone numbers stripped) and truncated — the panel never shows
 * raw window text, and when the timeline is off it shows nothing recorded. The ring
 * is bounded + transient (in-RAM only, off disk / memory / the optimizer), and
 * nothing ever leaves the box.
 */
export default function AperturePanel({ aperture }: { aperture: ApertureStatus | null }) {
  if (aperture === null) return null;

  return (
    <div className="aperture-panel">
      <Frame title="APERTURE" tag="MNEMOSYNE">
        <div className="aperture-body">
          <div className="aperture-head">
            <span className={`aperture-pill ${aperture.enabled ? "on" : "off"}`}>
              {aperture.enabled ? "RECORDING" : "OFF"}
            </span>
            {aperture.enabled ? (
              <span className="aperture-meta dim-note">
                {aperture.count} / {aperture.cap} activities · every {aperture.pollIntervalSecs}s
              </span>
            ) : (
              <span className="aperture-meta dim-note">
                opt-in — enable [aperture] for an on-device activity timeline
              </span>
            )}
          </div>
          {aperture.enabled && (
            <p className="aperture-coverage dim-note">app + window title + time · never screen pixels</p>
          )}
          {aperture.enabled && (
            <ul className="aperture-activities">
              {aperture.recent.length === 0 ? (
                <li className="aperture-empty dim-note">nothing recorded yet</li>
              ) : (
                aperture.recent.map((a, i) => (
                  <li key={i} className="aperture-activity">
                    <span className="aperture-app">{a.app}</span>
                    <span className="aperture-duration">{formatApertureDuration(a.durationSecs)}</span>
                    {a.title.trim().length > 0 && (
                      <span className="aperture-title dim-note">{a.title}</span>
                    )}
                  </li>
                ))
              )}
            </ul>
          )}
        </div>
      </Frame>
    </div>
  );
}
