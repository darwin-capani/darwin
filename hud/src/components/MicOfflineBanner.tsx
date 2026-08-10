/**
 * MICROPHONE OFFLINE — the one thing the idle ring cannot say for itself.
 *
 * `audio.rs` parks its capture thread forever when `acquire_input_device()`
 * fails, deliberately: a microphone that cannot be opened must not take the
 * router, scheduler, HUD feed and tool host down with it on a launchd restart
 * loop. The cost of that correct decision is that the HUD then renders a fully
 * live appliance sitting at idle — pixel-for-pixel what it renders while
 * listening to a quiet room. The daemon says `capture.unavailable`; this is
 * where that sentence becomes visible.
 *
 * Its own component rather than JSX inside App.tsx so the pixels are assertable
 * in the node-environment vitest suite — a reducer field with no renderer is the
 * same silence one layer up.
 */
export default function MicOfflineBanner({ error }: { error: string | null }) {
  return (
    <div className={`banner mic ${error !== null ? "visible" : ""}`}>
      <span className="pulse">MICROPHONE OFFLINE — NOT LISTENING</span>
      {error ? <span className="banner-detail">{error}</span> : null}
    </div>
  );
}
