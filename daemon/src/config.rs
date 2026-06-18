use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Deserialize;
use tracing::warn;

/// Mirrors config/jarvis.toml. Every section and key falls back to the
/// contract defaults so the daemon runs even with no config file on disk.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub audio: AudioConfig,
    pub models: ModelsConfig,
    pub router: RouterConfig,
    pub local_tools: LocalToolsConfig,
    pub cloud: CloudConfig,
    pub speech: SpeechConfig,
    pub inference: InferenceConfig,
    pub self_heal: SelfHealConfig,
    pub forge: ForgeConfig,
    pub telemetry: TelemetryConfig,
    pub proactive: ProactiveConfig,
    /// [focus] — FOCUS PROFILES (#24, focus.rs). `profile` ships "default" (the
    /// IDENTITY — today's behavior). A profile is PERMISSION-NEUTRAL: it can only
    /// quiet/narrow which non-consequential proactive intel surfaces, never
    /// loosen a gate, enable a consequential action, or raise autonomy.
    pub focus: FocusConfig,
    pub apps: AppsConfig,
    pub integrations: IntegrationsConfig,
    pub standing: StandingConfig,
    /// [drafts] — AUTO-DRAFT (#25, drafts.rs). `enabled` ships OFF: proactive
    /// drafting only runs when the operator turns it on. A draft is ALWAYS a
    /// reviewable PENDING suggestion — never auto-sent — so the flag only governs
    /// whether JARVIS composes drafts proactively, never whether one is dispatched.
    pub drafts: DraftsConfig,
    /// [missions] — DURABLE MISSIONS (#26, durable_missions.rs). `durable` ships
    /// OFF: Fury mission state is persisted only when the operator turns it on. A
    /// persisted mission ALWAYS loads PAUSED (never auto-runs on restart) and every
    /// consequential step re-runs through the gate when resumed — the flag only
    /// governs persistence, never autonomy.
    pub missions: MissionsConfig,
    /// [macros] — MACRO RECORD/REPLAY (#27, macros.rs). `enabled` ships OFF:
    /// recording a named command sequence only happens when the operator turns it
    /// on. A macro stores ONLY utterances/intent names (never secrets) and replay
    /// re-runs each command through the normal router + the gate fresh — the flag
    /// only governs whether macros may be recorded/replayed, never the gate.
    pub macros: MacrosConfig,
    pub mcp: McpConfig,
    pub skills: SkillsConfig,
    pub optimize: OptimizeConfig,
    pub voice_id: VoiceIdConfig,
    pub episodic: EpisodicConfig,
    pub notebooks: NotebookConfig,
    pub lifelog: LifeLogConfig,
    pub voice: VoiceConfig,
    /// [wake] — the CUSTOM WAKE-WORD (#32). `phrase` defaults to "jarvis" and
    /// `enabled` ships OFF (the matcher then never gates activation — today's
    /// behavior). PURE matcher in wake.rs; the always-listening loop is device-gated.
    pub wake: WakeConfig,
    /// [interpret] — CONTINUOUS LIVE INTERPRETATION (#30). `live` ships OFF; when on
    /// the device-gated mic loop feeds each VAD segment through the PURE
    /// interpret_segment pipeline. The pure core is in interpret.rs.
    pub interpret: InterpretConfig,
    pub docsearch: DocSearchConfig,
    pub code: CodeConfig,
    /// [shell] — SANDBOXED SHELL / TERMINAL (#43, shell.rs): the HIGHEST-RISK
    /// capability (arbitrary command execution). `enabled` SHIPS OFF (false): with
    /// it false the shell intent is never classified and the `shell_run` tool is
    /// inert (an honest "off" reply); nothing is parked, nothing runs. Even ON, a
    /// command must clear a conservative destructive DENYLIST, then PARK as a
    /// CONSEQUENTIAL tool for a spoken human "yes", and only ever EXEC under the
    /// master switch + confirm + voice-id + !lockdown — under a DENY-DEFAULT
    /// sandbox-exec profile (no network, write-confined to a scratch dir, the
    /// Keychain / ~/.claude / daemon state denied). The exec is DEVICE-gated.
    pub shell: ShellConfig,
    /// [ui_automation] — GATED UI AUTOMATION (#44, the CAPSTONE, ui_automation.rs):
    /// the SINGLE MOST DANGEROUS capability (physically actuating the macOS UI —
    /// click/type/key). `enabled` SHIPS OFF (false): with it false the actuate
    /// intent is never classified and the `ui_actuate` tool is inert (an honest
    /// "off" reply); nothing is planned, parked, or actuated. Even ON, EVERY
    /// actuation is CONSEQUENTIAL, so it PARKS PER ACTION for a spoken human "yes"
    /// (ONE confirm = ONE actuation; a second re-parks), and only ever fires under
    /// the master switch + confirm + voice-id + !lockdown — never batched, never
    /// autonomous. The actuation itself is DEVICE-gated (Accessibility TCC consent).
    pub ui_automation: UiAutomationConfig,
    pub vision: VisionConfig,
    pub image: ImageConfig,
    /// [screen_context] — CONTINUOUS SCREEN CONTEXT (#42, screen_context.rs).
    /// `enabled` SHIPS OFF: with it false the device-gated continuous capture loop
    /// NEVER runs, the bounded in-RAM ring NEVER grows on its own, no WATCHING
    /// indicator fires, and routing is byte-for-byte today's. Even ON it is
    /// TCC-device-gated; the ring is bounded/redacted/transient (in-RAM only, off
    /// lifelong memory / optimizer / disk) + forgettable; recall is read-only.
    pub screen_context: ScreenContextConfig,
    pub answers: AnswersConfig,
    pub audit: AuditConfig,
    pub policy: PolicyConfig,
    pub security: SecurityConfig,
    /// [webhooks] — WEBHOOK TRIGGERS (#35, webhooks.rs): an INBOUND network
    /// surface. `enabled` SHIPS OFF (false): with it false the loopback listener
    /// never binds and no event is ever received. Even with it on, EVERY request
    /// is HMAC-authenticated (a forged/missing signature is rejected), only an
    /// explicitly-mapped event routes (an unmapped event is rejected), and a
    /// mapped CONSEQUENTIAL intent PARKS for a spoken confirm — a webhook can
    /// never auto-execute a side-effecting action. `bind` defaults to 127.0.0.1
    /// loopback. The HMAC secret is resolved from the Keychain, never the TOML.
    pub webhooks: WebhooksConfig,
    /// [plugin_sdk] — PLUGIN SDK (#36, plugin_sdk.rs): formalizes + VALIDATES the
    /// micro-app capability-module contract (the [intents]/[tools] manifest
    /// block). `enabled` SHIPS OFF (false): with it false the register-on-launch
    /// handshake is inert and no plugin's intents/tools are scoped. The validator
    /// itself is PURE (always available for `apps validate`); the flag governs
    /// whether the live launch handshake admits a plugin's declared intents.
    pub plugin_sdk: PluginSdkConfig,
    /// [power] — BATTERY/THERMAL ADAPTIVE THROTTLING (#38, power.rs). `adaptive`
    /// SHIPS OFF (false): with it false NOTHING reads power/thermal state, the
    /// throttle is always neutral (no tier preference, no deferral), and routing
    /// is byte-for-byte today's behavior. Even when ON, the PURE throttle policy
    /// only ever PREFERS the cheaper LOCAL Fast sub-tier / defers heavy work on a
    /// low battery or serious thermal pressure — it never loosens a gate, never
    /// makes a cloud call, and the LIVE pmset/thermal reader is device-gated.
    pub power: PowerConfig,
    /// [report] — REPORT GENERATION (#40, report.rs). `enabled` SHIPS OFF (false):
    /// with it false the "generate a report on X" op declines and routing is
    /// byte-for-byte today's. The op is READ-ONLY — it pulls the already-cited
    /// notebook/research material and folds it into a BOUNDED markdown report,
    /// REUSING research.rs's cite discipline (every citation a REAL source ref an
    /// input claim carried; an uncited claim dropped, never fabricated; no citable
    /// source -> an honest-empty report). It speaks/displays, acts/reaches nothing.
    pub report: ReportConfig,
    /// [chart] — DATA -> CHART (#41, chart.rs). `enabled` SHIPS OFF (false): with it
    /// false the "chart this" op declines and emits nothing. The op is a NEUTRAL
    /// presentation act — it serializes a ChartSpec (the exact data points) as a
    /// `chart.data` telemetry envelope the HUD plots EXACTLY (no interpolation, no
    /// invented point, honest axes, honest-empty). It changes no gate, takes no
    /// action, reaches no network; the emit is fire-and-forget like every other
    /// telemetry envelope.
    pub chart: ChartConfig,
}

