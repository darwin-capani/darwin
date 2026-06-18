import type { CSSProperties } from "react";
import type { ImageGenerated } from "../core/events";
import { agentProfile } from "../core/agents";
import Frame from "./Frame";

/** The agent the daemon routes a "generate/make/draw an image of X" request to —
 *  the vision agent (Research + OSINT, the on-device vision owner). The live
 *  pipeline re-pins generate_image (alongside describe + identify-sound) to
 *  VISION_APP ("vision") in daemon/src/router.rs (pinned by the router test
 *  generate_image_routes_to_the_vision_agent), so the panel accent must read as
 *  that agent's to match the agent that actually acts. vision's identity hue is
 *  265 (a violet) in the static roster (config/agents.toml mirror), distinct
 *  from the cyan default. RED stays reserved for alerts/errors only — an
 *  unavailable/OFF model is the honest, expected ships-OFF state, NOT an error. */
export const IMAGE_AGENT = "vision";

/** The vision agent's identity hue from the static roster (config/agents.toml mirror). */
const IMAGE_HUE = agentProfile(IMAGE_AGENT)?.hue ?? 265;

export default function ImagePanel({
  generated,
}: {
  /** The last ON-DEVICE image-generation outcome (image.generated, channel
   *  "local"), or null until the daemon emits one. METADATA ONLY — whether the
   *  on-device MLX diffusion model actually produced an image (`available`),
   *  WHERE on the device the image landed (`path`, a local abs path under
   *  state/images/), and non-secret model/size/steps metadata + the `image`
   *  (cfg.image.enabled) opt-in flag. NEVER the prompt, NEVER pixels — the two
   *  most sensitive things in the op never ride telemetry (the seed is dropped
   *  too). Daemon-driven, so the panel surfaces this readout on its own. */
  generated?: ImageGenerated | null;
}) {
  // Before the daemon emits the first image.generated, there is nothing to show.
  // This panel is purely the image-generation readout (no app feed of its own).
  const has = !!generated;

  // The whole panel carries the routed agent's hue as a CSS custom property so
  // the accents read as that agent's (consumed by .img-* in styles.css).
  const style = { ["--image-hue" as string]: String(IMAGE_HUE) } as CSSProperties;

  // Header tag: a produced image wins, then the honest OFF / UNAVAILABLE posture,
  // else IDLE (nothing generated yet this session).
  const tag = !generated
    ? "IDLE"
    : generated.available
      ? "IMAGE SAVED"
      : generated.image
        ? "UNAVAILABLE"
        : "OFF";

  return (
    <Frame
      className={`image ${has ? "" : "idle"}`}
      title="IMAGE // ON-DEVICE DIFFUSION"
      tag={tag}
    >
      {!has ? (
        <div className="img-placeholder" style={style}>
          <div className="img-ph-big">NO IMAGE YET</div>
          <div className="img-ph-small">say "draw an image of …"</div>
        </div>
      ) : (
        <div className="img-body" style={style}>
          {/* On-device honesty banner. Generation runs ON-DEVICE (MLX diffusion);
              the prompt + the image stay on the machine — nothing goes to the
              cloud. This panel is the telemetry readout (where the file landed +
              non-secret metadata), never the pixels, never the prompt. */}
          <div className="img-surface">
            <span className="img-surface-dot" aria-hidden="true" />
            <span className="img-surface-text">GENERATION ON DEVICE · NO CLOUD</span>
            <span className="img-surface-sub">prompt + image stay on the machine</span>
          </div>

          {/* image.generated — the ON-DEVICE MLX-DIFFUSION readout. HONESTY: the
              PROMPT and the PIXELS never ride telemetry (the seed is dropped too),
              so this surfaces only the honest POSTURE: whether the on-device model
              actually produced an image (`available`), WHERE on the device it
              landed (`path`, a local abs path under state/images/), the non-secret
              model/size/steps metadata, and whether the model is enabled (`image`).
              DEVICE-GATED: a multi-GB diffusion model + RAM, slow on smaller chips,
              so it ships OFF; when it isn't downloaded / enabled the readout shows
              an honest unavailable state, NEVER a fabricated image, NEVER a silent
              cloud call. */}
          {generated ? (
            <div className={`img-gen ${generated.available ? "available" : "fallback"}`}>
              <div className="img-gen-head">
                <span className="img-gen-label">GENERATED IMAGE</span>
                <span className="img-gen-tag">ON-DEVICE MLX DIFFUSION</span>
                <span className={`img-gen-state ${generated.available ? "on" : "off"}`}>
                  {generated.available ? "SAVED" : "UNAVAILABLE"}
                </span>
              </div>

              {generated.available ? (
                // The on-device diffusion model produced + saved an image. Show
                // WHERE it landed on-device (the local path) + the non-secret
                // model/size/steps metadata. The PIXELS are NOT shown here (they
                // never ride telemetry); the PROMPT is NOT shown either (it stays
                // on the machine and never crosses the wire — honest, not hidden).
                <>
                  <div className="img-gen-saved">
                    Image generated on-device and saved locally. The pixels never
                    ride this readout, and the prompt stays on the machine — neither
                    leaves the device, nothing goes to the cloud.
                  </div>
                  <div className="img-gen-path-row">
                    <span className="img-gen-path-label">ON-DEVICE PATH</span>
                    <code className="img-gen-path">{generated.path}</code>
                  </div>
                  <div className="img-gen-meta">
                    <span className="img-gen-chip">
                      MODEL {generated.model ?? "—"}
                    </span>
                    {generated.size !== null ? (
                      <span className="img-gen-chip">{generated.size}px</span>
                    ) : null}
                    {generated.steps !== null ? (
                      <span className="img-gen-chip">{generated.steps} STEPS</span>
                    ) : null}
                  </div>
                </>
              ) : (
                // Honest unavailable state. `image` distinguishes the two honest
                // shapes: enabled-but-couldn't-produce vs the shipped-OFF model.
                // NEVER a fabricated image, NEVER a silent cloud fall-back.
                <div className="img-gen-fallback">
                  {generated.image
                    ? "The on-device image model is enabled but couldn't generate an image this turn — it failed honestly. No image is invented, and there is no cloud fall-back (image generation is on-device only)."
                    : "The on-device image model isn't set up — it needs a multi-GB MLX diffusion model download + RAM, so it ships OFF/opt-in. No image is invented, and there is no cloud fall-back (image generation is on-device only)."}
                </div>
              )}

              <div className="img-gen-note">
                on-device MLX diffusion · prompt + image stay on the machine, nothing
                goes to the cloud · device-gated (multi-GB model + RAM, slow on
                smaller chips) · OFF/opt-in · MODEL {generated.image ? "ON" : "OFF"}
              </div>
            </div>
          ) : null}
        </div>
      )}
    </Frame>
  );
}