/// Every section and key the config knows, for unknown-key diagnostics
/// (audit fix: a typo'd key or section used to be silently ignored with zero
/// signal). MUST stay in lockstep with the section structs below and with
/// config/jarvis.toml — including keys consumed only server-side.
/// "mode" under self_heal is part of the self-heal contract
/// ("propose"|"auto") and is listed here so adding it never reads as a typo.
const KNOWN_KEYS: &[(&str, &[&str])] = &[
    ("audio", &["rms_threshold", "silence_ms", "min_speech_ms", "barge_in", "barge_in_rms", "barge_in_ms", "sound_monitor"]),
    // [models] — `vlm` is consumed server-side only (the OPTIONAL on-device VLM
    // for op=describe_image); listed so it never reads as a typo. The
    // multi-resident LOCAL warm-set keys (local_warm/local_budget_gib/local_sizes,
    // task #17) ship CONSERVATIVE: empty + 0 == single-resident.
    ("models", &["llm", "stt", "classifier", "vlm", "local_warm", "local_budget_gib", "local_sizes"]),
    ("router", &["cloud_confidence_threshold", "conversation_route"]),
    // [local_tools] — the OFFLINE bounded tool-loop (Local tier / cloud
    // unreachable). "subset" is an OPTIONAL allow-list override of the curated
    // safe local read/compute tools; an empty/absent list uses the built-in
    // curated subset. "enabled" gates the whole loop; "max_rounds" bounds it.
    ("local_tools", &["enabled", "max_rounds", "subset"]),
    ("cloud", &["fast_model", "heavy_model", "max_tokens"]),
    (
        "speech",
        &[
            "engine",
            "model",
            "voice",
            "speed",
            "openers",
            "sentence_pause_ms",
            "opener_delay_ms",
            "instant_opener",
        ],
    ),
    // [inference] — server-side runtime knobs. `preload` is the existing
    // contract key. SPECULATIVE DECODING (#37): `speculative` ships OFF +
    // `draft_model` ships "" (off => normal generation, today's runtime).
    // SELECTABLE QUANTIZATION (#39): `quant` ships "auto" (== today's behavior;
    // validated against InferenceConfig::ALLOWED_QUANT, an unknown value falls
    // back to "auto"). Listed so none reads as a typo.
    ("inference", &["preload", "speculative", "draft_model", "quant"]),
    ("self_heal", &["enabled", "mode"]),
    // [forge] — Self-Forge (forge.rs). Same shape and contract as [self_heal]:
    // "mode" is "propose"|"auto" and is listed so it never reads as a typo. Note
    // "auto" NEVER deploys a forged app into apps/ — deploy is ALWAYS a separate
    // human step (scripts/apply_forge.sh); see ForgeConfig.
    ("forge", &["enabled", "mode"]),
    ("telemetry", &["port"]),
    (
        "proactive",
        &[
            "enabled",
            "idle_gap_hours",
            // EDITH anticipation (anticipate.rs). `speak` ships OFF, exactly
            // like self_heal/allow_consequential: with it false EDITH only
            // surfaces a HUD card and NEVER speaks unprompted.
            "speak",
            // Proactive-intelligence suggester (proactive_intel.rs). `suggest`
            // ships OFF, its OWN gate (not piggybacked on `enabled`), mirroring
            // `speak`: with it false the anticipation tick emits no suggestion.
            "suggest",
            "lead_minutes",
            "unread_floor",
            "quiet_start",
            "quiet_end",
        ],
    ),
    // [focus] — FOCUS PROFILES (#24, focus.rs). `profile` ships "default" (the
    // identity — today's behavior). PERMISSION-NEUTRAL: a profile only quiets
    // which non-consequential proactive intel surfaces, never loosens a gate.
    ("focus", &["profile"]),
    ("apps", &["autostart"]),
    ("integrations", &["allow_consequential"]),
    // [standing] — Standing Missions (standing.rs). `enabled` is the subsystem
    // master switch and ships OFF, exactly like self_heal/forge/proactive.speak:
    // with it false the scheduler marks NOTHING due, so no standing mission ever
    // fires (and establishing one is itself a confirmation-gated action).
    ("standing", &["enabled"]),
    // [drafts] — AUTO-DRAFT (#25, drafts.rs). `enabled` SHIPS OFF (no proactive
    // drafting). A draft is always a reviewable suggestion — the module has no send
    // path, so this flag never enables an autonomous send. `retention` bounds the
    // pending-draft store. Listed so neither key reads as a typo.
    ("drafts", &["enabled", "retention"]),
    // [missions] — DURABLE MISSIONS (#26, durable_missions.rs). `durable` SHIPS OFF
    // (in-memory missions, today's behavior). A persisted mission ALWAYS loads
    // PAUSED (no auto-run on restart) and re-gates each consequential step on
    // resume — this flag governs persistence only, never autonomy. `retention`
    // bounds the mission store. Listed so neither key reads as a typo.
    ("missions", &["durable", "retention"]),
    // [macros] — MACRO RECORD/REPLAY (#27, macros.rs). `enabled` SHIPS OFF. Replay
    // re-runs each recorded command through the NORMAL router + the gate FRESH (no
    // pre-approval, no batching past the gate); the store holds only utterances +
    // intent names (never a secret). `max_steps` bounds one macro; `retention`
    // bounds the store. Listed so none reads as a typo.
    ("macros", &["enabled", "max_steps", "retention"]),
    // [mcp] — Model Context Protocol client (mcp.rs). `enabled` is the subsystem
    // master switch and SHIPS OFF, exactly like self_heal/forge/standing. The
    // bounds (max_servers / max_tools_per_server / call_timeout_ms /
    // max_output_bytes) cap blast radius. `servers` is an array-of-tables
    // ([[mcp.servers]]); its per-entry keys are validated by McpServerConfig's
    // `deny_unknown_fields` at deserialize time, so only the [mcp] top-level keys
    // are listed here.
    (
        "mcp",
        &[
            "enabled",
            "max_servers",
            "max_tools_per_server",
            "call_timeout_ms",
            "max_output_bytes",
            "servers",
        ],
    ),
    // [skills] — the skill library (skills/). `enabled` is the master switch and,
    // UNLIKE self_heal/forge/standing/mcp, SHIPS ON (true): the in-tree skills are
    // PURE + read-only and safe to offer by default. Turning skills off only hides
    // the meta-tools; it does NOT loosen any other gate. A CONSEQUENTIAL skill is
    // still parked behind the cross-turn confirmation gate + the OFF-by-default
    // [integrations] allow_consequential master switch regardless of this flag —
    // `enabled` controls whether the catalog is OFFERED, never whether a
    // side-effecting skill may fire unconfirmed.
    ("skills", &["enabled"]),
    // [optimize] — the optimization-from-usage loop (optimize.rs). The SAME
    // OFF-by-default, propose-only contract as [self_heal]/[forge]: `enabled`
    // is the master switch and SHIPS OFF — when false the trace recorder is a
    // no-op (nothing is stored), so no learning corpus accrues. `mode` is
    // "propose"|"auto" and is listed so adding it never reads as a typo; the
    // Trace Store itself never acts on either value (it only records when
    // enabled), and the downstream Optimizer phase ALWAYS proposes — there is
    // no auto-apply-to-live-config path, exactly like self-heal's mode.
    ("optimize", &["enabled", "mode"]),
    // [voice_id] — on-device speaker verification (voiceid.rs). `enabled` is the
    // master switch and SHIPS OFF, exactly like self_heal/forge/standing/mcp/
    // optimize: with it false (or with no enrolled profile) NOTHING is gated by
    // voice — behavior is unchanged. `gate_scope` is "consequential"|"all"
    // (unknown -> "consequential"); listed here so it never reads as a typo.
    ("voice_id", &["enabled", "threshold", "min_enroll_samples", "gate_scope"]),
    // [episodic] — the episodic store (episodic.rs). UNLIKE self_heal/forge/
    // optimize/voice_id, `enabled` SHIPS ON (true): it is the SAME always-on,
    // bounded, local posture as the transcripts table / lifelong-learning fact
    // loop, not an autonomy gate. Recording is still gated per-turn (transient
    // screen-reads + voice-id-unverified + empty turns are never recorded),
    // redacted, agent-scoped, and forgettable. `retention` is the evict-oldest
    // episodes cap (bounded memory); both keys are listed so neither reads as a
    // typo.
    ("episodic", &["enabled", "retention"]),
    // [notebooks] — RESEARCH NOTEBOOKS (notebook.rs): the persistent store of
    // SAGE research runs (a run -> a CITED notebook entry; revisit + append). SAME
    // always-on-but-BOUNDED posture as [episodic] — `enabled` SHIPS ON (true): a
    // notebook is just a persisted, READ-ONLY record of a research run that
    // already happened (cited, redacted, agent-scoped, forgettable), not an
    // autonomy gate. With it false no run is saved and revisit returns an honest
    // empty (never fabricates). `retention` is the evict-oldest ENTRIES cap
    // (bounded memory). Both keys listed so neither reads as a typo.
    ("notebooks", &["enabled", "retention"]),
    // [lifelog] — the LIFE-LOG DIGEST (lifelog.rs): a periodic (daily/weekly)
    // browsable summary built ONLY from the agent-scoped redacted EPISODIC store.
    // SAME always-on-but-bounded posture as [episodic]/[notebooks] — `enabled`
    // SHIPS ON (true): the digest is a READ-ONLY fold over episodes that already
    // exist (never fabricating; empty/sparse window -> honest empty), not an
    // autonomy gate. With it false the digest intent returns an honest "life log
    // is off". It owns no store of its own — forgetting episodes empties it. Listed
    // so the key never reads as a typo.
    ("lifelog", &["enabled"]),
    // [voice] — the OPTIONAL ElevenLabs cloud VOICE TIER (voice_tier.rs). An ADDED
    // TTS layer on top of the on-device Kokoro default, never a replacement. The
    // SAME OFF-by-default posture as self_heal/forge/standing/mcp: `cloud_tier`
    // SHIPS OFF (false) — with it false (OR no `elevenlabs_api_key` in the Keychain,
    // OR the model-swap tier is Local/"work offline") TTS behaves EXACTLY as today
    // (on-device Kokoro). `model` is the ElevenLabs model id (default
    // eleven_flash_v2_5). `voices` is an inline per-agent map (agent name -> EL
    // voice id); an empty/unmapped agent falls back to that agent's Kokoro voice.
    // VOICE-ONLY: JARVIS owns its own brain/router/turn-taking — this tier is TTS,
    // not a hosted Conversational Agents platform. Listed here so neither key reads
    // as a typo; the [voice.voices] table is validated structurally by serde.
    // `cloud_stt` (build 2/2) is the SEPARATE OFF-by-default master switch for the
    // ElevenLabs Scribe cloud-STT tier — gated independently of `cloud_tier` (TTS)
    // because STT sends the user's VOICE AUDIO to the cloud (MORE sensitive than
    // TTS text); on-device whisper is the private/offline default + fallback. Listed
    // here so it never reads as a typo.
    // `adaptive_prosody` (#33) / `whisper` + `whisper_auto` (#34) are the
    // EXPRESSIVENESS flags (prosody.rs), all OFF by default: adaptive prosody shapes
    // EL-v3 audio-tags/stability when the backend is EL-v3-capable (coarse/neutral on
    // Kokoro — EL-v3-gated, never faked); whisper makes replies terse + soft via an
    // explicit command (never silencing a required confirmation); whisper_auto is the
    // separately-gated PURE low-amplitude auto-engage heuristic. Listed so none reads
    // as a typo.
    // `diarize` (#31) is the OFF-by-default consumer of EL-Scribe speaker labels:
    // when ON and the active STT backend is EL Scribe (which carries speaker labels),
    // a PURE label-mapper (diarize.rs) renders a multi-speaker transcript; on-device
    // whisper (no diarization model) is an HONEST single-stream "speaker: unknown"
    // labeling — NEVER a fabricated set of distinct speakers. Listed so it never reads
    // as a typo.
    ("voice", &["cloud_tier", "cloud_stt", "model", "voices", "adaptive_prosody", "whisper", "whisper_auto", "diarize"]),
    // [wake] — CUSTOM WAKE-WORD (#32, wake.rs). `enabled` SHIPS OFF (false): with it
    // false the configured phrase gates NOTHING and activation is byte-for-byte today's
    // (the wake matcher is never consulted). `phrase` defaults to "jarvis" so even when
    // turned on the default preserves today's wake behavior. The PURE wake_match
    // (case/punct/whitespace-insensitive + a small edit-distance tolerance; NEVER
    // matches an empty/blank phrase; never triggers on a substring of a larger unrelated
    // word) is in wake.rs; the always-listening loop that calls it is DEVICE-GATED.
    // Listed so neither key reads as a typo.
    ("wake", &["enabled", "phrase"]),
    // [interpret] — CONTINUOUS LIVE INTERPRETATION (#30, interpret.rs). `live` SHIPS OFF
    // (false): with it false the per-segment interpret pipeline NEVER runs from the mic
    // loop and the audio path is byte-for-byte today's. When ON, the DEVICE-GATED mic
    // loop feeds each VAD segment through the PURE interpret_segment (transcribe ->
    // on-device-LLM translate -> render/optionally speak); offline/unavailable degrades
    // HONESTLY (never a fabricated translation). `source_lang` / `target_lang` are the
    // interpret direction (target defaults to "English"; an empty source = auto-detect).
    // `speak` (OFF) decides whether the rendered translation is also voiced through the
    // single echo-safe speech path. Listed so none reads as a typo.
    ("interpret", &["live", "speak", "source_lang", "target_lang"]),
    // [docsearch] — on-device file RAG (docsearch.rs): index + search the user's
    // OWN text-like files, 100% on-device. The SAME OFF-by-default, opt-in posture
    // as self_heal/forge/standing/mcp/optimize/voice_id — it reads the user's
    // files, so it SHIPS DISABLED and indexes NOTHING until the operator both flips
    // `enabled` and ALLOWLISTS a root. `roots` is the EXPLICIT allowlist of folders
    // that may be indexed (EMPTY by default — never a whole-disk scan; every
    // candidate file is path-confined under a canonicalized root). The remaining
    // keys are BOUNDS on the index (max files/chunks/bytes, per-file size cap,
    // recursion depth) so the on-disk store stays finite. `build_graph`
    // (knowledge_graph.rs) is the OFF-by-default switch for the deterministic
    // knowledge-graph build over the already-indexed chunks; it is a real parsed
    // DocSearchConfig field, so it MUST be listed here or the daemon falsely warns
    // "unknown config key docsearch.build_graph ignored" while still honoring it.
    // Listed here so none reads as a typo; the `roots` array is validated
    // structurally by serde.
    (
        "docsearch",
        &[
            "enabled",
            "roots",
            "max_files",
            "max_chunks",
            "max_file_bytes",
            "max_depth",
            "chunk_chars",
            "chunk_overlap",
            "build_graph",
        ],
    ),
    // [code] — CODE INTELLIGENCE (code.rs): code_explain (grounded answers over the
    // docsearch code index, CITED) + code_propose_diff (a PROPOSE-ONLY reviewable
    // unified diff written to state/code/proposals/<ts>/ — it NEVER edits the user's
    // code; the only path that touches code is the human-reviewed
    // scripts/apply_code_diff.sh, confined-by-construction to a [code].roots root).
    // The SAME OFF-by-default, opt-in posture as self_heal/forge/standing/mcp/
    // optimize/voice_id/docsearch — because it READS and PROPOSES EDITS to the
    // user's code, it SHIPS DISABLED and does NOTHING until the operator both flips
    // `enabled` AND allowlists a `roots` codebase root. `roots` is the EXPLICIT
    // allowlist of codebase roots (the apply script writes ONLY under a canonicalized
    // root, and code_explain answers only from the docsearch index built over them);
    // EMPTY by default — never an arbitrary path. `max_diff_bytes` bounds the size of
    // a proposed diff (a bounded artifact). Listed here so none reads as a typo; the
    // `roots` array is validated structurally by serde.
    (
        "code",
        &[
            "enabled",
            "roots",
            "max_diff_bytes",
        ],
    ),
    // [shell] — SANDBOXED SHELL / TERMINAL (#43, shell.rs): the HIGHEST-RISK
    // capability (arbitrary command execution). The SAME OFF-by-default, opt-in
    // posture as self_heal/forge/code/vision. `enabled` SHIPS OFF (false): with it
    // false the shell intent is never classified and `shell_run` is inert (an
    // honest "off" reply); nothing parks, nothing runs. Even ON, every command must
    // clear a conservative destructive DENYLIST, then PARK as a consequential tool
    // for a spoken human "yes", and only ever EXEC under the master switch +
    // confirm + voice-id + !lockdown, inside a DENY-DEFAULT sandbox-exec profile
    // (no network, write-confined to a scratch dir, the Keychain/~/.claude/daemon
    // state denied). The exec itself is DEVICE-gated. Listed here so the key never
    // reads as a typo.
    ("shell", &["enabled"]),
    // [ui_automation] — GATED UI AUTOMATION (#44, the CAPSTONE, ui_automation.rs):
    // the SINGLE MOST DANGEROUS capability (physically actuating the macOS UI —
    // click/type/key). The SAME OFF-by-default, opt-in posture as shell/self_heal/
    // forge/code/vision. `enabled` SHIPS OFF (false): with it false the actuate
    // intent is never classified and `ui_actuate` is inert (an honest "off" reply);
    // nothing is planned, parked, or actuated. Even ON, EVERY actuation is
    // CONSEQUENTIAL — it PARKS PER ACTION for a spoken human "yes" (ONE confirm =
    // ONE actuation; a second re-parks) and only ever fires under the master switch
    // + confirm + voice-id + !lockdown, never batched/autonomous. The actuation
    // itself is DEVICE-gated (Accessibility TCC consent). Listed here so the key
    // never reads as a typo.
    ("ui_automation", &["enabled"]),
    // [vision] — the OPTIONAL on-device VISION-LANGUAGE model (VLM) describe path
    // (inference describe_image op + the daemon "describe my screen / what am I
    // looking at / describe this image" intent). The SAME OFF-by-default, opt-in
    // posture as self_heal/forge/standing/mcp/optimize/voice_id/docsearch — it is
    // DEVICE-GATED (needs mlx-vlm + a multi-GB VLM checkpoint download + enough
    // RAM), so it SHIPS DISABLED and the op honestly reports "unavailable" until
    // the operator both flips `enabled` AND names a `model`:
    //   - `enabled` (SHIPS OFF, false): master switch. With it false the describe
    //     intent NEVER calls the VLM — it falls back honestly (OCR/classification
    //     or "the model isn't downloaded"). Turn on deliberately.
    //   - `model` (SHIPS EMPTY): the VLM repo id ([models].vlm-style). EMPTY =>
    //     the server has no VLM to load and the op returns the honest unavailable
    //     structure; the daemon never fabricates a description.
    // The image is read ON-DEVICE by the inference server (pixels never leave the
    // device); DISTINCT from OCR (read.screen = text glyphs; VLM = visual
    // understanding). Listed here so neither key reads as a typo.
    ("vision", &["enabled", "model"]),
    // [image] — the OPTIONAL on-device TEXT->IMAGE generation path (task #18):
    // the inference `generate_image` op (MLX diffusion) plus the daemon
    // "generate/make/draw an image of X" intent. SAME OFF-by-default, opt-in
    // posture as [vision]/[self_heal]/[forge]/[standing]/[mcp]/[optimize].
    //   - `enabled` (SHIPS OFF): master switch. With it false the generate-image
    //     intent NEVER calls the op — it surfaces an honest "the on-device image
    //     model isn't set up" line. Turn on deliberately.
    //   - `model` (SHIPS EMPTY): the on-device diffusion model id (a
    //     FLUX.1-schnell-class mflux checkpoint). EMPTY => the server has no
    //     image model to load and the op returns the honest unavailable structure;
    //     the daemon NEVER fabricates an image and NEVER calls a cloud image API.
    // The prompt + the generated pixels stay ON-DEVICE (image generation is LOCAL
    // only — NO cloud image API). Listed here so neither key reads as a typo.
    ("image", &["enabled", "model"]),
    // [screen_context] — CONTINUOUS SCREEN CONTEXT (#42). `enabled` SHIPS OFF: the
    // device-gated continuous capture loop never runs and the bounded in-RAM ring
    // never grows on its own. `interval_secs` (cadence, floored >= 1) and `cap`
    // (the hard ring bound, evict-oldest, floored >= 1) tune the loop/ring. The
    // ring is redacted + transient (in-RAM only, off lifelong memory / optimizer /
    // disk) + forgettable; recall is read-only. Listed so no key reads as a typo.
    ("screen_context", &["enabled", "interval_secs", "cap"]),
    // [answers] — answer annotations (anthropic.rs `answers` module): the
    // always-cite source-tracking (#5) + the self-reported confidence (#8). The
    // SAME OFF-by-default, opt-in posture as self_heal/forge/standing/mcp/optimize/
    // voice_id/docsearch. BOTH ship OFF and are pinned:
    //   - `cite` (false): surface the REAL tool-result sources that fed a turn as a
    //     "Sources:" line — or "from my own knowledge" when no retrieval ran (never
    //     a fabricated citation). With it false the response is byte-for-byte
    //     today's.
    //   - `confidence` (false): ask the model to self-report grounded/inferred/
    //     uncertain + a one-line why, parsed + surfaced. The PLUMBING is gated; the
    //     model's calibration is runtime/model-behavior-gated (never claimed
    //     measured). With it false no instruction is added and the prompt is
    //     unchanged.
    //   - `verify` (false): the self-verification pass (#7). On an IMPORTANT turn,
    //     ONE extra self-critique of the draft against the real sources + AT MOST
    //     one bounded revise. With it false the response path is byte-for-byte
    //     today's and NO critique call is made. Gated (skips trivial turns) AND
    //     bounded (one critique + at most one revise, never loops). A second check
    //     REDUCES hallucination; it is NOT a correctness guarantee.
    //   - `cross_check` (false): #21 tool-result verification. A BOUNDED plausibility
    //     cross-check of a TOOL RESULT before it is surfaced as fact / built into a
    //     consequential action — deterministic sanity checks (empty-vs-claimed,
    //     uncited fact, self-contradiction, out-of-range) always run when on; it only
    //     DOWNGRADES confidence + FLAGS, NEVER removes a confirmation gate.
    //   - `cross_check_model_pass` (false): #21 optional single bounded "does this
    //     result look right?" model call, gated UNDER `cross_check` and OFF (a cost).
    //   - `debate` (false): #22 multi-model debate. For HIGH-STAKES asks only, a
    //     SECOND independent model answers the same question; agreement RAISES
    //     confidence, disagreement SURFACES BOTH (never picked/averaged), an
    //     unavailable second brain falls back to one + says so. ≤2 model calls.
    // Listed here so none of the keys reads as a typo.
    (
        "answers",
        &["cite", "confidence", "verify", "cross_check", "cross_check_model_pass", "debate"],
    ),
    // [audit] — the append-only, hash-chained, tamper-EVIDENT consequential-action
    // audit log (audit.rs). UNLIKE the autonomy switches (self_heal/forge/...),
    // `enabled` SHIPS ON (true): the log is READ-ONLY ACCOUNTABILITY — it never
    // takes an action, only records the decisions the consequential gate already
    // makes, secret-free and bounded. It is on-but-bounded (defensible: a record-
    // only ledger loosens nothing). With it false NO entry is written and the
    // chokepoints behave byte-for-byte as today (the audit calls are skipped).
    // `max_entries` bounds retention (prune-oldest + re-root past the cap). Listed
    // here so neither key reads as a typo.
    ("audit", &["enabled", "max_entries"]),
    // [policy] — the per-action policy store (policy.rs). The controlled, USER-SET
    // loosening/hardening that sits BENEATH the [integrations] master switch. It
    // SHIPS EMPTY: no rules => `evaluate` returns Ask for every action, so the
    // three consequential chokepoints behave EXACTLY as today (ASK/park
    // everywhere). Rules are USER-SET ONLY (Settings / the authenticated-local
    // command channel) — there is NO tool/agent/model path that can write a
    // policy. `enabled` is the master switch for the layer (ships ON, but inert
    // while the store is empty); with it false the layer is bypassed and every
    // action is Ask regardless of any saved rule. The rules themselves live in the
    // user-owned state/policy.json, NOT in this TOML (so the model can never reach
    // them via a config edit either). Listed here so the key never reads as a typo.
    ("policy", &["enabled"]),
    // [security] — AT-REST ENCRYPTION of the sensitive local stores (#11; crypto.rs
    // + the per-store `open_encrypted` seam). `encrypt_memory` is the master switch
    // and SHIPS OFF (false), exactly like self_heal/forge/standing/mcp/optimize/
    // voice_id/docsearch. With it false EVERY store opens via its plaintext
    // `open(path)` with NO `PRAGMA key` — byte-for-byte today's plaintext SQLite.
    // When the operator flips it true, a fresh 256-bit master key is generated +
    // stored in the macOS Keychain (account `memory_encryption_key`), the existing
    // plaintext stores are re-keyed to SQLCipher (migration), and every subsequent
    // open uses `open_encrypted`. HONESTY: SQLCipher protects AT REST ON DISK only
    // — NOT against a live-process/root attacker (key + decrypted pages are in RAM
    // while the daemon runs); the config TOML and the Keychain item itself are not
    // covered; lose the Keychain item => the DBs are unrecoverable. Listed here so
    // the key never reads as a typo.
    ("security", &["encrypt_memory"]),
    // [webhooks] — WEBHOOK TRIGGERS (#35, webhooks.rs). An INBOUND network surface,
    // so the SAME OFF-by-default posture as self_heal/forge/standing/mcp: `enabled`
    // SHIPS OFF (false) — with it false the loopback listener never binds and no
    // event is received. `bind` is the listen address (defaults to 127.0.0.1
    // loopback; a non-loopback value is refused at bind time). The HMAC secret is
    // resolved from the Keychain (account `webhook_hmac_secret`), NEVER inlined here.
    // `mappings` is an array-of-tables ([[webhooks.mappings]]) of explicit
    // event->intent allowlist entries; its per-entry keys are validated by
    // WebhookMapping's `deny_unknown_fields` at deserialize time, so only the
    // [webhooks] top-level keys are listed here. `max_body_bytes` bounds a request.
    (
        "webhooks",
        &[
            "enabled",
            "bind",
            "port",
            "max_body_bytes",
            "mappings",
        ],
    ),
    // [plugin_sdk] — PLUGIN SDK (#36, plugin_sdk.rs). The capability-module
    // contract validator + register-on-launch handshake. `enabled` SHIPS OFF
    // (false), exactly like self_heal/forge/standing/mcp: with it false the live
    // launch handshake does not scope a plugin's declared intents/tools (the
    // validator itself is pure and always available for inspection). Listed so the
    // key never reads as a typo.
    ("plugin_sdk", &["enabled"]),
    // [power] — BATTERY/THERMAL ADAPTIVE THROTTLING (#38, power.rs). `adaptive`
    // SHIPS OFF (false), exactly like self_heal/forge/standing/mcp: with it false
    // NOTHING reads power/thermal state and the throttle is always neutral
    // (today's routing). The bounds tune the conservative policy: `low_battery_pct`
    // is the discharge threshold below which (on battery) JARVIS prefers the
    // cheaper local Fast sub-tier + defers heavy work. Listed so neither reads as
    // a typo.
    ("power", &["adaptive", "low_battery_pct"]),
    // [report] — REPORT GENERATION (#40, report.rs). `enabled` SHIPS OFF (false):
    // with it false the read-only "generate a report on X" op declines and routing
    // is today's. The op folds already-cited notebook/research material into a
    // bounded markdown report under research.rs's cite discipline (every citation a
    // REAL source ref; uncited claims dropped; no citable source -> honest-empty).
    // Listed so the key never reads as a typo.
    ("report", &["enabled"]),
    // [chart] — DATA -> CHART (#41, chart.rs). `enabled` SHIPS OFF (false): with it
    // false the "chart this" op declines and emits nothing. When on it serializes a
    // ChartSpec (the EXACT data points) as a neutral `chart.data` telemetry envelope
    // the HUD plots exactly (no interpolation/invented point, honest axes/empty); it
    // changes no gate. Listed so the key never reads as a typo.
    ("chart", &["enabled"]),
];

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    pub rms_threshold: f64,
    pub silence_ms: u64,
    pub min_speech_ms: u64,
    /// Barge-in: let the user interrupt JARVIS mid-reply by speaking over him.
    pub barge_in: bool,
    /// RMS the user's voice must exceed DURING playback to count as a barge-in —
    /// set well ABOVE rms_threshold so JARVIS's own voice through the speakers
    /// (echo) cannot trip it. Device/volume dependent; tune on the real Mac
    /// (raise it if JARVIS cuts himself off; lower it if barge-in won't trigger).
    pub barge_in_rms: f64,
    /// How long (ms) the user must stay above barge_in_rms before JARVIS stops —
    /// a dwell so a cough/click/transient never cuts him off.
    pub barge_in_ms: u64,
    /// OPT-IN ambient sound monitor (task #15). When ON (and macOS mic/TCC
    /// consent is granted on-device) the daemon PERIODICALLY classifies a short
    /// ambient audio clip through the Vision app's on-device `classify.sound` op
    /// (Apple Sound Analysis, the fixed ~300-class SNClassifierIdentifier.version1)
    /// and emits sound-class events (name-called / doorbell / alarm / glass-break).
    /// SHIPS OFF (false) and is PINNED — exactly like self_heal/forge/standing/
    /// mcp/optimize/voice_id/docsearch/vision. With it OFF the monitor NEVER
    /// starts, the mic is never opened for ambient classification, and the audio
    /// path is byte-for-byte today's (one-shot "what was that sound" only, on a
    /// clip the daemon already captured). PRIVACY: continuous ambient listening is
    /// a liability, so it is opt-in + TCC/mic-gated and NEVER always-on without
    /// this explicit switch; only the sound-class LABELS (+ confidence) are ever
    /// emitted, the AUDIO never leaves the device. DISTINCT from STT (speech).
    pub sound_monitor: bool,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            rms_threshold: 0.015,
            silence_ms: 350,
            min_speech_ms: 250,
            barge_in: true,
            barge_in_rms: 0.06,
            barge_in_ms: 250,
            // SHIPS OFF — the opt-in ambient sound monitor never auto-starts.
            // Continuous ambient listening is a privacy liability, so it is
            // off-by-default + pinned; the one-shot "what was that sound" intent
            // (on an already-captured clip) needs no switch and works regardless.
            sound_monitor: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    pub llm: String,
    pub stt: String,
    /// Dedicated small resident model for op=classify; "" = reuse the main
    /// LLM. Consumed server-side; mirrored here so the Default impl stays in
    /// lockstep with jarvis.toml. Gated: only set after a candidate passes
    /// the 7-utterance accuracy eval (>=6/7, all heavy cases heavy).
    #[allow(dead_code)] // shared contract; read by the inference server
    pub classifier: String,
    /// MULTI-RESIDENT LOCAL warm-set (task #17): OPTIONAL extra local model ids
    /// the inference server keeps WARM alongside the base [models].llm so the
    /// Local tier can swap between them INSTANTLY (no reload) — a "local-fast"
    /// model for trivial offline turns and the capable base for harder ones.
    /// DEFAULT is EMPTY == single-resident: only `llm` is warm, exactly today's
    /// behavior and the safe state on a low-RAM Mac. Multi-resident is OPT-IN
    /// and RAM-BOUNDED (see `local_budget_gib`): the server admits an extra only
    /// while the running footprint estimate stays within budget, else it stays
    /// single-resident. Mirrors [models].local_warm in jarvis.toml + server.py.
    pub local_warm: Vec<String>,
    /// RAM budget (GiB of unified memory) the local warm-set may occupy. 0 (the
    /// CONSERVATIVE default) or any non-positive value => SINGLE-RESIDENT: only
    /// the base `llm` is kept warm regardless of `local_warm`. A positive budget
    /// lets the policy admit extras until their estimated footprints would exceed
    /// it. HONEST: two warm models cost ~2x RAM; the default keeps a low-RAM Mac
    /// (8GB M1) unaffected. The ESTIMATE drives only keep-warm bookkeeping — it is
    /// not a measurement and the swap speed benefit is device-gated.
    pub local_budget_gib: f64,
    /// OPTIONAL id -> approx resident GiB overrides for the budgeting policy,
    /// used when the coarse heuristic would mis-estimate a model. Mirrors
    /// [models].local_sizes; consumed both here (the HUD telemetry plan) and by
    /// server.py (the real keep-warm manager).
    pub local_sizes: std::collections::BTreeMap<String, f64>,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            llm: "mlx-community/Qwen3-4B-Instruct-2507-4bit".to_string(),
            stt: "mlx-community/whisper-small-mlx".to_string(),
            classifier: String::new(),
            // CONSERVATIVE single-resident default: no extra warm models, a 0
            // budget. A Mac left at the defaults keeps exactly one local model
            // warm (today's behavior). Multi-resident is opt-in + RAM-bounded.
            local_warm: Vec::new(),
            local_budget_gib: 0.0,
            local_sizes: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RouterConfig {
    pub cloud_confidence_threshold: f64,
    /// Where the CONVERSATION intent (casual chat, greetings, opinions — the
    /// llm_voice conversation path, NOT actions/stats/memory ops) is answered.
    /// "cloud_heavy" (the default): cloud Opus ([cloud].heavy_model) for
    /// genuinely varied, human personality — the local 4B is near-deterministic
    /// on bare greetings (a model-capacity ceiling). "cloud_fast": cloud Haiku
    /// ([cloud].fast_model). "local": the resident 4B (offline/Hulk path).
    /// The cloud variants require the cloud key — with no key, or on a cloud
    /// error, conversation degrades to the local 4B. Unknown values behave as
    /// "local" (the safe, always-available path). One line flips the brain.
    pub conversation_route: String,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self {
            cloud_confidence_threshold: 0.6,
            conversation_route: "cloud_heavy".to_string(),
        }
    }
}

/// The OFFLINE bounded tool-loop (task #3). When the conversation tier resolves
/// to Local (the "work offline" override, no cloud key, or a cloud-unreachable
/// fallback), the on-device 4B is OFFERED a CURATED SAFE local-tool subset and
/// run in a BOUNDED loop: prompt -> parse the 4B's tool call -> execute it
/// through the SAME gated `execute_tool` (so the consequential confirmation gate,
/// the voice-id gate, lockdown and per-action policy ALL still apply offline) ->
/// feed the result back -> at most `max_rounds` rounds. There is NO benefit /
/// chit-chat classifier gate: the subset is offered on every Local turn, the 4B
/// uses a tool when its reply parses as one, and otherwise the loop falls back
/// gracefully to a plain converse answer.
///
/// Defaults are conservative: ON (the offline path gains agency over SAFE local
/// tools only), 3 rounds, and the BUILT-IN curated subset (an empty `subset`).
/// The subset is local READ/COMPUTE only — it can never list an outward/cloud
/// tool (gmail/slack/web/etc.); a configured `subset` is INTERSECTED with the
/// curated safe set, so a misconfiguration can only ever NARROW it, never widen
/// it past the safe boundary. The cloud tool loop is entirely separate and
/// unchanged by these knobs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LocalToolsConfig {
    /// Engage the offline tool-loop at all. When false, a Local-tier
    /// conversation turn answers with today's plain converse (no tool use).
    pub enabled: bool,
    /// Hard ceiling on the number of (prompt -> tool) rounds before the loop is
    /// forced to a plain text answer. Bounded — there is no unbounded loop.
    pub max_rounds: u32,
    /// OPTIONAL allow-list override. Empty (the default) = the built-in curated
    /// safe subset. A non-empty list is INTERSECTED with the curated safe set
    /// (so it can only narrow, never reach an outward/cloud tool).
    pub subset: Vec<String>,
}

impl Default for LocalToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_rounds: 3,
            subset: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CloudConfig {
    pub fast_model: String,
    pub heavy_model: String,
    pub max_tokens: u32,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            fast_model: "claude-haiku-4-5".to_string(),
            heavy_model: "claude-opus-4-8".to_string(),
            max_tokens: 4096,
        }
    }
}

/// [speech] — neural TTS via the inference server's "speak" op. The daemon
/// passes `voice`, maps opener WAV indices back to `openers` text, and paces
/// clips with `sentence_pause_ms`; `engine` and `speed` are consumed
/// server-side but are mirrored here so the Default impl stays in lockstep
/// with jarvis.toml. `instant_opener` (default false) gates the canned
/// instant acknowledgment: off by default so the converse stream is the
/// whole, naturally-phrased reply.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SpeechConfig {
    #[allow(dead_code)] // shared contract; read by the inference server
    pub engine: String,
    /// Explicit HF repo for the engine; "" = the engine's default repo
    /// (resolved server-side from its engine registry).
    #[allow(dead_code)] // shared contract; read by the inference server
    pub model: String,
    pub voice: String,
    #[allow(dead_code)] // shared contract; read by the inference server
    pub speed: f64,
    /// Instant-acknowledgment lines. The server pre-synthesizes each entry to
    /// state/openers/opener-<idx>.wav at startup; the daemon plays one at
    /// utterance end and uses this list (by filename index) to tell the
    /// server which text already went out aloud (opener_spoken).
    pub openers: Vec<String>,
    /// Pure silence inserted between consecutive clips of one reply (after
    /// the opener and between content sentences; never after the last).
    pub sentence_pause_ms: u64,
    /// Opener breath: how long the daemon waits after an utterance ends
    /// before the instant acknowledgment fires. Runs CONCURRENTLY with
    /// transcription (never serialized in front of STT); first_audio_ms
    /// includes it naturally. Only consulted when `instant_opener` is true.
    pub opener_delay_ms: u64,
    /// Master gate for the instant acknowledgment. Ships OFF (false): a
    /// canned task-ack played before STT/classify ("Hi JARVIS" -> "Right
    /// away, sir") reads as programmed, so by default the converse stream IS
    /// the whole reply — JARVIS greets/answers naturally from its first word.
    /// When true the prior behavior holds: ReplySession::begin breathes
    /// `opener_delay_ms`, plays one `openers` clip, and passes opener_spoken
    /// to converse so the model continues from it. All the opener machinery
    /// stays intact either way; this flag only decides whether it engages.
    pub instant_opener: bool,
}

impl Default for SpeechConfig {
    fn default() -> Self {
        Self {
            engine: "kokoro".to_string(),
            model: String::new(),
            voice: "bm_george".to_string(),
            speed: 1.2,
            openers: [
                "Right away, sir.",
                "Of course.",
                "One moment.",
                "On it, sir.",
                "Let me see.",
            ]
            .map(String::from)
            .to_vec(),
            sentence_pause_ms: 250,
            opener_delay_ms: 300,
            instant_opener: false,
        }
    }
}

/// [voice] — the OPTIONAL ElevenLabs cloud VOICE TIER (voice_tier.rs). An ADDED
/// premium-TTS layer on top of the on-device Kokoro default ([speech].voice),
/// NEVER a replacement. Same OFF-by-default posture as self_heal/forge/standing:
///
///   - `cloud_tier` (ships FALSE): the master switch. With it false — OR with no
///     `elevenlabs_api_key` in the Keychain, OR when the runtime model-swap tier is
///     Local/"work offline" — TTS behaves EXACTLY as today: on-device Kokoro. The
///     ElevenLabs path is reachable ONLY when this is true AND a key is present AND
///     the active tier is non-Local. Honesty: when the tier is ON, the text to
///     synthesize LEAVES the device (a cloud round trip to api.elevenlabs.io);
///     on-device Kokoro is the private/offline default + the fallback on any error.
///   - `cloud_stt` (ships FALSE; build 2/2): the SEPARATE master switch for the
///     ElevenLabs Scribe cloud-STT tier. Gated independently of `cloud_tier` on
///     purpose — STT sends the user's VOICE AUDIO to the cloud, which is MORE
///     sensitive than the TTS text leg. With it false (OR no key, OR Local tier)
///     transcription is EXACTLY today's on-device mlx_whisper, which is also the
///     fallback on ANY Scribe error/offline. On-device whisper is the
///     private/offline default.
///   - `model` (default "eleven_flash_v2_5"): the ElevenLabs model id. Read by the
///     inference server when it makes the (credential+runtime-gated) TTS call.
///   - `voices` (default empty): a per-agent map, agent name -> ElevenLabs voice id.
///     An empty map or an unmapped agent falls back to that agent's Kokoro voice —
///     so turning the tier on with no mapping still works (every agent just keeps
///     its on-device voice until the operator maps it). VOICE-ONLY: this is a TTS
///     voice layer; JARVIS owns its own brain/router/turn-taking.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Master switch for the ElevenLabs cloud voice tier (TTS). SHIPS OFF (false):
    /// with it false TTS is exactly today's on-device Kokoro. An ADDED tier.
    pub cloud_tier: bool,
    /// Master switch for the ElevenLabs Scribe cloud-STT tier (build 2/2). SHIPS
    /// OFF (false) and is GATED INDEPENDENTLY of `cloud_tier`: STT sends the user's
    /// VOICE AUDIO to the cloud — MORE sensitive than the TTS text leg — so it has
    /// its own pinned switch. With it false (OR no `elevenlabs_api_key`, OR the
    /// model-swap tier is Local/"work offline") transcription is EXACTLY today's
    /// on-device mlx_whisper, which is also the fallback on ANY cloud error.
    pub cloud_stt: bool,
    /// The ElevenLabs model id used when the tier is active. Read server-side.
    pub model: String,
    /// Per-agent ElevenLabs voice ids (agent name -> EL voice id). Empty/unmapped
    /// -> that agent's Kokoro voice (the fallback). BTreeMap for deterministic
    /// iteration (stable tests / telemetry).
    pub voices: std::collections::BTreeMap<String, String>,
    /// #33 ADAPTIVE TONE / PROSODY (prosody.rs). SHIPS OFF (false): with it false
    /// the speak request is byte-for-byte today's NEUTRAL request on EVERY backend.
    /// With it ON, a PURE context->profile classifier picks a ProsodyProfile
    /// (Neutral|Calm|Urgent|Warm) and `shape_speak_request` emits ElevenLabs v3
    /// audio-tags + stability/style values ONLY when the resolved backend is
    /// ElevenLabs AND its model is v3-capable; on Kokoro (and non-v3 EL models) the
    /// mapping is COARSE / neutral — rich prosody is EL-v3-GATED and that limitation
    /// is stated honestly, NEVER faked. EXPRESSIVENESS-ONLY: changes delivery, never
    /// a gate/policy/autonomy surface.
    pub adaptive_prosody: bool,
    /// #34 WHISPER / DISCREET MODE (prosody.rs). SHIPS OFF (false): with it false the
    /// whisper state machine is inert and the request is unchanged. With it ON, an
    /// EXPLICIT command ("whisper mode" / "speak quietly" / "back to normal") toggles
    /// a terse + SOFT (low-volume) delivery. Whisper changes DELIVERY ONLY — it NEVER
    /// suppresses a safety confirmation the gate requires (a required confirm still
    /// speaks, just softly/tersely).
    pub whisper: bool,
    /// #34 OPTIONAL auto-engage of whisper mode by SUSTAINED low-amplitude input — a
    /// PURE energy-series heuristic. SHIPS OFF (false) and gated SEPARATELY from
    /// `whisper`: it does NOT open the mic here; it is a pure function over an energy
    /// series the audio layer already computes. With it false the only way into
    /// whisper is the explicit command.
    pub whisper_auto: bool,
    /// #31 MULTI-SPEAKER DIARIZATION (diarize.rs). SHIPS OFF (false): with it false the
    /// transcript is rendered exactly as today (a single stream, no speaker labels).
    /// With it ON, a PURE mapper CONSUMES the speaker labels the ElevenLabs SCRIBE STT
    /// backend reports (it carries per-word/segment speaker ids) into a diarized
    /// transcript. On-device whisper has NO diarization model, so the on-device path is
    /// an HONEST single-stream "speaker: unknown" labeling — it NEVER fabricates distinct
    /// speakers the backend did not report. Diarization is EL-Scribe-gated; that
    /// limitation is stated honestly, never faked.
    pub diarize: bool,
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            // OFF by default — the cloud tier is opt-in, exactly like self_heal/
            // forge/standing/mcp. Kokoro remains the default + fallback.
            cloud_tier: false,
            // OFF by default and pinned — the Scribe cloud-STT tier is opt-in and
            // separately gated (voice audio is more sensitive than TTS text).
            // mlx_whisper remains the default + fallback.
            cloud_stt: false,
            model: "eleven_flash_v2_5".to_string(),
            voices: std::collections::BTreeMap::new(),
            // #33 OFF by default — the prosody classifier is inert and the speak
            // request stays byte-for-byte today's neutral request on every backend.
            adaptive_prosody: false,
            // #34 OFF by default — the whisper state machine is inert; replies are
            // delivered exactly as today (no terse/soft shaping, no auto-engage).
            whisper: false,
            // #34 OFF by default and separately gated — the low-amplitude auto-engage
            // heuristic never trips; the only entry to whisper is the explicit command.
            whisper_auto: false,
            // #31 OFF by default — diarization is inert and the transcript is a single
            // stream exactly as today. EL-Scribe-gated when turned on; on-device whisper
            // stays an honest single-stream labeling (never fabricated speakers).
            diarize: false,
        }
    }
}

/// [wake] — CUSTOM WAKE-WORD (#32, wake.rs). The configured phrase that gates "is this
/// utterance for JARVIS". SHIPS OFF + defaults to "jarvis" so the default preserves
/// today's activation behavior; the PURE matcher is conservative (case/punct/whitespace-
/// insensitive + a small edit-distance tolerance; never matches an empty/blank phrase;
/// never triggers on a substring of a larger unrelated word). The always-listening loop
/// that consults the matcher is DEVICE-GATED.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WakeConfig {
    /// Master switch for custom-wake-word gating. SHIPS OFF (false): with it false the
    /// matcher gates NOTHING and activation is byte-for-byte today's. Turn on deliberately.
    pub enabled: bool,
    /// The wake phrase that gates activation. Defaults to "jarvis" so even when `enabled`
    /// is flipped on with no override, the default phrase preserves today's wake behavior.
    /// An empty/blank phrase NEVER matches (the matcher rejects it — fail-safe).
    pub phrase: String,
}

impl Default for WakeConfig {
    fn default() -> Self {
        Self {
            // OFF by default — wake-word gating is opt-in; with it off the matcher is
            // never consulted and activation is exactly today's.
            enabled: false,
            // Default phrase preserves today's behavior when the feature is turned on.
            phrase: "jarvis".to_string(),
        }
    }
}

/// [interpret] — CONTINUOUS LIVE INTERPRETATION (#30, interpret.rs). When `live` is ON the
/// DEVICE-GATED mic loop feeds each VAD segment through the PURE interpret_segment pipeline
/// (transcribe -> on-device-LLM translate -> render/optionally speak); offline/unavailable
/// degrades HONESTLY (never a fabricated translation). SHIPS OFF so the audio path is
/// byte-for-byte today's by default.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InterpretConfig {
    /// Master switch for the continuous live-interpret mode. SHIPS OFF (false): with it
    /// false the per-segment interpret pipeline NEVER runs from the mic loop. Turning it
    /// on is a deliberate, device-gated step.
    pub live: bool,
    /// Whether the rendered translation is also VOICED (through the single echo-safe speech
    /// path) in addition to being shown. SHIPS OFF (false): render-only by default.
    pub speak: bool,
    /// The SOURCE language to interpret FROM. Empty (the default) => auto-detect (the
    /// translator is told the source is unknown — Babel never claims to KNOW a source it
    /// only guessed).
    pub source_lang: String,
    /// The TARGET language to interpret INTO. Defaults to "English". An empty target is an
    /// honest "which language?" — never a fabricated rendering.
    pub target_lang: String,
}

impl Default for InterpretConfig {
    fn default() -> Self {
        Self {
            // OFF by default — continuous live interpretation is opt-in + device-gated.
            live: false,
            // Render-only by default — voicing the translation is a separate opt-in.
            speak: false,
            // Empty => auto-detect the source language (honest; never claimed-known).
            source_lang: String::new(),
            // A sensible default target so a turned-on interpreter has somewhere to go.
            target_lang: "English".to_string(),
        }
    }
}

/// [inference] — server-side knobs mirrored for the shared contract.
///
/// SPECULATIVE DECODING (#37) + SELECTABLE QUANTIZATION (#39) join `preload` as
/// PERF/RUNTIME knobs. Both ship OFF/neutral so the daemon's defaults are
/// byte-for-byte today's runtime behavior:
///   - `speculative` (ships false): the master gate for draft/speculative
///     decoding in the inference server's generate path. With it false the
///     server runs NORMAL generation exactly as today. Turning it on ALSO
///     requires a loadable `draft_model`; absent that the server honestly falls
///     back to normal generation and reports `speculative=false`. The real
///     speedup is device/model-dependent and is NEVER measured headlessly.
///   - `draft_model` (ships ""): the small DRAFT checkpoint mlx_lm uses to
///     propose tokens the main model verifies. Empty => speculative is inert
///     even if `speculative=true` (honest: no draft, normal gen).
///   - `quant` (ships "auto"): the requested on-device weight quantization for
///     the LOCAL model load. "auto" == today's behavior (load the model as
///     configured). An explicit value (fp16/int8/int4) asks the server to load
///     a matching quant variant; if that variant is not present the server
///     loads the available one and reports the quant that ACTUALLY loaded — it
///     never claims int4 when fp16 loaded. Validated below; an unknown value is
///     a parse issue and falls back to "auto" (today's behavior).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct InferenceConfig {
    #[allow(dead_code)] // shared contract; read by the inference server
    pub preload: bool,
    /// SPECULATIVE/DRAFT decoding master gate (#37). Ships OFF; off => normal
    /// generation, today's exact runtime. Read by the inference server's
    /// generate path AND by the daemon's `should_use_speculative` decision /
    /// HUD telemetry. The actual speedup is device/model-gated, never claimed
    /// headlessly.
    #[allow(dead_code)] // shared contract; read by the inference server + telemetry
    pub speculative: bool,
    /// Small DRAFT model id mlx_lm uses to propose tokens (#37). "" (default) =>
    /// no draft, so speculative is inert even when `speculative=true` (honest
    /// fallback to normal gen). A non-empty id is the checkpoint the server
    /// lazy-loads; if it cannot load, the server falls back to normal gen and
    /// reports `speculative=false`.
    #[allow(dead_code)] // shared contract; read by the inference server
    pub draft_model: String,
    /// SELECTABLE weight QUANTIZATION for the local model load (#39). "auto"
    /// (default) == today's behavior. Allowed: auto/fp16/int8/int4 (validated by
    /// `InferenceConfig::quant_is_valid`; an unknown value is reported as a
    /// config issue and kept at "auto"). The real RAM/speed/quality tradeoff is
    /// device-gated; the server reports the quant that ACTUALLY loaded.
    #[allow(dead_code)] // shared contract; read by the inference server
    pub quant: String,
}

impl InferenceConfig {
    /// The quantization values the contract allows. MUST match server.py's
    /// `ALLOWED_QUANT` / `validate_quant`. "auto" is the neutral default
    /// (today's behavior — load the model as configured, no quant override).
    pub const ALLOWED_QUANT: &'static [&'static str] = &["auto", "fp16", "int8", "int4"];

    /// Whether `q` is an allowed quantization value (PURE; mirrors the server's
    /// `validate_quant`). Used by the parse-time validation so an unknown value
    /// is reported and kept at the neutral "auto" default rather than passed to
    /// the server.
    pub fn quant_is_valid(q: &str) -> bool {
        Self::ALLOWED_QUANT.contains(&q)
    }
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            preload: true,
            // SHIPS OFF — speculative/draft decoding is opt-in + device-gated.
            // Off => normal generation, byte-for-byte today's runtime behavior.
            speculative: false,
            // No draft model => speculative is inert even if the gate is on
            // (honest: nothing to draft with, normal gen).
            draft_model: String::new(),
            // "auto" == today's behavior (load the model as configured); an
            // explicit quant is opt-in and device-gated.
            quant: "auto".to_string(),
        }
    }
}

/// [power] — BATTERY/THERMAL ADAPTIVE THROTTLING (#38). PERF/RUNTIME ONLY: this
/// never adds an outward surface, never loosens a gate, never makes a cloud call.
/// It only influences the LOCAL model-tier sub-choice (prefer the cheaper Fast
/// sub-tier + defer heavy work) when the machine is on a low battery or under
/// serious thermal pressure.
///
///   - `adaptive` (ships false): the master gate. With it false the LIVE power
///     reader (pmset/thermal/IOKit) is NEVER consulted, `throttle_decision` is
///     fed a neutral (None battery, on_ac=true, nominal thermal) reading, and the
///     resulting plan is always neutral — routing is byte-for-byte today's. The
///     real power read is DEVICE-GATED behind this flag.
///   - `low_battery_pct` (default 20): the discharge threshold below which, ON
///     BATTERY, the conservative policy prefers the Fast local sub-tier + defers
///     heavy work. On AC + nominal thermal the policy never throttles regardless.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PowerConfig {
    pub adaptive: bool,
    pub low_battery_pct: u8,
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — nothing reads power/thermal; the throttle stays neutral
            // (today's routing). Adaptive throttling is opt-in + device-gated.
            adaptive: false,
            // Conservative discharge threshold: below 20% on battery, prefer the
            // cheaper local Fast sub-tier + defer heavy work.
            low_battery_pct: 20,
        }
    }
}

/// [report] — REPORT GENERATION (#40, report.rs). The SAME OFF-by-default,
/// read-only posture: `enabled` SHIPS OFF (false). With it false the "generate a
/// report on X" op declines (it never builds), and routing is byte-for-byte
/// today's. When on, the op is READ-ONLY — it pulls the agent-scoped, already-cited
/// notebook/research material and folds it into a BOUNDED markdown report under
/// research.rs's cite discipline (every citation a REAL source ref an input claim
/// carried; an uncited claim DROPPED, never fabricated a source; no citable source
/// -> an HONEST-EMPTY report). It speaks/displays, acts/reaches nothing outward.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ReportConfig {
    pub enabled: bool,
}

impl Default for ReportConfig {
    fn default() -> Self {
        // SHIPS OFF — the report op is opt-in. Read-only/neutral when off.
        Self { enabled: false }
    }
}

/// [chart] — DATA -> CHART (#41, chart.rs). The SAME OFF-by-default, neutral
/// posture: `enabled` SHIPS OFF (false). With it false the "chart this" op declines
/// and emits nothing; behavior is byte-for-byte today's. When on, the op is a
/// NEUTRAL presentation act — it serializes a ChartSpec (the EXACT data points) as
/// a `chart.data` telemetry envelope the HUD plots exactly (no interpolation, no
/// invented/extrapolated point, honest axes + honest-empty). It changes no gate,
/// takes no action, reaches no network; the emit is fire-and-forget like every
/// other telemetry envelope.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ChartConfig {
    pub enabled: bool,
}

impl Default for ChartConfig {
    fn default() -> Self {
        // SHIPS OFF — the chart op is opt-in. Neutral when off.
        Self { enabled: false }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SelfHealConfig {
    /// Master gate. false (the shipped default): the watchdog only observes
    /// and emits heal.suppressed on error bursts — no cloud call, no patch.
    pub enabled: bool,
    /// "propose" (default): a validated patch is written to
    /// state/heal/proposals/<ts>/ with its report, meta.heal_pending is
    /// stamped, and a human applies it via scripts/apply_heal.sh <ts>.
    /// "auto" (DANGEROUS; additionally requires enabled = true): the daemon
    /// applies the validated patch to daemon/ itself, rebuilds --release,
    /// and EXITS cleanly so its supervisor restarts it — under launchd
    /// KeepAlive that is a restart, under `cargo run` it is a stop.
    /// Unknown values fall back to "propose" (the safe behavior).
    pub mode: String,
}

impl Default for SelfHealConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "propose".to_string(),
        }
    }
}

/// [optimize] — the optimization-from-usage loop (optimize.rs). The SAME
/// OFF-by-default, propose-only contract as [self_heal]/[forge], applied to
/// "learn better routing/selection from how interactions actually went":
///
///   - `enabled` (ships false): master gate. With it false the Trace Store's
///     recorder is a NO-OP — nothing is recorded, so no learning corpus
///     accrues and the optimizer has nothing to act on. Turn on deliberately,
///     exactly like self_heal/forge/standing/mcp.
///   - `mode` ("propose" default; "auto" reserved): the downstream Optimizer
///     phase reuses the self-heal posture — it PROPOSES a measured config/
///     prompt/example diff for human review+apply and NEVER silently mutates a
///     live config. The Trace Store itself acts on neither value; it only ever
///     records (when enabled) and reads. Unknown values fall back to "propose"
///     (the safe behavior).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OptimizeConfig {
    pub enabled: bool,
    pub mode: String,
}

impl Default for OptimizeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "propose".to_string(),
        }
    }
}

/// [episodic] — the EPISODIC STORE (episodic.rs): JARVIS's durable, redacted,
/// agent-scoped, BOUNDED memory of completed interactions, and the recall over
/// it. Unlike [self_heal]/[forge]/[optimize] (which ship OFF because they feed an
/// autonomous propose/learn loop), the episodic store ships **ON** — it is the
/// SAME posture as the always-on `transcripts` table and the lifelong-learning
/// fact loop: a per-completed-turn LOCAL record that powers READ-ONLY recall, not
/// any autonomous behavior. The honesty that earns the on-default:
///
///   - `enabled` (ships TRUE, default-on-but-BOUNDED): the master switch. When
///     true, a completed turn is recorded as an episode ONLY through the same
///     gates that already govern transcript/learning recording — a screen-read
///     TRANSIENT turn and a voice-id-UNVERIFIED turn are NEVER recorded, nor is
///     an empty/abandoned turn. Every field is REDACTED before store (reusing the
///     optimize::redact redactor), recall is AGENT-SCOPED (an episode stays in its
///     agent's scope), and retention is BOUNDED (evict-oldest past `retention`).
///     Turn it OFF to record no episodes at all; recall then returns nothing
///     (honest empty), it never fabricates one.
///   - `retention` (episodes_keep): the evict-oldest cap on the on-disk store —
///     the bounded-memory contract. The store remembers the RECENT past, NOT
///     "everything forever"; past the cap the OLDEST episodes are dropped by the
///     same retention pass that caps transcripts.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct EpisodicConfig {
    pub enabled: bool,
    pub retention: usize,
}

impl Default for EpisodicConfig {
    fn default() -> Self {
        Self {
            // Ships ON — the same posture as the always-on transcripts table /
            // lifelong-learning fact loop (NOT the OFF-by-default autonomy gates).
            // It is bounded, redacted, agent-scoped, gated, and forgettable.
            enabled: true,
            // Evict-oldest cap. Generous for a meaningful recent history, small
            // enough that the on-disk store stays tiny on the always-on appliance.
            retention: 5_000,
        }
    }
}

/// [notebooks] — RESEARCH NOTEBOOKS (notebook.rs): the persistent, redacted,
/// agent-scoped, BOUNDED store of SAGE research runs. A run is saved as a CITED
/// notebook entry {topic, synthesized text, the real fetched citations, ts}; the
/// user can REVISIT a notebook and APPEND a follow-up run to it (source memory
/// accrues). Same posture as [episodic]: always-on-but-bounded, NOT an autonomy
/// gate — a notebook is a READ-ONLY persisted record of a research run that
/// already happened, under the SAME cite-discipline research.rs enforces (a
/// notebook holds NO citation that was not in its run, never a fabricated one).
///
///   - `enabled` (ships TRUE, default-on-but-BOUNDED): the master switch. With it
///     false no run is saved and revisit returns an HONEST EMPTY (never fabricates).
///     The synthesized text is redacted before store, scope is agent-scoped, and
///     the store is forgettable (per-notebook or per-agent).
///   - `retention` (entries_keep): the evict-oldest cap on the on-disk store — the
///     bounded-memory contract. The store remembers the recent runs, NOT
///     "everything forever"; past the cap the OLDEST entries (and their citations)
///     are dropped by `memory::notebook_retention_pass`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NotebookConfig {
    pub enabled: bool,
    pub retention: usize,
}

impl Default for NotebookConfig {
    fn default() -> Self {
        Self {
            // Ships ON — same always-on-but-bounded posture as [episodic]. It is
            // bounded, redacted, agent-scoped, cited, and forgettable.
            enabled: true,
            // Evict-oldest ENTRIES cap. A research run is heavier than an episode
            // (a synthesis + a bibliography), so the entry cap is smaller than the
            // episodes cap while still holding a generous recent shelf.
            retention: 500,
        }
    }
}

/// [lifelog] — the LIFE-LOG DIGEST (lifelog.rs): a periodic (daily/weekly)
/// browsable summary built ONLY from the agent-scoped, redacted EPISODIC store.
/// Same posture as [episodic]/[notebooks]: always-on-but-bounded, NOT an autonomy
/// gate — the digest is a READ-ONLY, DETERMINISTIC fold over episodes that already
/// exist (it needs no model/network), and it NEVER fabricates: a window with no
/// episodes yields an HONEST EMPTY digest, a sparse window says exactly what little
/// it holds.
///
///   - `enabled` (ships TRUE, default-on-but-bounded): the master switch. With it
///     false the digest intent returns an honest "the life log is off". The digest
///     owns NO store of its own — its bound is the episodic store's bound, and
///     forgetting episodes empties it.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LifeLogConfig {
    pub enabled: bool,
}

impl Default for LifeLogConfig {
    fn default() -> Self {
        Self {
            // Ships ON — same always-on posture as [episodic]; it is a read-only,
            // never-fabricating fold over the bounded episodic store.
            enabled: true,
        }
    }
}

/// [docsearch] — ON-DEVICE FILE RAG (docsearch.rs): index + cosine/BM25 search
/// over the user's OWN text-like files, 100% on-device. The SAME OFF-by-default,
/// opt-in posture as [self_heal]/[forge]/[standing]/[mcp]/[optimize]/[voice_id]:
/// because it reads the user's files, it SHIPS DISABLED and indexes NOTHING until
/// the operator both flips `enabled` AND allowlists a root.
///
///   - `enabled` (SHIPS OFF, false): master switch. With it false the indexer is
///     inert — no walk, no read, no embed, no store. Turn on deliberately.
///   - `roots` (SHIPS EMPTY): the EXPLICIT allowlist of folders that may be
///     indexed. NEVER a whole-disk scan — even with `enabled` true, an empty
///     `roots` indexes nothing. Every candidate file is PATH-CONFINED (canonicalize
///     + assert it starts_with a canonicalized allowed root; symlink-escape / `..`
///     / absolute-elsewhere are REJECTED), so the index can never reach a file
///     outside an allowlisted root.
///   - `max_files` / `max_chunks` / `max_file_bytes` / `max_depth` /
///     `chunk_chars` / `chunk_overlap`: the BOUNDS — total files, total chunks,
///     per-file byte cap, recursion depth, chunk window size, and overlap. They
///     keep the on-disk store finite (bounded memory), exactly like the
///     [mcp]/[episodic] bounds. Hidden + binary + non-allowlisted-extension files
///     are skipped regardless.
///
/// HONESTY: file CONTENTS + EMBEDDINGS never leave the device — embedding is the
/// on-device MLX embed op and falls back to lexical BM25 when that server is down
/// (the search reports which actually ran). v1 indexes TEXT-LIKE files only; PDFs
/// and other binaries are OUT OF SCOPE (a PDF needs a parser dependency — they are
/// skipped, never silently "indexed"). The index is FORGETTABLE (clear it).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DocSearchConfig {
    pub enabled: bool,
    pub roots: Vec<String>,
    pub max_files: usize,
    pub max_chunks: usize,
    pub max_file_bytes: usize,
    pub max_depth: usize,
    pub chunk_chars: usize,
    pub chunk_overlap: usize,
    /// KNOWLEDGE GRAPH (knowledge_graph.rs): when true, the "build/map knowledge
    /// graph from my documents" intent (and an OPTIONAL auto-pass after a reindex)
    /// runs the conservative DETERMINISTIC extractor over the already-indexed
    /// chunks and UPSERTs the grounded entities/relationships into the SHARED
    /// `user.world.*` tier (provenance-tagged, deduped, bounded). SHIPS OFF —
    /// exactly like `enabled`. The graph build reads only chunks the confined,
    /// allowlisted indexer already produced and writes only the shared world tier;
    /// it never re-walks the disk and never writes an agent's private namespace.
    pub build_graph: bool,
}

impl Default for DocSearchConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — exactly like self_heal/forge/standing/mcp/optimize/voice_id.
            enabled: false,
            // SHIPS EMPTY — no folder is indexable until explicitly allowlisted; an
            // empty allowlist means "index nothing" even with `enabled` true.
            roots: Vec::new(),
            // Generous-but-finite ceilings; the master switch + empty roots are what
            // actually ship the subsystem off.
            max_files: 5_000,
            max_chunks: 50_000,
            // 2 MiB per file — large enough for source/notes, small enough to skip
            // accidental blobs.
            max_file_bytes: 2 * 1024 * 1024,
            // Recursion depth bound on the std::fs walk (root itself is depth 0).
            max_depth: 16,
            // ~1200-char overlapping windows keep a chunk focused yet citeable.
            chunk_chars: 1_200,
            chunk_overlap: 200,
            // SHIPS OFF — the knowledge-graph build never runs until explicitly
            // turned on (in addition to the [docsearch].enabled master switch).
            build_graph: false,
        }
    }
}

/// [code] — CODE INTELLIGENCE (code.rs): the read-only `code_explain` (a grounded,
/// CITED answer over the on-device docsearch code index) + the PROPOSE-ONLY
/// `code_propose_diff` (a reviewable unified diff written to
/// state/code/proposals/<ts>/ — it NEVER edits the user's tree). The SAME
/// OFF-by-default, opt-in posture as [self_heal]/[forge]/[standing]/[mcp]/
/// [optimize]/[voice_id]/[docsearch]: because it READS and PROPOSES EDITS to the
/// user's code, it SHIPS DISABLED and does NOTHING until the operator both flips
/// `enabled` AND allowlists a codebase root.
///
///   - `enabled` (SHIPS OFF, false): master switch. With it false `code_explain`
///     and `code_propose_diff` are inert — they report the feature is off and
///     touch nothing. Turn on deliberately.
///   - `roots` (SHIPS EMPTY): the EXPLICIT allowlist of codebase roots. NEVER an
///     arbitrary path. The human apply script (scripts/apply_code_diff.sh) writes
///     ONLY under a canonicalized root (confined BY CONSTRUCTION via sandbox-exec
///     deny-default-write), and `code_explain` answers only from the docsearch
///     index built over allowlisted roots. An empty allowlist means "no codebase
///     is reachable" even with `enabled` true.
///   - `max_diff_bytes`: the BOUND on a proposed diff's size, so the proposal
///     artifact stays finite (a degenerate/huge model diff is refused).
///
/// HONESTY: `code_explain` is GROUNDED + CITED — it answers ONLY from the real
/// indexed code chunks (file + offset) and never fabricates code that is not in
/// the index (an empty/no-match index => an honest "I don't have that indexed").
/// `code_propose_diff` is PROPOSE-ONLY — a reviewable diff to the proposal store,
/// NEVER an auto-edit; the apply is the human-reviewed, confined-by-construction
/// script. The model's diff QUALITY (does it compile/work) is runtime/model-gated
/// and NOT claimed measured. On-device-first: the code index is on-device; the
/// authoring model is per the active tier.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CodeConfig {
    pub enabled: bool,
    pub roots: Vec<String>,
    pub max_diff_bytes: usize,
}

impl Default for CodeConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — exactly like self_heal/forge/standing/mcp/optimize/
            // voice_id/docsearch. It both reads AND proposes edits to the user's
            // code, so it stays inert until deliberately enabled.
            enabled: false,
            // SHIPS EMPTY — no codebase is reachable until explicitly allowlisted;
            // an empty allowlist means "no code" even with `enabled` true. Never an
            // arbitrary path (the apply script confines writes to a canonicalized
            // root by construction).
            roots: Vec::new(),
            // 256 KiB — large enough for a substantial multi-file refactor diff,
            // small enough to refuse a degenerate/runaway model output.
            max_diff_bytes: 256 * 1024,
        }
    }
}

/// [shell] — the SANDBOXED SHELL / TERMINAL (#43), the HIGHEST-RISK capability:
/// arbitrary command execution. It ships OFF by default and is maximally gated by
/// construction — see [`crate::shell`] for the four hermetic layers (the
/// destructive DENYLIST, the DENY-DEFAULT sandbox-exec profile, the consequential
/// park + master/voice-id/lockdown gate routing) and the fifth, device-gated
/// exec seam (built, never invoked in a test).
///
///   - `enabled` (SHIPS OFF, false): the master switch. With it false the shell
///     intent is NEVER classified and `shell_run` is inert — it returns the honest
///     "shell is off" reply, parks nothing, and runs nothing. Turn on deliberately.
///
/// HONESTY: even with `enabled` true the tool NEVER auto-runs. Every command is
/// CONSEQUENTIAL (it is in `confirm::CONSEQUENTIAL_TOOLS`), so it parks for a
/// spoken human "yes" and only ever executes under the `[integrations]
/// .allow_consequential` master switch + the confirm + the voice-id owner gate +
/// `!is_locked_down()`. A destructive/denylisted command is refused PRE-exec and
/// never even parks. The actual execution is DEVICE-gated (it needs
/// `/usr/bin/sandbox-exec` + `/bin/sh` on-device) and is NOT claimed proven by the
/// hermetic tests. A command's output is NEVER fabricated.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ShellConfig {
    pub enabled: bool,
}

impl Default for ShellConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — exactly like self_heal/forge/standing/mcp/optimize/
            // voice_id/docsearch/code/vision. It is the highest-risk capability
            // (arbitrary execution), so it stays inert until deliberately enabled,
            // and even then is maximally gated (denylist + sandbox + park + master
            // switch + voice-id + lockdown). The exec itself is device-gated.
            enabled: false,
        }
    }
}

/// [ui_automation] — GATED UI AUTOMATION (#44, the CAPSTONE), the SINGLE MOST
/// DANGEROUS capability: actually ACTUATING the macOS UI (a synthetic click /
/// type / key combo). It ships OFF by default and is maximally gated by
/// construction — see [`crate::ui_automation`] for the layers (the PURE
/// single-action planner that can never batch, the consequential park PER ACTION
/// + master/voice-id/lockdown gate routing) and the device-gated actuation seam
/// (built, never invoked in a test, and itself behind an Accessibility-TCC consent
/// check).
///
///   - `enabled` (SHIPS OFF, false): the master switch. With it false the actuate
///     intent is NEVER classified and the `ui_actuate` tool is inert — it returns
///     the honest "UI automation is off" reply, plans nothing, parks nothing, and
///     actuates nothing. Turn on deliberately.
///
/// HONESTY: even with `enabled` true the tool NEVER auto-runs. EVERY actuation is
/// CONSEQUENTIAL (it is in `confirm::CONSEQUENTIAL_TOOLS`), so it parks PER ACTION
/// for a spoken human "yes" — ONE confirm authorizes EXACTLY ONE actuation; a
/// second re-parks — and only ever fires under the `[integrations]
/// .allow_consequential` master switch + the confirm + the voice-id owner gate +
/// `!is_locked_down()`. It is NEVER batched and NEVER autonomous. The actual
/// CGEvent/AX post is DEVICE-gated (it needs the Accessibility TCC consent —
/// runtime user consent, NOT SBPL-grantable — plus a real display) and is NOT
/// claimed proven by the hermetic tests. An actuation result is NEVER fabricated.
/// The Vision app stays READ-ONLY; this actuate op is a SEPARATE, maximally-gated
/// surface.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiAutomationConfig {
    pub enabled: bool,
}

impl Default for UiAutomationConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — like shell/self_heal/forge/code/vision. It is the single
            // most dangerous capability (physically actuating the UI), so it stays
            // inert until deliberately enabled, and even then is maximally gated
            // (single-action planner + per-action park + master switch + voice-id +
            // lockdown). The actuation itself is device-gated (Accessibility TCC).
            enabled: false,
        }
    }
}

/// [vision] — the OPTIONAL on-device VISION-LANGUAGE model (VLM) describe path:
/// the inference `describe_image` op plus the daemon "describe my screen / what
/// am I looking at / describe this image" intent (DISTINCT from the OCR
/// `read.screen` intent). The SAME OFF-by-default, opt-in posture as
/// [self_heal]/[forge]/[standing]/[mcp]/[optimize]/[voice_id]/[docsearch].
///
///   - `enabled` (SHIPS OFF, false): master switch. With it false the describe
///     intent NEVER calls the VLM — it FALLS BACK honestly (to the OCR
///     `read.screen` path / classification, or an honest "the vision-language
///     model isn't downloaded"). Turn on deliberately.
///   - `model` (SHIPS EMPTY): the on-device VLM repo id (a Qwen2-VL-class
///     mlx-vlm model). EMPTY => the server has no VLM to load and the op returns
///     the honest "vlm_unavailable" structure; the daemon NEVER fabricates a
///     description.
///
/// HONESTY: the VLM runs ON-DEVICE — the image's pixels go ONLY to the local
/// mlx-vlm and NEVER leave the device / never to the cloud. It is DEVICE-GATED:
/// it needs mlx-vlm installed + a multi-GB VLM checkpoint downloaded + enough
/// RAM (slow/absent on smaller chips), so it ships OFF and the op honestly
/// reports when the model isn't available. It is DISTINCT from OCR (OCR =
/// reading text glyphs off the screen; VLM = reasoning about the visual scene).
/// The op + wiring + fallback are tested; the actual description QUALITY is
/// device/runtime-gated and is NEVER claimed measured. No "it can see and
/// understand anything" overclaim.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VisionConfig {
    pub enabled: bool,
    pub model: String,
}

impl Default for VisionConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — exactly like self_heal/forge/standing/mcp/optimize/
            // voice_id/docsearch. The describe intent falls back honestly when off.
            enabled: false,
            // SHIPS EMPTY — no VLM is loaded until the operator names one (and
            // downloads it). Empty => the op honestly reports unavailable.
            model: String::new(),
        }
    }
}

/// [image] — the OPTIONAL on-device TEXT->IMAGE generation path (task #18): the
/// inference `generate_image` op (MLX diffusion) plus the daemon "generate /
/// make / draw an image of X" intent. The SAME OFF-by-default, opt-in posture as
/// [vision]/[self_heal]/[forge]/[standing]/[mcp]/[optimize]/[voice_id]/[docsearch].
///
///   - `enabled` (SHIPS OFF, false): master switch. With it false the generate-
///     image intent NEVER calls the op — it surfaces an honest "the on-device
///     image model isn't set up" line. Turn on deliberately.
///   - `model` (SHIPS EMPTY): the on-device diffusion model id (a FLUX.1-schnell-
///     class mflux checkpoint). EMPTY => the server has no image model to load and
///     the op returns the honest "image_model_unavailable" structure; the daemon
///     NEVER fabricates an image.
///
/// HONESTY: image generation runs 100% ON-DEVICE (MLX diffusion) — the prompt
/// and the generated pixels go ONLY to the local model and the image is saved
/// on-device under state/images/; NOTHING is sent to the cloud (there is NO cloud
/// image API anywhere on this path). It is DEVICE-GATED: it needs an MLX diffusion
/// package installed + a multi-GB checkpoint downloaded + enough RAM (slow/absent
/// on smaller chips), so it ships OFF and the op honestly reports when the model
/// isn't available. The op + wiring + fallback are tested; the actual image
/// QUALITY/speed are device/runtime-gated and are NEVER claimed measured.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ImageConfig {
    pub enabled: bool,
    pub model: String,
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — exactly like vision/self_heal/forge/standing/mcp/optimize/
            // voice_id/docsearch. The generate-image intent reports honestly when off.
            enabled: false,
            // SHIPS EMPTY — no diffusion model is loaded until the operator names
            // one (and downloads it). Empty => the op honestly reports unavailable.
            model: String::new(),
        }
    }
}

/// [screen_context] — CONTINUOUS SCREEN CONTEXT (#42, screen_context.rs): the
/// MOST privacy-sensitive READ feature. A bounded, redacted, transient in-RAM
/// ring of recent on-screen OCR snapshots, fed by a DEVICE-gated continuous
/// capture loop, recallable by a read-only "what was I working on" intent and
/// wipeable by "forget my screen context". The SAME OFF-by-default, opt-in,
/// device-gated posture as [vision]/[interpret].live/[wake].enabled, with EXTRA
/// privacy rails because the loop runs CONTINUOUSLY:
///
///   - `enabled` (SHIPS OFF, false): the master switch for the CONTINUOUS loop.
///     With it false NOTHING is ever captured continuously, the ring NEVER grows
///     on its own, no `screen_context.watching` indicator ever fires, and routing
///     is byte-for-byte today's behavior. Turning it on is a deliberate step AND
///     still requires runtime macOS Screen-Recording consent (TCC) — the flag
///     cannot grant the device permission, so on without consent captures nothing.
///   - `interval_secs` (DEFAULT 30): the cadence at which the device-gated loop
///     grabs ONE frame. Floored to >= 1 (a 0/negative would be a busy loop).
///   - `cap` (DEFAULT 50): the HARD bound on the in-RAM ring — past it the OLDEST
///     entry is evicted (no unbounded accumulation, no disk-spill). Floored to >= 1.
///
/// PRIVACY (every rail enforced, none weakenable here): OFF by default; the live
/// loop is TCC-device-gated; recognized text is REDACTED before it enters the ring
/// (the optimizer redactor, so an on-screen secret never survives) and is TRANSIENT
/// (in-RAM only — NEVER written to lifelong memory / optimizer traces / disk);
/// the ring is BOUNDED (evict-oldest at `cap`); FORGETTABLE ("forget my screen
/// context" wipes it); a PROMINENT HUD WATCHING indicator fires whenever the loop
/// is active; glyph/text ONLY (never a face/person id/embedding); the pixels NEVER
/// leave the device; READ-ONLY (recall describes, never actuates) and recall NEVER
/// fabricates context (an empty ring is an honest "no recent screen context").
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ScreenContextConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub cap: usize,
}

impl Default for ScreenContextConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — the continuous capture loop never runs until the operator
            // deliberately turns it on AND grants Screen-Recording consent (TCC).
            enabled: false,
            // A calm cadence — one frame every 30s when on. Floored to >= 1 at use.
            interval_secs: 30,
            // A hard bound on the in-RAM ring; evict-oldest past it. Floored to >= 1.
            cap: 50,
        }
    }
}

impl ScreenContextConfig {
    /// The effective ring cap (>= 1) — a misconfigured 0 would make the ring
    /// useless, so it is floored, never trusted raw.
    pub fn effective_cap(&self) -> usize {
        self.cap.max(1)
    }

    /// The effective capture interval in seconds (>= 1) — a 0/negative would be a
    /// busy loop, so it is floored.
    pub fn effective_interval_secs(&self) -> u64 {
        self.interval_secs.max(1)
    }
}

/// [answers] — answer annotations (anthropic.rs `answers` module): the
/// always-cite source-tracking (#5) and the self-reported confidence (#8). An
/// ADDED honesty layer over the answer, never a change to any safety gate.
///
///   - `cite` (SHIPS OFF, false): when on, a turn's answer is followed by a
///     "Sources:" line naming the REAL tool-result sources that actually fed it
///     (the citation-carrying reads — docsearch/unified/recall/episodic/web/
///     integration reads). When the turn used NO retrieval the answer is honestly
///     labeled "from my own knowledge" — NEVER a fabricated citation. With it
///     false the response is byte-for-byte today's.
///   - `confidence` (SHIPS OFF, false): when on, a bounded instruction asks the
///     model to end its answer with a self-reported confidence (grounded /
///     inferred / uncertain) + a one-line why; the daemon parses + surfaces it.
///     With it false no instruction is added and the prompt is unchanged.
///   - `verify` (SHIPS OFF, false): the self-verification pass (#7). When on, an
///     IMPORTANT turn (a factual / retrieval / consequential turn — the trivial
///     greeting/ack is skipped by the gating heuristic) gets ONE extra self-
///     critique of the DRAFT answer AGAINST the real sources the turn actually
///     used, and AT MOST one bounded revise/annotate when the critique flags an
///     unsupported claim. With it false the response path is byte-for-byte today's
///     and NO critique call is made. A second self-check REDUCES hallucination on
///     important turns; it is NOT a correctness guarantee, and it costs one extra
///     model call (a latency/cost tradeoff) — so it is gated AND bounded.
///
/// HONESTY: a citation maps to a REAL source that fed the turn (recorded by the
/// per-turn source accumulator from actual tool results), never invented; a
/// no-retrieval turn says "from my own knowledge". Confidence is the model's
/// SELF-REPORT under a gated prompt — the PLUMBING is what the daemon's tests
/// cover; the calibration QUALITY is runtime/model-behavior-gated and is never
/// claimed measured. The verify pass's critique QUALITY is likewise the model's
/// behavior (runtime/model-behavior-gated, never measured) — only the gating +
/// the bounded critique/revise PLUMBING is tested. All THREE ship OFF and are
/// pinned.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AnswersConfig {
    pub cite: bool,
    pub confidence: bool,
    pub verify: bool,
    /// #21 TOOL-RESULT VERIFICATION. The deterministic plausibility cross-check of
    /// a tool result before it is surfaced as fact / built into a consequential
    /// action.
    pub cross_check: bool,
    /// #21 OPTIONAL bounded model pass sub-flag — the single "does this result look
    /// right for this query?" model call, gated UNDER `cross_check` and OFF by
    /// default (it is a cost). The deterministic layer runs whenever `cross_check`
    /// is on; this only adds the model pass for important results.
    pub cross_check_model_pass: bool,
    /// #22 MULTI-MODEL DEBATE. The conservative, high-stakes-only two-brain debate
    /// + reconcile.
    pub debate: bool,
}

impl Default for AnswersConfig {
    fn default() -> Self {
        Self {
            // SHIP OFF — exactly like self_heal/forge/standing/mcp/optimize/
            // voice_id/docsearch. Honesty annotations are opt-in.
            cite: false,
            confidence: false,
            // SHIP OFF — the extra self-critique call is opt-in (latency/cost).
            verify: false,
            // SHIP OFF — #21 tool-result cross-check (deterministic + optional pass).
            cross_check: false,
            // SHIP OFF — #21 optional model pass (an extra call, under cross_check).
            cross_check_model_pass: false,
            // SHIP OFF — #22 multi-model debate (a second full model call).
            debate: false,
        }
    }
}

/// [voice_id] — on-device speaker verification (voiceid.rs). An ADDED safety
/// layer, never a replacement for the OFF-by-default [integrations]
/// allow_consequential master switch or the cross-turn confirmation gate.
///
///   - `enabled` (SHIPS OFF, false): master switch. With it false, OR with no
///     enrolled owner profile, behavior is UNCHANGED from today — `owner_verified`
///     is not enforced anywhere. Turn on deliberately, after explicitly enrolling.
///   - `threshold` (cosine accept on the acoustic embedding): the operating
///     point. Voice/device-dependent — NOT a measured FAR/FRR; tune on the real
///     mic. Higher = stricter (fewer false accepts, more false rejects).
///   - `min_enroll_samples`: how many owner utterances the explicit "enroll my
///     voice" flow captures before a profile is saved.
///   - `gate_scope` ("consequential" default | "all"): "consequential" gates only
///     outward/consequential actions + the confirmation replay (an unrecognized
///     speaker can't act outwardly nor approve a parked action); "all"
///     additionally blocks non-consequential commands. Unknown values fall back to
///     "consequential" (the safe default — never silently to "all").
///
/// HONESTY: this is a LIGHTWEIGHT acoustic model (filterbank statistics +
/// cosine), NOT a high-assurance biometric. It rejects an obviously different
/// voice but is spoofable by replay/impersonation. It FAILS CLOSED for
/// consequential actions (embed error / no usable audio while enabled+enrolled =>
/// treated as unverified, the consequential path is denied) but never bricks an
/// ordinary reply. Raw audio is never persisted; the profile is a local feature
/// vector only.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VoiceIdConfig {
    pub enabled: bool,
    pub threshold: f64,
    pub min_enroll_samples: usize,
    pub gate_scope: String,
}

impl Default for VoiceIdConfig {
    fn default() -> Self {
        Self {
            // SHIPS OFF — exactly like self_heal/forge/standing/mcp/optimize.
            enabled: false,
            // The shipped acoustic-embedding default; device-tuned in practice.
            threshold: 0.86,
            min_enroll_samples: 3,
            gate_scope: "consequential".to_string(),
        }
    }
}

/// [forge] — Self-Forge (forge.rs): JARVIS authoring a NEW sandboxed micro-app
/// from a goal. The SAME gated-codegen contract as [self_heal], generalized
/// from "patch the daemon" to "author an app":
///
///   - `enabled` (ships false): master gate. With it false the forge does
///     NOTHING — no cloud draft, no staging, no proposal — exactly like
///     self_heal/allow_consequential.
///   - `mode` ("propose" default; "auto" requires enabled = true): controls
///     what happens to the forge's OWN staged artifact. CRUCIAL DIFFERENCE
///     from self_heal: there is NO auto-DEPLOY path. Even in "auto" the forge
///     may at most do for its staged app what heal's auto does for its staged
///     patch; DEPLOYING a forged app into apps/ (where AppRegistry::discover
///     would pick it up and run it) is ALWAYS a separate human step — the
///     operator runs scripts/apply_forge.sh <ts> after reviewing. No code path
///     in the daemon ever moves a proposal into apps/. Unknown values fall back
///     to "propose" (the safe behavior).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ForgeConfig {
    pub enabled: bool,
    pub mode: String,
}

impl Default for ForgeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            mode: "propose".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct TelemetryConfig {
    pub port: u16,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self { port: 7177 }
    }
}

/// [proactive] — three distinct proactivity features share this section:
///   1. The first-contact brief (proactive.rs): when the user returns after
///      more than `idle_gap_hours` away, the next converse reply carries a
///      verified data brief for the persona to phrase. Gated by `enabled`.
///   2. EDITH's anticipation engine (anticipate.rs): the daemon surfaces what
///      matters UNPROMPTED. `speak` is its master switch for SPOKEN output and
///      ships OFF (false), exactly like self_heal/allow_consequential — with it
///      false EDITH only emits a HUD proactive card and NEVER speaks on its own.
///      `lead_minutes`/`unread_floor`/`quiet_start`/`quiet_end` tune the
///      relevance thresholds and quiet-hours band; the remaining guard knobs
///      (cooldown, rate limit) keep their conservative code defaults.
///   3. The proactive-intelligence suggester (proactive_intel.rs): the habit
///      detector (#13) + predictive suggester (#14). `suggest` is its OWN master
///      switch and ships OFF (false), exactly like `speak` and the other autonomy
///      gates — it does NOT piggyback on `enabled` (which ships ON purely to power
///      the first-contact brief). With `suggest` false the anticipation tick mines
///      NO patterns and emits NO `proactive.suggestion` card; the HUD feed renders
///      nothing. The suggester is OBSERVED-pattern-based + propose-only: even with
///      `suggest` on it only SURFACES suggestions — accepting a habit offer still
///      routes through the gated `standing_create` path.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProactiveConfig {
    pub enabled: bool,
    pub idle_gap_hours: u64,
    /// EDITH spoken-proactivity master switch. Ships OFF: HUD card only.
    pub speak: bool,
    /// Proactive-intelligence suggester master switch (habit detector #13 +
    /// predictive suggester #14, proactive_intel.rs). Ships OFF (false), its OWN
    /// gate independent of `enabled` — with it false the anticipation tick mines
    /// no patterns and emits no `proactive.suggestion` card. Mirrors `speak`'s
    /// OFF-by-default posture so the suggestion feed ships off by the documented
    /// gate, not by being dead code.
    pub suggest: bool,
    /// Surface a calendar event this many minutes away (or nearer).
    pub lead_minutes: i64,
    /// Surface important-unread mail at or above this count.
    pub unread_floor: u32,
    /// Quiet-hours band start (local hour, 0-23). Within [start, end) EDITH
    /// stays fully silent (no card either). Wraps midnight when start > end.
    pub quiet_start: u8,
    /// Quiet-hours band end (local hour, 0-23, exclusive). start == end
    /// disables quiet hours.
    pub quiet_end: u8,
}

impl Default for ProactiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_gap_hours: 4,
            // EDITH defaults mirror anticipate::Policy::default() (the pure
            // evaluator's conservative defaults): SPEAK OFF, 15-min lead,
            // 3-message unread floor, 22:00-07:00 quiet band.
            speak: false,
            // The proactive-intelligence suggester ships OFF, mirroring `speak`
            // and the other autonomy gates (self_heal/forge/standing/optimize/mcp
            // all default false). Independent of `enabled` so the suggestion feed
            // is gated by its OWN ships-off switch.
            suggest: false,
            lead_minutes: 15,
            unread_floor: 3,
            quiet_start: 22,
            quiet_end: 7,
        }
    }
}

/// [focus] — FOCUS PROFILES (#24, focus.rs). A focus profile is a
/// PERMISSION-NEUTRAL lens over JARVIS's proactive surfaces: it narrows WHICH
/// non-consequential intel reaches the user (which signal categories surface,
/// brief verbosity, whether suggestions are quieted) and can ONLY make JARVIS
/// quieter — never more permissive. By construction (focus.rs) a profile cannot
/// loosen the master switch / confirm gate / voice-id / lockdown / policy, cannot
/// enable a consequential action, and cannot raise autonomy.
///
/// `profile` ships "default" (the IDENTITY — today's behavior byte-for-byte), so
/// the feature ships NEUTRAL. Valid values: "default" | "work" | "sleep" |
/// "deep_focus" | any other string (a named CUSTOM profile, itself restrict-only).
/// A blank/"default" value is the identity; an UNRECOGNIZED non-blank value is a
/// named CUSTOM profile — which is itself restrict-only (it can only quiet, never
/// broaden), so a typo can never accidentally LOOSEN anything. Parsed by
/// `focus::FocusProfile::from_config_str`.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct FocusConfig {
    /// The active focus profile name. Ships "default" (the identity). Parsed by
    /// `focus::FocusProfile::from_config_str`; an unknown value is a named custom
    /// profile (restrict-only) and a blank degrades to "default".
    pub profile: String,
}

impl Default for FocusConfig {
    /// Ships NEUTRAL: the "default" profile is the identity, reproducing today's
    /// proactive behavior with no profile active.
    fn default() -> Self {
        FocusConfig {
            profile: "default".to_string(),
        }
    }
}

/// [apps] — the micro-app runtime substrate (docs/SANDBOX.md). `autostart`
/// lists micro-app names jarvisd launches at startup; it defaults to EMPTY —
/// nothing is autostarted unless the operator opts in. Names that do not match
/// a registered manifest are skipped with a telemetry warning at startup.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct AppsConfig {
    pub autostart: Vec<String>,
}

/// [integrations] — the shared Chart-2 integration substrate (integrations.rs).
/// `allow_consequential` is the master gate for side-effecting actions (post a
/// message, create an event): it ships OFF (false), exactly like [self_heal].
/// With it false a consequential action returns a DRY-RUN PREVIEW and performs
/// no side effect, even when the call site confirmed; only when an operator
/// flips it true AND the call confirms does the real action run.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct IntegrationsConfig {
    pub allow_consequential: bool,
}

/// [audit] — the append-only, hash-chained, tamper-EVIDENT audit log (audit.rs)
/// of every consequential decision (proposed / parked / blocked-by-policy /
/// auto-approved / confirmed / denied / executed). UNLIKE the autonomy switches,
/// `enabled` SHIPS ON: the log is READ-ONLY accountability — it never acts, only
/// records the decisions the gate already makes, secret-free (the target is
/// redacted) and bounded (prune-oldest + re-root past `max_entries`). With it
/// false NO entry is written and the chokepoints behave byte-for-byte as today.
///
/// HONESTY: the log is tamper-EVIDENT (a hash chain detects mutate/insert/delete/
/// reorder), not tamper-PROOF (a root attacker who can rewrite the whole on-disk
/// chain forward would still verify) — see audit.rs.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AuditConfig {
    /// Master switch for recording. SHIPS ON (read-only accountability loosens
    /// nothing). With it false the audit calls at the chokepoints are skipped.
    pub enabled: bool,
    /// Retention cap: past this many entries the oldest are pruned and the chain
    /// re-rooted (truncation keeps the surviving suffix consistent).
    pub max_entries: usize,
}

impl Default for AuditConfig {
    fn default() -> Self {
        // On by default (it only records), bounded by the audit module's cap.
        Self {
            enabled: true,
            max_entries: crate::audit::MAX_ENTRIES,
        }
    }
}

/// [policy] — the per-action policy store (policy.rs): the controlled, USER-SET
/// loosening/hardening BENEATH the [integrations] master switch. SHIPS EMPTY
/// (no rules => Ask everywhere => behavior is exactly today's). Rules are USER-SET
/// ONLY (Settings / the authenticated-local command channel); there is NO
/// tool/agent/model path that can write one, and the rules live in the user-owned
/// state/policy.json (never in this TOML), so the model can't reach them via a
/// config edit either. `enabled` is the layer master switch (ships ON but inert
/// while empty); with it false the layer is bypassed and every action is Ask.
///
/// INVARIANTS (enforced at the chokepoints, not here): a policy can NEVER grant an
/// action the master switch forbids (Always is inert under master OFF); a `Never`
/// rule HARD-BLOCKS even with master ON + a fresh confirmation; the voice-id +
/// confirmation gates remain backstops.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PolicyConfig {
    /// Master switch for the policy layer. SHIPS ON, but the store ships EMPTY so
    /// the layer is inert (Ask everywhere) until the USER sets a rule. With it
    /// false the layer is bypassed entirely (every action is Ask).
    pub enabled: bool,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        // On by default but inert: an empty store evaluates to Ask everywhere, so
        // the shipped behavior is byte-for-byte today's (ASK/park everywhere).
        Self { enabled: true }
    }
}

/// [security] — AT-REST ENCRYPTION of the sensitive local stores (crypto.rs). The
/// SAME OFF-by-default, opt-in posture as self_heal/forge/standing/mcp/optimize/
/// voice_id/docsearch: it CHANGES THE ON-DISK FORMAT, so it ships OFF and is turned
/// on deliberately.
///
///   - `encrypt_memory` (SHIPS OFF, false; PINNED): the master switch. With it
///     false EVERY sensitive store opens via its plaintext `open(path)` with NO
///     `PRAGMA key` — byte-for-byte today's plaintext SQLite (no behavior change,
///     no key, no migration). When the operator flips it true: a fresh 256-bit
///     master key is generated, written to the macOS Keychain (account
///     `memory_encryption_key`), the existing plaintext stores are re-keyed to
///     transparent whole-file SQLCipher AES-256 (a read-plaintext -> write-
///     encrypted migration), and every subsequent open uses `open_encrypted`.
///
/// SCOPE (be honest): ENCRYPTED = the four sensitive SQLite stores (the main Db in
/// memory.rs, docsearch.db, audit.db, the optimize trace store) + the voiceid owner
/// profile (wrapped in its own encrypted SQLCipher blob). NOT ENCRYPTED = the
/// config TOML, the Keychain item itself (already OS-protected), and — critically —
/// the IN-RAM working set + decrypted pages + the key WHILE THE DAEMON RUNS.
/// SQLCipher protects AT REST ON DISK only; it does NOT defend against a live-
/// process/root attacker. Lose the Keychain item => the DBs are unrecoverable.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SecurityConfig {
    /// Master switch for at-rest encryption. SHIPS OFF (false) and is pinned:
    /// with it false the stores are exactly today's plaintext SQLite.
    pub encrypt_memory: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        // OFF by default — enabling changes the on-disk format (migration), so it
        // is opt-in exactly like self_heal/forge/standing/mcp/optimize/docsearch.
        Self {
            encrypt_memory: false,
        }
    }
}

/// [webhooks] — WEBHOOK TRIGGERS (#35, webhooks.rs): an INBOUND network surface
/// that lets an external system trigger a JARVIS intent. The MOST security-
/// sensitive thing added here, so it ships with the strongest fences:
///
///   - `enabled` (SHIPS OFF, false): the subsystem master switch, exactly like
///     [mcp]/[self_heal]. With it false the loopback listener NEVER binds — no
///     port is opened and no event can be received, period. Turn on deliberately.
///   - `bind` (defaults to "127.0.0.1"): the listen address. Loopback-ONLY by
///     default; the listener refuses to bind a non-loopback address (the receiver
///     is for a local relay/tunnel, never a public internet listener).
///   - The HMAC secret is NEVER in this TOML. It resolves from the macOS Keychain
///     at account `webhook_hmac_secret` (the same `resolve_secret` machinery the
///     integrations use), so the shared secret never lands in a config/log/Debug.
///   - `mappings` (SHIPS EMPTY): the EXPLICIT event->intent allowlist. An event
///     not named here is REJECTED (never guessed). A mapping whose intent is
///     consequential PARKS for a spoken confirm — a webhook can never auto-execute.
///   - `port` / `max_body_bytes`: bounds (the listen port; a request body cap).
///
/// HONESTY: the live bind/accept-loop is RUNTIME-GATED (wired behind `enabled`,
/// not exercised in tests). The PURE `handle_webhook` decision — verify HMAC,
/// map via the allowlist, route-or-park — is proven hermetically with synthetic
/// signed requests. The secret/body are never logged.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebhooksConfig {
    /// Subsystem master switch. SHIPS OFF. With it false the listener never binds.
    pub enabled: bool,
    /// Listen address. SHIPS "127.0.0.1" (loopback). A non-loopback value is
    /// refused at bind time (`crate::webhooks::is_loopback_bind`).
    pub bind: String,
    /// Listen port for the loopback receiver.
    pub port: u16,
    /// Hard cap (bytes) on a received request body — a larger body is rejected
    /// rather than buffered, so an oversized POST can never wedge the receiver.
    pub max_body_bytes: usize,
    /// The EXPLICIT event->intent allowlist. SHIPS EMPTY — an unmapped event is
    /// rejected, never guessed. A mapping to a consequential intent still parks.
    pub mappings: Vec<WebhookMapping>,
}

impl Default for WebhooksConfig {
    fn default() -> Self {
        // OFF by default (no inbound surface); loopback bind; generous-but-finite
        // body cap; NO mappings (so even flipping `enabled` true accepts nothing
        // until an event->intent entry is added).
        Self {
            enabled: false,
            bind: "127.0.0.1".to_string(),
            port: 8723,
            max_body_bytes: 64 * 1024,
            mappings: Vec::new(),
        }
    }
}

/// One explicit event->intent allowlist entry. `deny_unknown_fields`: a mistyped
/// key is a parse error so a fat-fingered mapping can never silently widen the
/// surface (mirrors [`McpServerConfig`]).
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebhookMapping {
    /// The external event name (the `X-Jarvis-Event` header / `event` field) this
    /// entry maps. An inbound event whose name matches no mapping is REJECTED.
    pub event: String,
    /// The JARVIS intent the event routes to. If this intent is consequential
    /// (`crate::confirm::is_consequential_tool`) the routed action PARKS for a
    /// spoken confirm instead of executing — a webhook never auto-executes.
    pub intent: String,
}

impl Default for WebhookMapping {
    fn default() -> Self {
        Self {
            event: String::new(),
            intent: String::new(),
        }
    }
}

/// [plugin_sdk] — PLUGIN SDK (#36, plugin_sdk.rs): formalizes + VALIDATES the
/// micro-app capability-module contract — the optional `[intents]`/`[tools]`
/// block a plugin's `manifest.toml` declares (what intents it answers, what tools
/// it exposes, and the capability scopes it requests). `enabled` SHIPS OFF
/// (false), exactly like [mcp]/[self_heal]: with it false the register-on-launch
/// HANDSHAKE does not scope a plugin's declared intents/tools onto the live
/// router. The validator itself is PURE and always callable for inspection
/// (`validate_manifest`) — the flag governs the LIVE admission, not the check.
///
/// A plugin can NOT request a capability outside the allowed set (the validator
/// rejects an over-privileged manifest), can NOT escape the SBPL default-deny
/// profile (the existing [`AppManifest`] -> `generate_sbpl` derivation is
/// unchanged), and a consequential tool it exposes still rides the gate.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PluginSdkConfig {
    /// Master switch for the live register-on-launch handshake. SHIPS OFF (false).
    pub enabled: bool,
}

impl Default for PluginSdkConfig {
    fn default() -> Self {
        // OFF by default — the live handshake is opt-in, exactly like
        // self_heal/forge/standing/mcp. The validator is pure and always available.
        Self { enabled: false }
    }
}

/// [standing] — Standing Missions (standing.rs): durable, scheduled, autonomous
/// goals that run on the standing-missions scheduler tick (a dedicated runtime
/// loop, distinct from EDITH's anticipation tick) and reason over the World Model.
/// `enabled` is the subsystem MASTER switch and SHIPS OFF (false), exactly like
/// [self_heal].enabled / [forge].enabled / [proactive].speak. With it false the
/// pure scheduler ([`crate::standing::due_missions`]) marks NOTHING due, so no
/// standing mission ever fires on the live tick — standing autonomy is opt-in,
/// turned on deliberately. (Establishing a mission is independently
/// confirmation-gated, and every consequential step a RUN takes still parks
/// behind the confirmation gate + the [integrations] master switch, so even with
/// this on a mission can never auto-send/post/spend.)
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct StandingConfig {
    pub enabled: bool,
}

/// [drafts] — AUTO-DRAFT (#25, drafts.rs): compose a REVIEWABLE pending draft (an
/// email reply / message / doc) the user reads and then sends THEMSELVES through
/// the existing gated send. The SAME OFF-by-default posture as [self_heal] /
/// [forge] / [standing]: `enabled` SHIPS OFF (false). With it false JARVIS never
/// drafts PROACTIVELY (the anticipation/triage surfaces won't auto-compose), and
/// even with it on a draft is ONLY ever a suggestion: the draft module has NO send
/// path, so turning this on can never cause an autonomous send. An actual send is a
/// SEPARATE explicit action that rides the existing gate
/// ([integrations].allow_consequential && a fresh confirm) exactly like a normal
/// send. `retention` bounds the persisted pending-draft store (evict-oldest).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DraftsConfig {
    /// Master switch for PROACTIVE drafting. SHIPS OFF (false). A draft is always a
    /// reviewable suggestion — this never enables an autonomous send.
    pub enabled: bool,
    /// Evict-oldest cap on persisted pending drafts (bounded store).
    pub retention: usize,
}

impl Default for DraftsConfig {
    fn default() -> Self {
        // OFF by default (no proactive drafting); a generous bounded store.
        Self { enabled: false, retention: crate::drafts::DEFAULT_RETENTION }
    }
}

/// [missions] — DURABLE MISSIONS (#26, durable_missions.rs): persist FURY mission
/// state (a mission record + per-sub-task status) so a long campaign survives a
/// restart and can be resumed / listed / cancelled. The SAME OFF-by-default posture
/// as [self_heal] / [forge] / [standing]: `durable` SHIPS OFF (false). With it
/// false missions are in-memory exactly as today (nothing persists).
///
/// KEY SAFETY (enforced in durable_missions.rs, not here): (a) a persisted mission
/// does NOT auto-run on restart — it loads as PAUSED and the user must explicitly
/// `resume` it (no silent autonomy); (b) a resumed mission re-runs each
/// consequential sub-task step through the SAME gate (the persistence carries NO
/// pre-approval); (c) it inherits FURY's <=6 sub-task / 1-deep bounds. `retention`
/// bounds the persisted mission store (evict-oldest).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MissionsConfig {
    /// Master switch for PERSISTING mission state. SHIPS OFF (false). A persisted
    /// mission always loads PAUSED and re-gates its steps — this never enables
    /// auto-run.
    pub durable: bool,
    /// Evict-oldest cap on persisted missions (bounded store).
    pub retention: usize,
}

impl Default for MissionsConfig {
    fn default() -> Self {
        // OFF by default (in-memory missions, today's behavior); bounded store.
        Self { durable: false, retention: crate::durable_missions::DEFAULT_RETENTION }
    }
}

/// [macros] — MACRO RECORD/REPLAY (#27, macros.rs): record a NAMED sequence of
/// commands (the utterances/intent names ONLY — NEVER secrets, tokens, or resolved
/// credentials) and replay it. The SAME OFF-by-default posture as [self_heal] /
/// [forge] / [standing]: `enabled` SHIPS OFF (false). With it false no macro is
/// recorded or replayed.
///
/// KEY SAFETY (enforced in macros.rs + the router, not here): replay re-runs EACH
/// recorded command through the NORMAL router path + the gate EACH time — a
/// consequential step in a macro hits the confirmation gate + the master switch
/// FRESH, exactly as if spoken live (NO pre-approval, NO batching past the gate).
/// The store holds only the recorded utterance + classifier intent name; a secret
/// can never be persisted. `max_steps` bounds a single macro; `retention` bounds
/// the macro store (evict-oldest).
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MacrosConfig {
    /// Master switch for recording/replaying macros. SHIPS OFF (false).
    pub enabled: bool,
    /// Max commands one macro may hold (a bounded sequence).
    pub max_steps: usize,
    /// Evict-oldest cap on stored macros (bounded store).
    pub retention: usize,
}

impl Default for MacrosConfig {
    fn default() -> Self {
        // OFF by default; bounded per-macro and store-wide.
        Self {
            enabled: false,
            max_steps: crate::macros::DEFAULT_MAX_STEPS,
            retention: crate::macros::DEFAULT_RETENTION,
        }
    }
}

/// [skills] — the skill library (skills/). UNLIKE the other subsystem switches,
/// this one SHIPS ON: the in-tree skills are PURE + read-only, so offering them
/// is safe by default. `enabled` only governs whether the `skill_list` /
/// `skill_invoke` meta-tools are surfaced — a CONSEQUENTIAL skill is STILL parked
/// behind the cross-turn confirmation gate + the OFF-by-default [integrations]
/// allow_consequential switch when invoked, so this flag never lets a
/// side-effecting skill fire unconfirmed.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Master switch for the skill library. SHIPS ON (true) — pure skills are
    /// safe to offer. Set false to hide the meta-tools entirely.
    pub enabled: bool,
}

impl Default for SkillsConfig {
    fn default() -> Self {
        // Ships ON: the in-tree library is pure + read-only and safe by default.
        Self { enabled: true }
    }
}

/// [mcp] — Model Context Protocol client (mcp.rs). The most dangerous external
/// surface in JARVIS: an MCP server is a LOCAL PROCESS (or remote endpoint) that
/// offers tools JARVIS agents can call. `enabled` is the subsystem MASTER switch
/// and SHIPS OFF (false), exactly like [self_heal] / [forge] / [standing]: with
/// it false NO server connects and NO MCP tool exists, period. Turn on
/// deliberately, after configuring at least one `[[mcp.servers]]` entry.
///
/// Even with `enabled = true`, every CONSEQUENTIAL MCP tool still parks behind
/// the cross-turn confirmation gate + the OFF-by-default [integrations]
/// allow_consequential master switch, and a per-server `agents` allowlist
/// controls WHICH agents may use WHICH server. Unknown/mutating tools default to
/// CONSEQUENTIAL (fail-safe). The bounds below cap blast radius regardless.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Subsystem master switch. SHIPS OFF. With it false the manager is inert:
    /// no connect, no tool discovery, no tool call.
    pub enabled: bool,
    /// Max servers the manager will connect to at once (bound on fan-out).
    pub max_servers: usize,
    /// Max tools the manager will accept from any one server (bound on a server
    /// that floods `tools/list`).
    pub max_tools_per_server: usize,
    /// Per-call wall-clock ceiling, milliseconds. A server that is slow or hangs
    /// is abandoned at this bound — it never wedges the tool loop.
    pub call_timeout_ms: u64,
    /// Output-size cap, bytes, on any single server response. A response larger
    /// than this is rejected rather than buffered/returned.
    pub max_output_bytes: usize,
    /// The configured servers. SHIPS EMPTY — no server is defined by default, so
    /// even flipping `enabled` true connects to nothing until one is added.
    pub servers: Vec<McpServerConfig>,
}

impl Default for McpConfig {
    fn default() -> Self {
        // Bounds chosen as safe, generous-but-finite ceilings; the master switch
        // (enabled=false) is what actually ships the subsystem OFF.
        Self {
            enabled: false,
            max_servers: 8,
            max_tools_per_server: 64,
            call_timeout_ms: 30_000,
            max_output_bytes: 256 * 1024,
            servers: Vec::new(),
        }
    }
}

/// Transport for one MCP server. `stdio` spawns a local subprocess and exchanges
/// newline-delimited JSON-RPC over its stdin/stdout (the primary local
/// transport). `http` speaks MCP Streamable-HTTP/SSE to a remote HTTPS endpoint
/// (TLS-only; not SBPL-sandboxed — it runs elsewhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpTransportKind {
    Stdio,
    Http,
}

impl Default for McpTransportKind {
    fn default() -> Self {
        McpTransportKind::Stdio
    }
}

/// Default classification for a server's tools when the per-tool overrides do
/// not name one. `consequential` (the default-of-the-default) is fail-safe: an
/// undeclared tool is treated as side-effecting and parks behind the gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpToolClass {
    ReadOnly,
    Consequential,
}

impl Default for McpToolClass {
    fn default() -> Self {
        // Fail-safe: unknown -> consequential.
        McpToolClass::Consequential
    }
}

/// One configured MCP server. A server is INERT until `[mcp].enabled` is true AND
/// it is listed here. `deny_unknown_fields`: a mistyped key is a parse error so a
/// fat-fingered classification or allowlist can never silently widen the surface.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct McpServerConfig {
    /// Server id. Must be the strict shape `[a-z0-9_-]+` with no leading/trailing
    /// or consecutive separator — validated at CONNECT time (not on parse): a name
    /// that fails `integrations::is_safe_mcp_server_name` mints no Keychain account,
    /// so `McpManager::connectable_servers` filters it out and it never spawns a
    /// subprocess or resolves a token. Also the Keychain account stem
    /// (`mcp_<name>_token`) and the sandbox profile filename stem.
    pub name: String,
    /// stdio (local subprocess) or http (remote MCP Streamable-HTTP/SSE).
    pub transport: McpTransportKind,
    /// stdio: the absolute interpreter/binary to spawn. Ignored for http.
    pub command: String,
    /// stdio: argv after `command`. Ignored for http.
    pub args: Vec<String>,
    /// http: the endpoint URL. MUST be `https://` (TLS-only is enforced at
    /// connect so a bearer token never rides plaintext). Ignored for stdio.
    pub url: String,
    /// Optional: the server declares an auth token, resolved from the Keychain at
    /// `mcp_<name>_token` (never inline here, never logged). `false` (default) =
    /// no token. The token never appears in config, Debug, argv, or a URL.
    pub uses_token: bool,
    /// The JARVIS agents permitted to use this server's tools. Default: EMPTY —
    /// no agent may use it until explicitly listed (plus the orchestrator, which
    /// the manager always admits). NEVER auto-grants all agents.
    pub agents: Vec<String>,
    /// Default tool classification for this server when a tool is not named in
    /// `read_only_tools`. Defaults to consequential (fail-safe).
    pub default_class: McpToolClass,
    /// Tool names this server's config asserts are READ-ONLY (safe to call
    /// ungated). Everything else on the server takes `default_class`. An unknown
    /// tool not listed here is therefore consequential by default.
    pub read_only_tools: Vec<String>,
    /// stdio sandbox: extra absolute filesystem subpaths the server is granted
    /// READ access to in its default-deny seatbelt profile (beyond the command
    /// itself). Empty = the command's own dir only.
    pub fs_read: Vec<String>,
    /// stdio sandbox: extra absolute filesystem subpaths the server is granted
    /// WRITE access to. Empty = none.
    pub fs_write: Vec<String>,
    /// stdio sandbox: outbound TCP host-names the server may reach. Empty = NO
    /// network at all (default-deny). A network-needing stdio server must
    /// declare its hosts here, honestly narrowing the profile.
    pub net_hosts: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            transport: McpTransportKind::default(),
            command: String::new(),
            args: Vec::new(),
            url: String::new(),
            uses_token: false,
            agents: Vec::new(),
            default_class: McpToolClass::default(),
            read_only_tools: Vec::new(),
            fs_read: Vec::new(),
            fs_write: Vec::new(),
            net_hosts: Vec::new(),
        }
    }
}

impl Config {
    /// Load the config plus a list of human-readable issues (unknown keys,
    /// invalid sections). Issues are warned here immediately; the caller
    /// re-emits them as config.invalid telemetry once the hub exists —
    /// Config::load runs before telemetry::init, so emitting here would be
    /// silently dropped (audit fix: misconfiguration used to be a buried
    /// log WARN on an appliance whose only live signal is the HUD).
    pub fn load(path: &Path) -> (Config, Vec<String>) {
        match std::fs::read_to_string(path) {
            Ok(raw) => Self::parse(&raw),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No file is a supported state (hardcoded contract defaults),
                // not a misconfiguration.
                warn!(path = %path.display(), "config file missing; using contract defaults");
                (Config::default(), Vec::new())
            }
            Err(e) => {
                let issue = format!("config unreadable ({e}); using contract defaults");
                warn!(path = %path.display(), "{issue}");
                (Config::default(), vec![issue])
            }
        }
    }

    /// Parse with per-section fallback (audit fix): one wrong-typed key used
    /// to silently revert EVERY other customization to hardcoded defaults.
    /// Now a section that fails to deserialize falls back alone, every other
    /// section keeps its configured values, and unknown sections/keys are
    /// reported instead of vanishing.
    fn parse(raw: &str) -> (Config, Vec<String>) {
        let mut issues = Vec::new();
        let table: toml::Table = match raw.parse() {
            Ok(table) => table,
            Err(e) => {
                let issue = format!("config has a TOML syntax error ({e}); using contract defaults");
                warn!("{issue}");
                return (Config::default(), vec![issue]);
            }
        };

        // Unknown-key diagnostics: a typo'd section or key means the operator
        // believes a tuning change is active when it is not.
        for (section, value) in &table {
            match KNOWN_KEYS.iter().find(|(name, _)| name == section) {
                None => {
                    let issue = format!("unknown config section [{section}] ignored");
                    warn!("{issue}");
                    issues.push(issue);
                }
                Some((_, keys)) => {
                    if let Some(entries) = value.as_table() {
                        for key in entries.keys() {
                            if !keys.contains(&key.as_str()) {
                                let issue = format!("unknown config key {section}.{key} ignored");
                                warn!("{issue}");
                                issues.push(issue);
                            }
                        }
                    }
                }
            }
        }

        let cfg = Config {
            audio: section(&table, "audio", &mut issues),
            models: section(&table, "models", &mut issues),
            router: section(&table, "router", &mut issues),
            local_tools: section(&table, "local_tools", &mut issues),
            cloud: section(&table, "cloud", &mut issues),
            speech: section(&table, "speech", &mut issues),
            inference: section(&table, "inference", &mut issues),
            self_heal: section(&table, "self_heal", &mut issues),
            forge: section(&table, "forge", &mut issues),
            telemetry: section(&table, "telemetry", &mut issues),
            proactive: section(&table, "proactive", &mut issues),
            focus: section(&table, "focus", &mut issues),
            apps: section(&table, "apps", &mut issues),
            integrations: section(&table, "integrations", &mut issues),
            standing: section(&table, "standing", &mut issues),
            drafts: section(&table, "drafts", &mut issues),
            missions: section(&table, "missions", &mut issues),
            macros: section(&table, "macros", &mut issues),
            mcp: section(&table, "mcp", &mut issues),
            skills: section(&table, "skills", &mut issues),
            optimize: section(&table, "optimize", &mut issues),
            voice_id: section(&table, "voice_id", &mut issues),
            episodic: section(&table, "episodic", &mut issues),
            notebooks: section(&table, "notebooks", &mut issues),
            lifelog: section(&table, "lifelog", &mut issues),
            voice: section(&table, "voice", &mut issues),
            wake: section(&table, "wake", &mut issues),
            interpret: section(&table, "interpret", &mut issues),
            docsearch: section(&table, "docsearch", &mut issues),
            code: section(&table, "code", &mut issues),
            shell: section(&table, "shell", &mut issues),
            ui_automation: section(&table, "ui_automation", &mut issues),
            vision: section(&table, "vision", &mut issues),
            image: section(&table, "image", &mut issues),
            screen_context: section(&table, "screen_context", &mut issues),
            answers: section(&table, "answers", &mut issues),
            audit: section(&table, "audit", &mut issues),
            policy: section(&table, "policy", &mut issues),
            security: section(&table, "security", &mut issues),
            webhooks: section(&table, "webhooks", &mut issues),
            plugin_sdk: section(&table, "plugin_sdk", &mut issues),
            power: section(&table, "power", &mut issues),
            report: section(&table, "report", &mut issues),
            chart: section(&table, "chart", &mut issues),
        };

        // SELECTABLE QUANTIZATION (#39) value validation: an unknown [inference]
        // .quant (e.g. "int3", "8bit") is a misconfiguration — the operator
        // believes a quant override is active when it is not. Report it AND keep
        // the neutral "auto" default (today's behavior) rather than pass a bogus
        // value to the server. PURE; mirrors server.py's validate_quant reject.
        let mut cfg = cfg;
        if !InferenceConfig::quant_is_valid(&cfg.inference.quant) {
            let issue = format!(
                "inference.quant = {:?} is not one of {:?}; keeping \"auto\"",
                cfg.inference.quant,
                InferenceConfig::ALLOWED_QUANT,
            );
            warn!("{issue}");
            issues.push(issue);
            cfg.inference.quant = "auto".to_string();
        }
        (cfg, issues)
    }
}

/// Deserialize one named section, falling back to that section's defaults —
/// and recording the issue — when it is malformed. Missing sections are the
/// normal defaulted case, not an issue.
fn section<T: DeserializeOwned + Default>(
    table: &toml::Table,
    name: &str,
    issues: &mut Vec<String>,
) -> T {
    match table.get(name) {
        None => T::default(),
        Some(value) => match value.clone().try_into() {
            Ok(parsed) => parsed,
            Err(e) => {
                let issue = format!("config section [{name}] invalid ({e}); using defaults for this section only");
                warn!("{issue}");
                issues.push(issue);
                T::default()
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::Config;

    /// Audit fix: a single wrong-typed key must only revert ITS section —
    /// the old whole-file fallback silently discarded every other
    /// customization (voice, thresholds, telemetry port).
    #[test]
    fn bad_section_falls_back_alone() {
        let raw = r#"
            [audio]
            rms_threshold = "loud"   # wrong type: this section reverts

            [speech]
            voice = "bf_emma"

            [telemetry]
            port = 7999
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert_eq!(cfg.audio.rms_threshold, 0.015, "bad section -> its defaults");
        assert_eq!(cfg.speech.voice, "bf_emma", "good sections must survive");
        assert_eq!(cfg.telemetry.port, 7999);
        assert!(
            issues.iter().any(|i| i.contains("[audio]")),
            "the failed section must be reported: {issues:?}"
        );
    }

    #[test]
    fn unknown_sections_and_keys_are_reported_not_swallowed() {
        let raw = r#"
            [audio]
            rms_treshold = 0.02      # typo: must be diagnosed, value unused

            [telemtry]
            port = 7177
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert_eq!(cfg.audio.rms_threshold, 0.015, "typo'd key never applies");
        assert!(issues.iter().any(|i| i.contains("audio.rms_treshold")), "{issues:?}");
        assert!(issues.iter().any(|i| i.contains("[telemtry]")), "{issues:?}");
    }

    #[test]
    fn syntax_error_reverts_to_defaults_with_an_issue() {
        let (cfg, issues) = Config::parse("not [valid toml");
        assert_eq!(cfg.telemetry.port, 7177);
        assert_eq!(issues.len(), 1);
        assert!(issues[0].contains("syntax"));
    }

    #[test]
    fn clean_config_parses_with_no_issues() {
        let raw = r#"
            [proactive]
            enabled = true
            idle_gap_hours = 6

            [self_heal]
            enabled = false
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "{issues:?}");
        assert!(cfg.proactive.enabled);
        assert_eq!(cfg.proactive.idle_gap_hours, 6);
    }

    /// MULTI-RESIDENT LOCAL warm-set (task #17): the CONSERVATIVE default is
    /// SINGLE-RESIDENT — an empty warm-set + a 0 budget, exactly today's behavior
    /// and the safe state on a low-RAM Mac. This PINS that default so it cannot
    /// silently flip to multi-resident.
    #[test]
    fn local_warm_set_defaults_are_conservative_single_resident() {
        let cfg = Config::default();
        assert!(cfg.models.local_warm.is_empty(), "default warm-set must be empty");
        assert_eq!(cfg.models.local_budget_gib, 0.0, "default budget must be 0 (single-resident)");
        assert!(cfg.models.local_sizes.is_empty(), "default sizes table must be empty");
    }

    /// The multi-resident keys ARE known (no unknown-key diagnostic) and parse
    /// into ModelsConfig. A configured warm-set + budget round-trips cleanly.
    #[test]
    fn local_warm_set_keys_are_known_and_parse() {
        let raw = r#"
            [models]
            local_warm = ["mlx-community/Qwen3-0.6B-Instruct-4bit"]
            local_budget_gib = 3.0
            local_sizes = { "mlx-community/Qwen3-0.6B-Instruct-4bit" = 0.5 }
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(
            !issues.iter().any(|i| i.contains("models.local")),
            "local_* keys must be KNOWN (no unknown-key diagnostic): {issues:?}"
        );
        assert_eq!(cfg.models.local_warm, vec!["mlx-community/Qwen3-0.6B-Instruct-4bit"]);
        assert_eq!(cfg.models.local_budget_gib, 3.0);
        assert_eq!(
            cfg.models.local_sizes.get("mlx-community/Qwen3-0.6B-Instruct-4bit"),
            Some(&0.5)
        );
    }

    // --- #37 SPECULATIVE DECODING + #39 QUANTIZATION defaults (OFF/neutral) ----

    /// #37 + #39: the [inference] runtime knobs ship OFF/neutral so the defaults
    /// are byte-for-byte today's runtime. `speculative`=false + `draft_model`=""
    /// (no draft, normal gen) + `quant`="auto" (load as configured). This PINS the
    /// OFF/neutral default so it cannot silently flip on.
    #[test]
    fn inference_speculative_and_quant_default_off_neutral() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty(), "{issues:?}");
        assert!(cfg.inference.preload, "preload stays today's default (true)");
        assert!(
            !cfg.inference.speculative,
            "speculative MUST ship OFF (off => normal generation, today's runtime)"
        );
        assert!(
            cfg.inference.draft_model.is_empty(),
            "draft_model MUST ship empty (no draft => speculative inert)"
        );
        assert_eq!(
            cfg.inference.quant, "auto",
            "quant MUST ship \"auto\" (== today's behavior, load as configured)"
        );
    }

    /// #37 + #39: the new [inference] keys are KNOWN (no unknown-key diagnostic)
    /// and round-trip. A configured draft model + speculative + an allowed quant
    /// parse cleanly.
    #[test]
    fn inference_speculative_and_quant_keys_are_known_and_parse() {
        let raw = r#"
            [inference]
            speculative = true
            draft_model = "mlx-community/Qwen3-0.6B-Instruct-4bit"
            quant = "int4"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(
            !issues.iter().any(|i| i.contains("inference")),
            "[inference] keys must be KNOWN (no diagnostic): {issues:?}"
        );
        assert!(cfg.inference.speculative);
        assert_eq!(cfg.inference.draft_model, "mlx-community/Qwen3-0.6B-Instruct-4bit");
        assert_eq!(cfg.inference.quant, "int4");
    }

    /// #39: every allowed quant value validates; an unknown value is REJECTED by
    /// the pure validator (mirrors server.py's validate_quant accept/reject).
    #[test]
    fn quant_validator_accepts_allowed_rejects_unknown() {
        for q in ["auto", "fp16", "int8", "int4"] {
            assert!(super::InferenceConfig::quant_is_valid(q), "{q} must be allowed");
        }
        for q in ["int3", "8bit", "bf16", "INT4", "", "fp32"] {
            assert!(!super::InferenceConfig::quant_is_valid(q), "{q} must be rejected");
        }
    }

    /// #39: an UNKNOWN [inference].quant is reported as a config issue AND kept at
    /// the neutral "auto" default — never passed bogus to the server (honest: the
    /// operator believes a quant override is active when it is not).
    #[test]
    fn unknown_quant_reported_and_falls_back_to_auto() {
        let raw = r#"
            [inference]
            quant = "int3"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(
            issues.iter().any(|i| i.contains("quant") && i.contains("int3")),
            "an unknown quant must be reported: {issues:?}"
        );
        assert_eq!(
            cfg.inference.quant, "auto",
            "an unknown quant must fall back to the neutral default, never pass through"
        );
    }

    /// #38: [power] adaptive throttling ships OFF (nothing reads power; routing is
    /// today's), with the conservative low_battery_pct = 20 default. The keys are
    /// KNOWN and round-trip.
    #[test]
    fn power_adaptive_defaults_off_and_keys_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty(), "{issues:?}");
        assert!(
            !cfg.power.adaptive,
            "[power].adaptive MUST ship OFF (off => nothing reads power, today's routing)"
        );
        assert_eq!(cfg.power.low_battery_pct, 20);

        let raw = r#"
            [power]
            adaptive = true
            low_battery_pct = 15
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(
            !issues.iter().any(|i| i.contains("power")),
            "[power] keys must be KNOWN (no diagnostic): {issues:?}"
        );
        assert!(cfg.power.adaptive);
        assert_eq!(cfg.power.low_battery_pct, 15);
    }

    /// Contract lockstep: [proactive] defaults are enabled=true,
    /// idle_gap_hours=4 — exactly what config/jarvis.toml ships.
    #[test]
    fn proactive_defaults_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.proactive.enabled);
        assert_eq!(cfg.proactive.idle_gap_hours, 4);
    }

    /// Contract lockstep: [proactive].speak (EDITH's spoken-proactivity master
    /// switch) ships OFF (false) — exactly like self_heal / allow_consequential
    /// — so EDITH only ever emits a HUD card unless an operator opts in. The key
    /// (and the EDITH tuning keys) must parse without an unknown-key diagnostic,
    /// and flipping speak on must take.
    #[test]
    fn proactive_speak_defaults_off_and_edith_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.proactive.speak,
            "EDITH must ship without unprompted SPEECH (HUD card only)"
        );
        // The conservative tuning defaults.
        assert_eq!(cfg.proactive.lead_minutes, 15);
        assert_eq!(cfg.proactive.unread_floor, 3);
        assert_eq!(cfg.proactive.quiet_start, 22);
        assert_eq!(cfg.proactive.quiet_end, 7);

        let raw = r#"
            [proactive]
            speak = true
            lead_minutes = 30
            unread_floor = 5
            quiet_start = 23
            quiet_end = 6
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "EDITH keys must all be known: {issues:?}");
        assert!(cfg.proactive.speak);
        assert_eq!(cfg.proactive.lead_minutes, 30);
        assert_eq!(cfg.proactive.unread_floor, 5);
        assert_eq!(cfg.proactive.quiet_start, 23);
        assert_eq!(cfg.proactive.quiet_end, 6);
    }

    /// Lockstep with the SHIPPED file: config/jarvis.toml must parse with
    /// zero diagnostics and carry exactly the contract defaults the structs
    /// fall back to — if either side drifts, this fails.
    #[test]
    fn shipped_config_file_parses_cleanly_and_matches_defaults() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("config")
            .join("jarvis.toml");
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        let (cfg, issues) = Config::parse(&raw);
        assert!(issues.is_empty(), "shipped config has diagnostics: {issues:?}");
        let defaults = Config::default();
        assert_eq!(cfg.self_heal.enabled, defaults.self_heal.enabled);
        assert_eq!(cfg.self_heal.mode, defaults.self_heal.mode);
        assert_eq!(cfg.forge.enabled, defaults.forge.enabled);
        assert_eq!(cfg.forge.mode, defaults.forge.mode);
        assert!(!cfg.forge.enabled, "self-forge ships OFF");
        assert_eq!(cfg.forge.mode, "propose");
        assert_eq!(cfg.answers.cite, defaults.answers.cite);
        assert_eq!(cfg.answers.confidence, defaults.answers.confidence);
        assert_eq!(cfg.answers.verify, defaults.answers.verify);
        assert!(!cfg.answers.cite, "answer citations ship OFF");
        assert!(!cfg.answers.confidence, "answer confidence ships OFF");
        assert!(!cfg.answers.verify, "answer self-verification ships OFF");
        assert_eq!(cfg.proactive.enabled, defaults.proactive.enabled);
        assert_eq!(cfg.proactive.idle_gap_hours, defaults.proactive.idle_gap_hours);
        assert_eq!(cfg.proactive.speak, defaults.proactive.speak);
        assert!(!cfg.proactive.speak, "EDITH spoken proactivity ships OFF");
        assert_eq!(cfg.proactive.suggest, defaults.proactive.suggest);
        assert!(!cfg.proactive.suggest, "proactive-intel suggester ships OFF");
        assert_eq!(cfg.cloud.heavy_model, defaults.cloud.heavy_model);
        assert_eq!(cfg.telemetry.port, defaults.telemetry.port);
        assert_eq!(cfg.speech.instant_opener, defaults.speech.instant_opener);
        assert_eq!(
            cfg.integrations.allow_consequential,
            defaults.integrations.allow_consequential
        );
        assert_eq!(cfg.standing.enabled, defaults.standing.enabled);
        assert!(!cfg.standing.enabled, "standing missions ship OFF");
        // [code] (task #16): code intelligence ships OFF with no allowlisted root.
        assert_eq!(cfg.code.enabled, defaults.code.enabled);
        assert!(!cfg.code.enabled, "code intelligence ships OFF (it reads + proposes edits to code)");
        assert!(cfg.code.roots.is_empty(), "no codebase root is allowlisted by default");
        assert_eq!(cfg.code.max_diff_bytes, defaults.code.max_diff_bytes);
        assert!(cfg.code.max_diff_bytes > 0, "the proposed-diff size bound is finite");
        assert_eq!(
            cfg.router.cloud_confidence_threshold,
            defaults.router.cloud_confidence_threshold
        );
        assert_eq!(cfg.router.conversation_route, defaults.router.conversation_route);
    }

    /// CONTINUOUS SCREEN CONTEXT (#42): [screen_context] ships OFF — the most
    /// privacy-sensitive read feature must NEVER run continuously without an
    /// explicit enable. Prove the default + the empty-config parse are OFF, the
    /// bounds are sane (cap >= 1, interval >= 1), and the keys are known (no
    /// unknown-key diagnostic). PRIVACY PIN: this is the OFF-default guarantee.
    #[test]
    fn screen_context_ships_off_with_sane_bounds_and_known_keys() {
        // The Default impl is OFF.
        let d = super::ScreenContextConfig::default();
        assert!(!d.enabled, "continuous screen context MUST ship OFF by default");
        assert_eq!(d.cap, 50);
        assert_eq!(d.interval_secs, 30);
        assert!(d.effective_cap() >= 1);
        assert!(d.effective_interval_secs() >= 1);

        // An empty config (no [screen_context] block) parses to OFF, no diagnostic.
        let (cfg, issues) = Config::parse("");
        assert!(
            !cfg.screen_context.enabled,
            "an absent [screen_context] block leaves the continuous loop OFF"
        );
        assert!(
            issues.iter().all(|i| !i.contains("screen_context")),
            "the [screen_context] keys must be known (no unknown-key diagnostic): {issues:?}"
        );

        // The keys take + a misconfigured 0 cap/interval is FLOORED, never trusted.
        let (cfg, issues) = Config::parse(
            "[screen_context]\nenabled = true\ninterval_secs = 0\ncap = 0\n",
        );
        assert!(issues.is_empty(), "valid keys parse clean: {issues:?}");
        assert!(cfg.screen_context.enabled);
        assert_eq!(cfg.screen_context.effective_cap(), 1, "a 0 cap is floored to 1");
        assert_eq!(
            cfg.screen_context.effective_interval_secs(),
            1,
            "a 0 interval is floored to 1 (never a busy loop)"
        );

        // A typo'd key under [screen_context] IS flagged (lockstep with KNOWN_KEYS).
        let (_cfg, issues) = Config::parse("[screen_context]\nenable = true\n");
        assert!(
            issues.iter().any(|i| i.contains("enable")),
            "a typo'd [screen_context] key must be diagnosed: {issues:?}"
        );
    }

    /// Contract lockstep: [router].conversation_route ships "cloud_heavy" —
    /// conversation is answered by cloud Opus by default (the local 4B is the
    /// offline fallback). The key must parse without an unknown-key diagnostic,
    /// and the other two allowed values must take.
    #[test]
    fn conversation_route_defaults_cloud_heavy_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert_eq!(
            cfg.router.conversation_route, "cloud_heavy",
            "conversation must default to cloud Opus"
        );

        for value in ["cloud_fast", "local", "cloud_heavy"] {
            let raw = format!("[router]\nconversation_route = \"{value}\"\n");
            let (cfg, issues) = Config::parse(&raw);
            assert!(
                issues.is_empty(),
                "conversation_route must be a known key: {issues:?}"
            );
            assert_eq!(cfg.router.conversation_route, value);
        }
    }

    /// Contract lockstep: [speech].instant_opener ships OFF (false) — the
    /// canned task-ack is gated behind it so "Hi JARVIS" gets a naturally
    /// phrased greeting, not a programmed acknowledgment. The key must parse
    /// without an unknown-key diagnostic, and flipping it on must take.
    #[test]
    fn instant_opener_defaults_off_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.speech.instant_opener,
            "the canned opener must ship OFF"
        );

        let raw = r#"
            [speech]
            instant_opener = true
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "instant_opener must be a known key: {issues:?}");
        assert!(cfg.speech.instant_opener);
    }

    /// Contract lockstep: [self_heal] ships enabled=false, mode="propose" —
    /// exactly what config/jarvis.toml carries — and both keys parse without
    /// unknown-key diagnostics.
    #[test]
    fn self_heal_defaults_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.self_heal.enabled, "self-heal must ship OFF");
        assert_eq!(cfg.self_heal.mode, "propose");

        let raw = r#"
            [self_heal]
            enabled = true
            mode = "auto"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "mode must be a known key: {issues:?}");
        assert!(cfg.self_heal.enabled);
        assert_eq!(cfg.self_heal.mode, "auto");
    }

    /// Contract lockstep: [optimize] ships enabled=false, mode="propose" — the
    /// optimization-from-usage gate is OFF by default, the SAME shape as
    /// [self_heal]/[forge] — and both keys parse without unknown-key
    /// diagnostics. With enabled=false the Trace Store recorder is a no-op
    /// (enforced in optimize.rs); this only pins the gate + key spelling.
    #[test]
    fn optimize_defaults_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.optimize.enabled, "the optimizer must ship OFF");
        assert_eq!(cfg.optimize.mode, "propose");

        let raw = r#"
            [optimize]
            enabled = true
            mode = "auto"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "enabled+mode must be known keys: {issues:?}");
        assert!(cfg.optimize.enabled);
        assert_eq!(cfg.optimize.mode, "auto");
    }

    /// Contract lockstep: [answers] ships cite=false, confidence=false, verify=false
    /// — the answer-annotation + self-verification gates are OFF by default (the SAME
    /// OFF-by-default posture as [self_heal]/[forge]/[voice_id]/[docsearch]) — and all
    /// three keys parse without unknown-key diagnostics. With them off the response is
    /// byte-for-byte today's (enforced in anthropic.rs); this pins the gate + key
    /// spelling. A typo is diagnosed, not silently swallowed.
    #[test]
    fn answers_defaults_off_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.answers.cite, "answer citations must ship OFF");
        assert!(!cfg.answers.confidence, "answer confidence must ship OFF");
        assert!(!cfg.answers.verify, "answer self-verification must ship OFF");

        // The operator can turn them on — all three known keys.
        let raw = r#"
            [answers]
            cite = true
            confidence = true
            verify = true
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "answers keys must be known: {issues:?}");
        assert!(cfg.answers.cite);
        assert!(cfg.answers.confidence);
        assert!(cfg.answers.verify);

        // A typo'd answers key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[answers]\nciteee = true\n");
        assert!(
            issues.iter().any(|i| i.contains("answers.citeee")),
            "a typo'd answers key must be reported: {issues:?}"
        );
        // The verify key, too, is spell-checked.
        let (_cfg, issues) = Config::parse("[answers]\nverifyy = true\n");
        assert!(
            issues.iter().any(|i| i.contains("answers.verifyy")),
            "a typo'd verify key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [audit] ships enabled=TRUE (default-on-but-bounded
    /// read-only accountability — a record-only ledger loosens nothing, the SAME
    /// posture as [episodic]), with max_entries defaulting to the audit module's
    /// cap. Both keys parse without an unknown-key diagnostic, and a typo is
    /// diagnosed. With it false the chokepoints behave byte-for-byte as today
    /// (enforced in audit.rs / anthropic.rs); this only pins the gate + spelling.
    #[test]
    fn audit_defaults_on_and_bounded_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.audit.enabled, "the audit log must ship ON (read-only accountability)");
        assert_eq!(
            cfg.audit.max_entries,
            crate::audit::MAX_ENTRIES,
            "the default retention cap is the audit module's bound"
        );

        let raw = r#"
            [audit]
            enabled = false
            max_entries = 500
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "audit keys must be known: {issues:?}");
        assert!(!cfg.audit.enabled);
        assert_eq!(cfg.audit.max_entries, 500);

        let (_cfg, issues) = Config::parse("[audit]\nenable = true\n");
        assert!(
            issues.iter().any(|i| i.contains("audit.enable")),
            "a typo'd audit key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [policy] ships enabled=TRUE but the rule store ships
    /// EMPTY (the rules live in the user-owned state/policy.json, NOT this TOML),
    /// so the layer is INERT by default — every action evaluates to Ask, the SAME
    /// behavior as today (ASK/park everywhere). The `enabled` key parses without an
    /// unknown-key diagnostic, and a typo is diagnosed. USER-SET ONLY: the rules
    /// are deliberately NOT a config key, so the model can never reach a policy via
    /// a config edit; with enabled=false the layer is bypassed (every action Ask).
    #[test]
    fn policy_layer_enabled_but_ships_empty_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.policy.enabled, "the policy layer ships ON (but inert while the store is empty)");

        let raw = r#"
            [policy]
            enabled = false
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "the policy.enabled key must be known: {issues:?}");
        assert!(!cfg.policy.enabled);

        // There is deliberately NO rules key in the TOML — a 'rules' key under
        // [policy] is an unknown key (the rules are user-set via state/policy.json,
        // never the model-reachable config), so an attempt to inject rules here is
        // diagnosed and ignored.
        let (_cfg, issues) = Config::parse("[policy]\nrules = [\"allow gmail_send\"]\n");
        assert!(
            issues.iter().any(|i| i.contains("policy.rules")),
            "policy rules are NOT a config key (user-set only via state/policy.json): {issues:?}"
        );
    }

    /// Contract lockstep: [episodic] ships enabled=TRUE (default-on-but-bounded),
    /// the SAME always-on posture as the transcripts table / lifelong-learning
    /// fact loop — NOT the OFF-by-default autonomy gates ([self_heal]/[forge]/
    /// [optimize]/[voice_id]). The honest default is documented in EpisodicConfig:
    /// it is bounded (evict-oldest `retention`), redacted, agent-scoped, gated
    /// per-turn, and forgettable, so on-by-default never means "remembers
    /// everything forever". Both keys parse without an unknown-key diagnostic, and
    /// a typo is diagnosed.
    #[test]
    fn episodic_defaults_on_and_bounded_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.episodic.enabled,
            "the episodic store SHIPS ON (same posture as transcripts/lifelong-learning)"
        );
        assert_eq!(cfg.episodic.retention, 5_000, "bounded evict-oldest cap by default");
        assert!(cfg.episodic.retention > 0, "retention must be a real bound, never unbounded");

        // The operator can turn it OFF and retune the bound — both known keys.
        let raw = r#"
            [episodic]
            enabled = false
            retention = 1000
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "episodic keys must be known: {issues:?}");
        assert!(!cfg.episodic.enabled);
        assert_eq!(cfg.episodic.retention, 1000);

        // A typo'd episodic key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[episodic]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("episodic.enabledd")),
            "typo'd episodic key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [notebooks] ships enabled=TRUE with a bounded evict-oldest
    /// `retention` — the SAME always-on-but-bounded posture as [episodic] (a
    /// notebook is a persisted, cited, READ-ONLY record of a research run, not an
    /// autonomy gate). Both keys parse without an unknown-key diagnostic; a typo is
    /// diagnosed; the operator can turn it OFF and retune the bound.
    #[test]
    fn notebooks_default_on_and_bounded_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.notebooks.enabled, "research notebooks SHIP ON (same posture as episodic)");
        assert!(cfg.notebooks.retention > 0, "retention must be a real bound, never unbounded");
        assert_eq!(cfg.notebooks.retention, 500, "bounded evict-oldest entries cap by default");

        let raw = r#"
            [notebooks]
            enabled = false
            retention = 100
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "notebooks keys must be known: {issues:?}");
        assert!(!cfg.notebooks.enabled);
        assert_eq!(cfg.notebooks.retention, 100);

        let (_cfg, issues) = Config::parse("[notebooks]\nretentionn = 1\n");
        assert!(
            issues.iter().any(|i| i.contains("notebooks.retentionn")),
            "typo'd notebooks key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [lifelog] ships enabled=TRUE — the SAME always-on posture
    /// as [episodic] (the digest is a read-only, never-fabricating fold over the
    /// bounded episodic store; it owns no store, so its only bound is the episodic
    /// bound). The key parses without an unknown-key diagnostic; a typo is diagnosed.
    #[test]
    fn lifelog_defaults_on_and_key_matches_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(cfg.lifelog.enabled, "the life-log digest SHIPS ON (read-only over episodic)");

        let (cfg, issues) = Config::parse("[lifelog]\nenabled = false\n");
        assert!(issues.is_empty(), "the lifelog.enabled key must be known: {issues:?}");
        assert!(!cfg.lifelog.enabled);

        let (_cfg, issues) = Config::parse("[lifelog]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("lifelog.enabledd")),
            "typo'd lifelog key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [forge] ships enabled=false, mode="propose" — the
    /// Self-Forge gate is OFF by default, the SAME shape as [self_heal] — and
    /// both keys parse without unknown-key diagnostics. (The "no auto-DEPLOY"
    /// guarantee is enforced in forge.rs, not config; this only pins the gate.)
    #[test]
    fn forge_defaults_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.forge.enabled, "self-forge must ship OFF");
        assert_eq!(cfg.forge.mode, "propose");

        let raw = r#"
            [forge]
            enabled = true
            mode = "auto"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "forge keys must be known: {issues:?}");
        assert!(cfg.forge.enabled);
        assert_eq!(cfg.forge.mode, "auto");

        // A typo'd forge key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[forge]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("forge.enabledd")),
            "typo'd forge key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [standing] ships enabled=false — the Standing-Missions
    /// subsystem master switch is OFF by default, exactly like
    /// [self_heal].enabled / [forge].enabled / [proactive].speak — and the key
    /// parses without an unknown-key diagnostic. A typo'd key is diagnosed.
    #[test]
    fn standing_enabled_defaults_off_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.standing.enabled,
            "standing missions must ship OFF (no silent recurring autonomy)"
        );

        let raw = r#"
            [standing]
            enabled = true
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "standing.enabled must be a known key: {issues:?}");
        assert!(cfg.standing.enabled);

        // A typo'd standing key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[standing]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("standing.enabledd")),
            "typo'd standing key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (#25): [drafts] ships enabled=false — proactive drafting is
    /// OFF by default. A draft is always a reviewable suggestion (the module has no
    /// send path), so the flag never enables an autonomous send. Keys parse without
    /// an unknown-key diagnostic; a typo is diagnosed.
    #[test]
    fn drafts_enabled_defaults_off_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.drafts.enabled, "proactive drafting must ship OFF");
        assert_eq!(cfg.drafts.retention, crate::drafts::DEFAULT_RETENTION);

        let raw = "[drafts]\nenabled = true\nretention = 10\n";
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "drafts keys must be known: {issues:?}");
        assert!(cfg.drafts.enabled);
        assert_eq!(cfg.drafts.retention, 10);

        let (_cfg, issues) = Config::parse("[drafts]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("drafts.enabledd")),
            "typo'd drafts key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (#26): [missions] ships durable=false — durable persistence
    /// is OFF by default (missions are in-memory exactly as today). A persisted
    /// mission loads PAUSED and re-gates on resume; the flag governs persistence
    /// only, never autonomy. Keys parse cleanly; a typo is diagnosed.
    #[test]
    fn missions_durable_defaults_off_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.missions.durable, "durable missions must ship OFF");
        assert_eq!(cfg.missions.retention, crate::durable_missions::DEFAULT_RETENTION);

        let raw = "[missions]\ndurable = true\nretention = 5\n";
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "missions keys must be known: {issues:?}");
        assert!(cfg.missions.durable);
        assert_eq!(cfg.missions.retention, 5);

        let (_cfg, issues) = Config::parse("[missions]\ndurablee = true\n");
        assert!(
            issues.iter().any(|i| i.contains("missions.durablee")),
            "typo'd missions key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (#27): [macros] ships enabled=false — macro record/replay is
    /// OFF by default. Replay re-runs each command through the router + the gate
    /// fresh; the store holds only utterances + intent names (never a secret). Keys
    /// parse cleanly; a typo is diagnosed.
    #[test]
    fn macros_enabled_defaults_off_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.macros.enabled, "macros must ship OFF");
        assert_eq!(cfg.macros.max_steps, crate::macros::DEFAULT_MAX_STEPS);
        assert_eq!(cfg.macros.retention, crate::macros::DEFAULT_RETENTION);

        let raw = "[macros]\nenabled = true\nmax_steps = 4\nretention = 7\n";
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "macros keys must be known: {issues:?}");
        assert!(cfg.macros.enabled);
        assert_eq!(cfg.macros.max_steps, 4);
        assert_eq!(cfg.macros.retention, 7);

        let (_cfg, issues) = Config::parse("[macros]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("macros.enabledd")),
            "typo'd macros key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [security].encrypt_memory ships FALSE (pinned) — at-rest
    /// encryption is OFF by default, exactly like self_heal/forge/standing/mcp/
    /// optimize/voice_id/docsearch. With it off every store opens plaintext
    /// (byte-for-byte today's behavior); the key parses without an unknown-key
    /// diagnostic and a typo is reported.
    #[test]
    fn security_encrypt_memory_defaults_off_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.security.encrypt_memory,
            "at-rest encryption must ship OFF (enabling changes the on-disk format)"
        );

        let raw = r#"
            [security]
            encrypt_memory = true
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "security.encrypt_memory must be a known key: {issues:?}");
        assert!(cfg.security.encrypt_memory);

        // A typo'd security key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[security]\nencrypt_memoryy = true\n");
        assert!(
            issues.iter().any(|i| i.contains("security.encrypt_memoryy")),
            "typo'd security key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [voice_id] ships enabled=false — speaker verification
    /// is OFF by default, exactly like self_heal/forge/standing/mcp/optimize — and
    /// every key parses without an unknown-key diagnostic. With it off (or no
    /// enrolled profile) NOTHING is gated by voice.
    #[test]
    fn voice_id_defaults_off_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.voice_id.enabled, "voice-id must ship OFF (no silent voice gating)");
        // Sensible, finite defaults.
        assert!(cfg.voice_id.threshold > 0.0 && cfg.voice_id.threshold < 1.0, "threshold in (0,1)");
        assert!(cfg.voice_id.min_enroll_samples >= 1, "needs at least one enroll sample");
        assert_eq!(cfg.voice_id.gate_scope, "consequential", "default gates consequential only");

        // All keys parse as known and round-trip.
        let raw = r#"
            [voice_id]
            enabled = true
            threshold = 0.9
            min_enroll_samples = 5
            gate_scope = "all"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "voice_id keys must be known: {issues:?}");
        assert!(cfg.voice_id.enabled);
        assert!((cfg.voice_id.threshold - 0.9).abs() < 1e-12);
        assert_eq!(cfg.voice_id.min_enroll_samples, 5);
        assert_eq!(cfg.voice_id.gate_scope, "all");

        // A typo'd voice_id key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[voice_id]\nthreshholdd = 0.5\n");
        assert!(
            issues.iter().any(|i| i.contains("voice_id.threshholdd")),
            "typo'd voice_id key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [docsearch] ships enabled=false AND roots=[] — on-device
    /// file RAG is OFF AND indexes nothing by default, exactly like
    /// self_heal/forge/standing/mcp/optimize/voice_id, with the EXTRA guard that an
    /// empty allowlist means "index nothing" even if `enabled` were flipped. Every
    /// key parses without an unknown-key diagnostic, every bound is finite (never
    /// unbounded), and a typo is diagnosed.
    #[test]
    fn docsearch_defaults_off_empty_roots_and_bounded() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.docsearch.enabled, "file RAG must ship OFF (it reads the user's files)");
        assert!(cfg.docsearch.roots.is_empty(), "no folder is indexable by default (no whole-disk scan)");
        // Every bound is a real, finite ceiling — never unbounded.
        assert!(cfg.docsearch.max_files > 0, "max_files must be a real bound");
        assert!(cfg.docsearch.max_chunks > 0, "max_chunks must be a real bound");
        assert!(cfg.docsearch.max_file_bytes > 0, "max_file_bytes must be a real bound");
        assert!(cfg.docsearch.max_depth > 0, "max_depth must be a real bound");
        assert!(cfg.docsearch.chunk_chars > 0, "chunk_chars must be a real bound");
        assert!(
            cfg.docsearch.chunk_overlap < cfg.docsearch.chunk_chars,
            "overlap must be smaller than the chunk window or chunking never advances"
        );

        // The operator can turn it on, allowlist a root, and retune the bounds —
        // all known keys, all round-tripping.
        let raw = r#"
            [docsearch]
            enabled = true
            roots = ["/Users/me/Documents", "/Users/me/notes"]
            max_files = 100
            max_chunks = 1000
            max_file_bytes = 65536
            max_depth = 4
            chunk_chars = 800
            chunk_overlap = 100
            build_graph = true
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "docsearch keys must all be known: {issues:?}");
        assert!(cfg.docsearch.enabled);
        assert_eq!(cfg.docsearch.roots, vec!["/Users/me/Documents", "/Users/me/notes"]);
        assert_eq!(cfg.docsearch.max_files, 100);
        assert_eq!(cfg.docsearch.max_chunks, 1000);
        assert_eq!(cfg.docsearch.max_file_bytes, 65536);
        assert_eq!(cfg.docsearch.max_depth, 4);
        assert_eq!(cfg.docsearch.chunk_chars, 800);
        assert_eq!(cfg.docsearch.chunk_overlap, 100);
        // `build_graph` is a real parsed field, so it must round-trip AND be a known
        // key (no false "unknown config key docsearch.build_graph ignored").
        assert!(cfg.docsearch.build_graph);

        // A typo'd docsearch key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[docsearch]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("docsearch.enabledd")),
            "typo'd docsearch key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (task #16): [code] ships enabled=false AND roots=[] —
    /// code intelligence (code_explain + code_propose_diff) is OFF AND has no
    /// reachable codebase by default, exactly like
    /// self_heal/forge/standing/mcp/optimize/voice_id/docsearch. Because it READS
    /// and PROPOSES EDITS to the user's code, the EXTRA guard is the same as
    /// docsearch: an empty `roots` allowlist means "no codebase is reachable" even
    /// if `enabled` were flipped. Every key parses without an unknown-key
    /// diagnostic, the bound is finite, and a typo is diagnosed.
    #[test]
    fn code_defaults_off_empty_roots_and_bounded() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.code.enabled,
            "code intelligence must ship OFF (it reads AND proposes edits to the user's code)"
        );
        assert!(
            cfg.code.roots.is_empty(),
            "no codebase is reachable by default (never an arbitrary path)"
        );
        assert!(cfg.code.max_diff_bytes > 0, "max_diff_bytes must be a real bound");

        // The operator can turn it on, allowlist a codebase root, and retune the
        // bound — all known keys, all round-tripping.
        let raw = r#"
            [code]
            enabled = true
            roots = ["/Users/me/proj", "/Users/me/other"]
            max_diff_bytes = 4096
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "code keys must all be known: {issues:?}");
        assert!(cfg.code.enabled);
        assert_eq!(cfg.code.roots, vec!["/Users/me/proj", "/Users/me/other"]);
        assert_eq!(cfg.code.max_diff_bytes, 4096);

        // A typo'd code key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[code]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("code.enabledd")),
            "typo'd code key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [shell] (the sandboxed shell / terminal #43, the
    /// HIGHEST-RISK capability) SHIPS OFF (enabled=false) — exactly like
    /// self_heal/forge/code/vision. With it off the shell intent is never
    /// classified and `shell_run` is inert. The operator turns it on deliberately
    /// (and even then every command parks for a spoken yes + clears the denylist +
    /// the master switch + voice-id + !lockdown). Every key parses without an
    /// unknown-key diagnostic, and a typo is diagnosed.
    #[test]
    fn shell_defaults_off_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.shell.enabled,
            "the sandboxed shell must ship OFF — it is the highest-risk capability (arbitrary execution)"
        );
        // It is OFF-by-default identically to the struct's Default.
        let defaults = Config::default();
        assert_eq!(cfg.shell.enabled, defaults.shell.enabled);

        // The operator can deliberately enable it — a known, round-tripping key.
        let (cfg, issues) = Config::parse("[shell]\nenabled = true\n");
        assert!(issues.is_empty(), "shell keys must all be known: {issues:?}");
        assert!(cfg.shell.enabled, "operator-enabled shell parses true");

        // A typo'd shell key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[shell]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("shell.enabledd")),
            "typo'd shell key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [ui_automation] (gated UI automation #44, the CAPSTONE —
    /// the SINGLE MOST DANGEROUS capability, physically actuating the macOS UI)
    /// SHIPS OFF (enabled=false) — exactly like shell/self_heal/forge/code/vision.
    /// With it off the actuate intent is never classified and `ui_actuate` is inert.
    /// The operator turns it on deliberately (and even then every actuation parks
    /// PER ACTION for a spoken yes + clears the master switch + voice-id + !lockdown,
    /// and the actuation itself is device-gated behind the Accessibility TCC consent).
    /// Every key parses without an unknown-key diagnostic, and a typo is diagnosed.
    #[test]
    fn ui_automation_defaults_off_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.ui_automation.enabled,
            "gated UI automation must ship OFF — it is the single most dangerous capability (actuating the UI)"
        );
        // It is OFF-by-default identically to the struct's Default.
        let defaults = Config::default();
        assert_eq!(cfg.ui_automation.enabled, defaults.ui_automation.enabled);
        assert!(!defaults.ui_automation.enabled, "the struct default is OFF");

        // The operator can deliberately enable it — a known, round-tripping key.
        let (cfg, issues) = Config::parse("[ui_automation]\nenabled = true\n");
        assert!(issues.is_empty(), "ui_automation keys must all be known: {issues:?}");
        assert!(cfg.ui_automation.enabled, "operator-enabled ui_automation parses true");

        // A typo'd ui_automation key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[ui_automation]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("ui_automation.enabledd")),
            "typo'd ui_automation key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [vision] (the on-device VLM describe path) SHIPS OFF
    /// (enabled=false) AND with an EMPTY model — exactly like
    /// self_heal/forge/standing/mcp/optimize/voice_id/docsearch. With it off, the
    /// "describe my screen / what am I looking at / describe this image" intent
    /// never calls the VLM and falls back honestly. The operator turns it on AND
    /// names a (downloaded) model deliberately. Every key parses without an
    /// unknown-key diagnostic, and a typo is diagnosed.
    #[test]
    fn vision_vlm_defaults_off_empty_model_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.vision.enabled,
            "the on-device VLM describe path MUST ship OFF (it is device-gated on a multi-GB model)"
        );
        assert!(
            cfg.vision.model.is_empty(),
            "no VLM is named by default — empty model means the op honestly reports unavailable"
        );

        // The operator can turn it on and name a model — both known keys, round-tripping.
        let raw = r#"
            [vision]
            enabled = true
            model = "mlx-community/Qwen2-VL-2B-Instruct-4bit"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "vision keys must all be known: {issues:?}");
        assert!(cfg.vision.enabled);
        assert_eq!(cfg.vision.model, "mlx-community/Qwen2-VL-2B-Instruct-4bit");

        // A typo'd vision key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[vision]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("vision.enabledd")),
            "typo'd vision key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (task #18): [image] (the on-device text->image
    /// generation path) SHIPS OFF (enabled=false) AND with an EMPTY model —
    /// exactly like [vision]/self_heal/forge/standing/mcp/optimize/voice_id/
    /// docsearch. With it off, the "generate/make/draw an image of X" intent never
    /// calls the op and surfaces an honest "not set up" line. The operator turns it
    /// on AND names a (downloaded) diffusion model deliberately. Every key parses
    /// without an unknown-key diagnostic, and a typo is diagnosed. HONESTY: image
    /// generation is LOCAL only (MLX diffusion; the prompt + pixels stay on-device,
    /// NO cloud image API) — the OFF default keeps the multi-GB model gated.
    #[test]
    fn image_gen_defaults_off_empty_model_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.image.enabled,
            "the on-device image-generation path MUST ship OFF (it is device-gated on a multi-GB diffusion model)"
        );
        assert!(
            cfg.image.model.is_empty(),
            "no image model is named by default — empty model means the op honestly reports unavailable"
        );

        // The operator can turn it on and name a model — both known keys, round-tripping.
        let raw = r#"
            [image]
            enabled = true
            model = "schnell"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "image keys must all be known: {issues:?}");
        assert!(cfg.image.enabled);
        assert_eq!(cfg.image.model, "schnell");

        // A typo'd image key is diagnosed, not silently swallowed.
        let (_cfg, issues) = Config::parse("[image]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("image.enabledd")),
            "typo'd image key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (task #15): [audio].sound_monitor — the OPT-IN ambient
    /// sound monitor — SHIPS OFF (false) AND is pinned, exactly like
    /// self_heal/forge/standing/mcp/optimize/voice_id/docsearch/vision. Continuous
    /// ambient listening is a privacy liability, so it never starts without this
    /// explicit switch; with it OFF the audio path is byte-for-byte today's (the
    /// one-shot "what was that sound" intent on an already-captured clip needs no
    /// switch). The operator turns it on deliberately. Every key parses without an
    /// unknown-key diagnostic, and a typo is diagnosed. The other audio knobs keep
    /// their defaults (the new field is additive — it must not perturb them).
    #[test]
    fn sound_monitor_ships_off_and_keys_are_known() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.audio.sound_monitor,
            "the opt-in ambient sound monitor MUST ship OFF (continuous listening is a privacy liability)"
        );
        // Additive: the rest of the audio contract is untouched by the new field.
        assert_eq!(cfg.audio.rms_threshold, 0.015);
        assert_eq!(cfg.audio.silence_ms, 350);
        assert_eq!(cfg.audio.min_speech_ms, 250);
        assert!(cfg.audio.barge_in);
        assert_eq!(cfg.audio.barge_in_rms, 0.06);
        assert_eq!(cfg.audio.barge_in_ms, 250);

        // The operator can opt in — a known key, round-tripping, leaving the rest.
        let (cfg, issues) = Config::parse("[audio]\nsound_monitor = true\n");
        assert!(issues.is_empty(), "sound_monitor must be a known key: {issues:?}");
        assert!(cfg.audio.sound_monitor, "the operator can deliberately opt in");
        assert_eq!(cfg.audio.rms_threshold, 0.015, "the other audio knobs keep their defaults");

        // A typo'd key is diagnosed, not silently swallowed (so a misspelled opt-in
        // never silently leaves the monitor off when the user thought they enabled it).
        let (cfg, issues) = Config::parse("[audio]\nsound_moniter = true\n");
        assert!(
            issues.iter().any(|i| i.contains("audio.sound_moniter")),
            "typo'd sound_monitor key must be reported: {issues:?}"
        );
        assert!(!cfg.audio.sound_monitor, "a typo'd opt-in never silently arms the monitor");
    }

    /// Contract lockstep: [voice] (the ElevenLabs cloud voice tier) SHIPS OFF
    /// (cloud_tier=false) — exactly like self_heal/forge/standing/mcp. With it off,
    /// TTS is the on-device Kokoro default. The default model is eleven_flash_v2_5
    /// and the per-agent voice map is empty (so every agent uses its Kokoro voice
    /// until mapped). Every key parses without an unknown-key diagnostic.
    #[test]
    fn voice_tier_ships_off_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.voice.cloud_tier,
            "the ElevenLabs cloud voice tier MUST ship OFF (Kokoro is the default)"
        );
        assert!(
            !cfg.voice.cloud_stt,
            "the ElevenLabs Scribe cloud-STT tier MUST ship OFF (on-device whisper is the default)"
        );
        assert!(
            !cfg.voice.diarize,
            "#31 multi-speaker diarization MUST ship OFF (single-stream transcript by default)"
        );
        assert_eq!(cfg.voice.model, "eleven_flash_v2_5", "default EL model");
        assert!(cfg.voice.voices.is_empty(), "no per-agent EL voice mapped by default");

        // All keys parse as known and round-trip (including the [voice.voices] map).
        let raw = r#"
            [voice]
            cloud_tier = true
            cloud_stt = true
            diarize = true
            model = "eleven_multilingual_v2"

            [voice.voices]
            jarvis = "EL_VOICE_JARVIS"
            friday = "EL_VOICE_FRIDAY"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "voice keys must be known: {issues:?}");
        assert!(cfg.voice.cloud_tier);
        assert!(cfg.voice.cloud_stt, "cloud_stt must round-trip as a known key");
        assert!(cfg.voice.diarize, "diarize must round-trip as a known key");
        assert_eq!(cfg.voice.model, "eleven_multilingual_v2");
        assert_eq!(cfg.voice.voices.get("jarvis").map(String::as_str), Some("EL_VOICE_JARVIS"));
        assert_eq!(cfg.voice.voices.get("friday").map(String::as_str), Some("EL_VOICE_FRIDAY"));

        // A typo'd voice key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[voice]\ncloud_tierr = true\n");
        assert!(
            issues.iter().any(|i| i.contains("voice.cloud_tierr")),
            "typo'd voice key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [wake] (#32 custom wake-word) SHIPS OFF (enabled=false) and
    /// the default phrase is "jarvis" — so even when turned on the default preserves
    /// today's activation behavior. Every key parses without an unknown-key diagnostic.
    #[test]
    fn wake_ships_off_default_phrase_jarvis_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.wake.enabled, "custom wake-word gating MUST ship OFF");
        assert_eq!(
            cfg.wake.phrase, "jarvis",
            "the default wake phrase preserves today's activation behavior"
        );

        // Both keys parse as known and round-trip.
        let raw = r#"
            [wake]
            enabled = true
            phrase = "computer"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "wake keys must be known: {issues:?}");
        assert!(cfg.wake.enabled);
        assert_eq!(cfg.wake.phrase, "computer");

        // A typo'd wake key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[wake]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("wake.enabledd")),
            "typo'd wake key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [interpret] (#30 continuous live interpretation) SHIPS OFF
    /// (live=false, speak=false) — the device-gated mic loop never feeds the interpret
    /// pipeline by default. The default target is "English" and the source auto-detects
    /// (empty). Every key parses without an unknown-key diagnostic.
    #[test]
    fn interpret_ships_off_and_keys_match_the_contract() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.interpret.live, "continuous live interpretation MUST ship OFF");
        assert!(!cfg.interpret.speak, "voicing the translation MUST ship OFF");
        assert_eq!(cfg.interpret.target_lang, "English", "default target language");
        assert_eq!(cfg.interpret.source_lang, "", "empty source => auto-detect");

        // All keys parse as known and round-trip.
        let raw = r#"
            [interpret]
            live = true
            speak = true
            source_lang = "Spanish"
            target_lang = "English"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "interpret keys must be known: {issues:?}");
        assert!(cfg.interpret.live);
        assert!(cfg.interpret.speak);
        assert_eq!(cfg.interpret.source_lang, "Spanish");
        assert_eq!(cfg.interpret.target_lang, "English");

        // A typo'd interpret key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[interpret]\nlivee = true\n");
        assert!(
            issues.iter().any(|i| i.contains("interpret.livee")),
            "typo'd interpret key must be reported: {issues:?}"
        );
    }

    /// [skills].enabled DEFAULTS ON — UNLIKE self_heal/forge/standing/mcp, the
    /// pure in-tree skill library is safe to offer by default. The key parses as a
    /// known key, can be turned off, and a typo is diagnosed.
    #[test]
    fn skills_enabled_defaults_on_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            cfg.skills.enabled,
            "the pure skill library ships ON (safe by default)"
        );

        // Explicitly turning it off is honored without a diagnostic.
        let (cfg, issues) = Config::parse("[skills]\nenabled = false\n");
        assert!(issues.is_empty(), "skills.enabled must be a known key: {issues:?}");
        assert!(!cfg.skills.enabled, "operator can turn the library off");

        // A typo'd skills key is reported, not silently swallowed.
        let (_cfg, issues) = Config::parse("[skills]\nenabledd = true\n");
        assert!(
            issues.iter().any(|i| i.contains("skills.enabledd")),
            "typo'd skills key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep: [integrations] ships allow_consequential=false — the
    /// consequential-action gate is OFF by default, exactly like self-heal —
    /// and the key parses without an unknown-key diagnostic.
    #[test]
    fn integrations_allow_consequential_defaults_off_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(
            !cfg.integrations.allow_consequential,
            "consequential actions must ship OFF"
        );

        let raw = r#"
            [integrations]
            allow_consequential = true
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "allow_consequential must be a known key: {issues:?}");
        assert!(cfg.integrations.allow_consequential);
    }

    /// Contract lockstep: [mcp] ships enabled=false — the MCP subsystem (external
    /// tool servers) is OFF by default, exactly like self_heal/forge/standing —
    /// with safe-but-finite bounds and NO servers configured. The keys parse
    /// without an unknown-key diagnostic.
    #[test]
    fn mcp_defaults_off_with_no_servers_and_finite_bounds() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.mcp.enabled, "MCP must ship OFF");
        assert!(cfg.mcp.servers.is_empty(), "MCP must ship with no servers");
        assert!(cfg.mcp.max_servers > 0 && cfg.mcp.max_servers < 1000, "finite server bound");
        assert!(cfg.mcp.max_tools_per_server > 0, "finite tool bound");
        assert!(cfg.mcp.call_timeout_ms > 0, "finite call timeout");
        assert!(cfg.mcp.max_output_bytes > 0, "finite output cap");
    }

    /// The [mcp] top-level keys and a full [[mcp.servers]] entry parse cleanly,
    /// classification + transport enums deserialize, and the per-entry
    /// `deny_unknown_fields` rejects a typo'd server key (it falls the SECTION
    /// back to defaults with an issue, never silently widening the surface).
    #[test]
    fn mcp_full_server_entry_parses_and_typos_are_caught() {
        let raw = r#"
            [mcp]
            enabled = true
            max_servers = 3
            call_timeout_ms = 5000

            [[mcp.servers]]
            name = "files"
            transport = "stdio"
            command = "/usr/bin/srv"
            args = ["--root", "/p"]
            uses_token = true
            agents = ["friday"]
            default_class = "consequential"
            read_only_tools = ["list", "read"]
            fs_read = ["/p"]
            net_hosts = []
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "clean [mcp] must parse with no issues: {issues:?}");
        assert!(cfg.mcp.enabled);
        assert_eq!(cfg.mcp.max_servers, 3);
        assert_eq!(cfg.mcp.servers.len(), 1);
        let s = &cfg.mcp.servers[0];
        assert_eq!(s.name, "files");
        assert!(s.uses_token, "uses_token must parse");
        assert_eq!(s.agents, vec!["friday".to_string()]);
        assert_eq!(s.read_only_tools.len(), 2);

        // A typo'd server key must be caught (deny_unknown_fields) — the section
        // falls back to defaults with a reported issue, never silently accepted.
        let raw_bad = r#"
            [mcp]
            enabled = true
            [[mcp.servers]]
            name = "files"
            commnd = "/usr/bin/srv"   # typo: not a known server field
        "#;
        let (cfg, issues) = Config::parse(raw_bad);
        assert!(
            !issues.is_empty(),
            "a typo'd server field must be reported, not silently accepted"
        );
        assert!(cfg.mcp.servers.is_empty(), "the bad section falls back to defaults");
    }

    /// A typo'd top-level [mcp] key is reported (unknown-key diagnostic), not
    /// silently swallowed — the operator must know their bound did not apply.
    #[test]
    fn mcp_typoed_top_level_key_is_reported() {
        let (_cfg, issues) = Config::parse("[mcp]\nmax_serverss = 2\n");
        assert!(
            issues.iter().any(|i| i.contains("mcp.max_serverss")),
            "typo'd [mcp] key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (#35): [webhooks] ships enabled=false (the inbound
    /// network surface is OFF), binds 127.0.0.1 loopback by default, has NO
    /// mappings (an unmapped event is rejected), and the secret is NOT in the TOML.
    #[test]
    fn webhooks_default_off_loopback_no_mappings() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.webhooks.enabled, "webhook receiver must ship OFF");
        assert_eq!(cfg.webhooks.bind, "127.0.0.1", "must default to loopback");
        assert!(cfg.webhooks.mappings.is_empty(), "no event->intent mappings by default");
        assert!(cfg.webhooks.max_body_bytes > 0, "finite body cap");
        assert!(cfg.webhooks.port > 0, "a listen port");
    }

    /// A full [webhooks] section + a [[webhooks.mappings]] entry parses cleanly;
    /// the per-entry `deny_unknown_fields` catches a typo'd mapping key (the
    /// section falls back, never silently widening the event allowlist).
    #[test]
    fn webhooks_full_section_parses_and_mapping_typos_are_caught() {
        let raw = r#"
            [webhooks]
            enabled = true
            bind = "127.0.0.1"
            port = 9100
            max_body_bytes = 4096

            [[webhooks.mappings]]
            event = "ci.failed"
            intent = "system.query"
        "#;
        let (cfg, issues) = Config::parse(raw);
        assert!(issues.is_empty(), "clean [webhooks] must parse: {issues:?}");
        assert!(cfg.webhooks.enabled);
        assert_eq!(cfg.webhooks.port, 9100);
        assert_eq!(cfg.webhooks.mappings.len(), 1);
        assert_eq!(cfg.webhooks.mappings[0].event, "ci.failed");
        assert_eq!(cfg.webhooks.mappings[0].intent, "system.query");

        let raw_bad = r#"
            [webhooks]
            enabled = true
            [[webhooks.mappings]]
            event = "ci.failed"
            intnt = "system.query"   # typo: not a known mapping field
        "#;
        let (cfg, issues) = Config::parse(raw_bad);
        assert!(!issues.is_empty(), "a typo'd mapping field must be reported");
        assert!(cfg.webhooks.mappings.is_empty(), "the bad section falls back to defaults");
    }

    /// A typo'd top-level [webhooks] key is reported, not silently swallowed.
    #[test]
    fn webhooks_typoed_top_level_key_is_reported() {
        let (_cfg, issues) = Config::parse("[webhooks]\nbnd = \"127.0.0.1\"\n");
        assert!(
            issues.iter().any(|i| i.contains("webhooks.bnd")),
            "typo'd [webhooks] key must be reported: {issues:?}"
        );
    }

    /// Contract lockstep (#36): [plugin_sdk] ships enabled=false — the live
    /// register-on-launch handshake is OFF by default (the pure validator is
    /// always available regardless). The key parses without a diagnostic.
    #[test]
    fn plugin_sdk_defaults_off_and_is_a_known_key() {
        let (cfg, issues) = Config::parse("");
        assert!(issues.is_empty());
        assert!(!cfg.plugin_sdk.enabled, "the plugin-SDK launch handshake must ship OFF");

        let (cfg, issues) = Config::parse("[plugin_sdk]\nenabled = true\n");
        assert!(issues.is_empty(), "plugin_sdk.enabled must be a known key: {issues:?}");
        assert!(cfg.plugin_sdk.enabled);
    }
}
