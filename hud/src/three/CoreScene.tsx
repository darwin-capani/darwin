/**
 * The reactive center core: glowing wireframe icosahedron + inner energy
 * sphere + orbiting particle field, with bloom. State signatures come from
 * core/visuals.ts (pure, tested); this file only draws.
 *
 * Anti-flash invariants (user directive #5):
 *  - Live rms is read from core/audioStore.ts inside useFrame — audio.level
 *    frames never re-render this component (props carry only coreState).
 *  - The EffectComposer is permanently mounted; tier changes LERP the bloom
 *    intensity to its tier target instead of mounting/unmounting the
 *    composer (which was a single-frame luminance cut + shader hitch).
 *  - The particle buffer is allocated ONCE at the tier-0 count; lower tiers
 *    shed load via geometry.setDrawRange with a damped count, so particle
 *    positions never regenerate/teleport.
 */
import { Canvas, useFrame } from "@react-three/fiber";
import { memo, useMemo, useRef } from "react";
import {
  BlendFunction,
  BloomEffect,
  ChromaticAberrationEffect,
  NoiseEffect,
} from "postprocessing";
import { EffectComposer } from "@react-three/postprocessing";
import * as THREE from "three";
import { audioStore } from "../core/audioStore";
import { PERF_TIERS, PerfGovernor } from "../core/perf";
import type { CoreState } from "../core/state";
import {
  CoreVisualTarget,
  ampFollow,
  coreVisualTarget,
  damp,
  dampHue,
  syntheticSpeechEnvelope,
} from "../core/visuals";

export interface CoreSceneProps {
  coreState: CoreState;
  /** Active-agent identity hue (0..360) the core damps toward, or null when no
   *  agent is handling the request (idle -> default cyan). Changes only on
   *  agent.active / turn-end — never per audio frame — so it is safe as a
   *  prop on this memo'd component. */
  agentHue: number | null;
}

const BLOOM_INTENSITY = 0.72;
// Whole-assembly proportion: a compact centerpiece, not a viewport-filling
// planet. Applied in the per-frame scale write (the JSX scale prop would be
// overwritten by the pulse). Trimmed 0.58 -> 0.42 -> 0.36 so the orb's bloom is
// cleanly framed by the tactical ring with the corner readouts outside it (no
// overlap), and stays a small focused centerpiece.
const CORE_BASE_SCALE = 0.36;

/* ---------------------------------------------------------------- shaders */

const PARTICLE_VERT = /* glsl */ `
  attribute float aSeed;
  uniform float uTime;
  uniform float uUpward;
  uniform float uConverge;
  uniform float uSize;
  uniform float uFlow;   // smoothed audio amplitude -> particle current energy
  varying float vFade;
  void main() {
    vec3 p = position;
    float t = uTime * 0.12 + aSeed * 6.28318;
    // Calm curl drift, energized by audio (uFlow) — the particle shell is now
    // the audio-reactive element (the orb itself holds still).
    float drift = 1.0 + uFlow * 1.5;
    p.x += sin(t * 1.1 + aSeed * 13.0) * 0.14 * drift;
    p.y += sin(t * 1.7 + aSeed * 29.0) * 0.10 * drift;
    p.z += cos(t * 0.9 + aSeed * 17.0) * 0.14 * drift;
    // Unified swirl around Y (one shared rotation = "unified direction") plus a
    // per-seed wobble (= organic/"random"); both speed up with audio. Rotation
    // preserves radius, so the shell can never spray outward / explode.
    float ang = uTime * (0.12 + uFlow * 0.55)
              + uFlow * 0.28 * sin(uTime * 1.3 + aSeed * 12.0);
    float ca = cos(ang), sa = sin(ang);
    p.xz = mat2(ca, -sa, sa, ca) * p.xz;
    // Gentle vertical current riding the audio, per-seed phased.
    p.y += uFlow * 0.13 * sin(uTime * 0.6 + aSeed * 8.0);
    // thinking: converge toward the core
    p *= 1.0 - uConverge * 0.28;
    // cloud routing: stream upward and recycle
    p.y += uUpward * (mod(aSeed * 19.0 + uTime * 0.9, 6.0) - 2.0);
    vec4 mv = modelViewMatrix * vec4(p, 1.0);
    gl_Position = projectionMatrix * mv;
    float size = uSize * (0.6 + fract(aSeed * 7.31) * 1.4);
    // 60/-z keeps sprites at dot scale (~5-20 device px). The old 220/-z
    // factor made 50-150px additive blobs that saturated the whole center
    // to white — every intensity change then read as a violent flash.
    gl_PointSize = size * (60.0 / max(0.001, -mv.z));
    // Fade band tracks the tighter shell (radius ~1.15..1.75, plus audio drift):
    // particles dim toward the new outer edge instead of the old 2.1..3.4 band
    // (which sat entirely outside the smaller shell, so nothing faded).
    vFade = 1.0 - smoothstep(1.6, 2.6, length(p));
  }
`;

const PARTICLE_FRAG = /* glsl */ `
  uniform vec3 uColor;
  uniform float uOpacity;
  varying float vFade;
  void main() {
    vec2 c = gl_PointCoord - 0.5;
    float d = length(c);
    float a = smoothstep(0.5, 0.0, d) * vFade * uOpacity;
    if (a < 0.004) discard;
    gl_FragColor = vec4(uColor, a);
  }
`;

const SPHERE_VERT = /* glsl */ `
  varying vec3 vNormal;
  varying vec3 vView;
  void main() {
    vNormal = normalize(normalMatrix * normal);
    vec4 mv = modelViewMatrix * vec4(position, 1.0);
    vView = normalize(-mv.xyz);
    gl_Position = projectionMatrix * mv;
  }
`;

const SPHERE_FRAG = /* glsl */ `
  uniform vec3 uColor;
  uniform float uIntensity;
  varying vec3 vNormal;
  varying vec3 vView;
  void main() {
    float fresnel = pow(1.0 - abs(dot(vNormal, vView)), 2.2);
    float coreGlow = 0.18 + fresnel * 1.4;
    gl_FragColor = vec4(uColor * uIntensity * coreGlow, coreGlow * 0.9);
  }
`;

/* ------------------------------------------------------------- assembly */

function makeParticles(count: number): THREE.BufferGeometry {
  const geo = new THREE.BufferGeometry();
  const pos = new Float32Array(count * 3);
  const seed = new Float32Array(count);
  for (let i = 0; i < count; i++) {
    // shell distribution around the core — tightened from 1.5+rand*0.9 to a
    // more compact 1.15+rand*0.6 halo (user directive: smaller particles),
    // paired with the smaller CORE_BASE_SCALE and the matching vFade band.
    const r = 1.15 + Math.random() * 0.6;
    const theta = Math.random() * Math.PI * 2;
    const z = Math.random() * 2 - 1;
    const s = Math.sqrt(1 - z * z);
    pos[i * 3] = r * s * Math.cos(theta);
    pos[i * 3 + 1] = r * z;
    pos[i * 3 + 2] = r * s * Math.sin(theta);
    seed[i] = Math.random();
  }
  geo.setAttribute("position", new THREE.BufferAttribute(pos, 3));
  geo.setAttribute("aSeed", new THREE.BufferAttribute(seed, 1));
  return geo;
}

/* --------------------------------------------------------- reticle layers */

// Reduced-motion: a single module-scope MediaQueryList (zero per-frame
// allocation) so the loop can freeze ALL continuous motion when the OS asks
// for it. matchMedia().matches is a cheap property read; the list updates
// itself when the system preference changes, so no listener is needed.
const REDUCED_MOTION_MQ =
  typeof window !== "undefined" && typeof window.matchMedia === "function"
    ? window.matchMedia("(prefers-reduced-motion: reduce)")
    : null;
const reducedMotion = () => REDUCED_MOTION_MQ?.matches === true;

// Audio-reactive SPECTRUM RING: `count` radial ticks just outside the particle
// shell. Two vertices per tick (inner fixed, outer pushed out per frame by the
// synthesized spectrum) drawn as LineSegments — one draw call, positions
// updated in place (no re-alloc, no teleport: same anti-flash discipline as the
// particle buffer). The base ring sits at SPECTRUM_R; bars grow outward.
const SPECTRUM_BARS = 72;
const SPECTRUM_R = 1.74;
// Shorter bars (was 0.55) so the spectrum reads as a tight energy ring hugging
// the orb rather than long spikes that collide with the overlay gauges/ticks.
const SPECTRUM_MAX = 0.32;
function makeSpectrum(count: number): THREE.BufferGeometry {
  const geo = new THREE.BufferGeometry();
  const pos = new Float32Array(count * 2 * 3);
  for (let i = 0; i < count; i++) {
    const a = (i / count) * Math.PI * 2;
    const cx = Math.cos(a) * SPECTRUM_R;
    const cy = Math.sin(a) * SPECTRUM_R;
    pos[i * 6] = cx;
    pos[i * 6 + 1] = cy;
    pos[i * 6 + 3] = cx; // outer starts coincident; useFrame pushes it out
    pos[i * 6 + 4] = cy;
  }
  geo.setAttribute("position", new THREE.BufferAttribute(pos, 3));
  return geo;
}

interface LiveVisual extends CoreVisualTarget {
  scale: number;
  bloom: number;
  particleCount: number;
  /** Smoothed audio amplitude (ampFollow output) — drives the gentle swell. */
  ampEnv: number;
  /** Motion clock: advances by dt ONLY when motion is allowed, so a
   *  reduced-motion user gets a frozen frame instead of the wall clock. */
  simTime: number;
  /** Damped pointer-parallax tilt (radians) added to the core's rest tilt. */
  tiltX: number;
  tiltZ: number;
}

// Grain strength when motion is allowed; silenced under reduced-motion since
// procedural noise animates per frame.
const NOISE_OPACITY = 0.05;

function CoreAssembly({
  coreState,
  agentHue,
  governor,
  bloomEffect,
  noiseEffect,
}: {
  coreState: CoreState;
  agentHue: number | null;
  governor: PerfGovernor;
  bloomEffect: BloomEffect;
  noiseEffect: NoiseEffect;
}) {
  const group = useRef<THREE.Group>(null);
  const ring = useRef<THREE.Mesh>(null);
  // Mk II audio spectrum ring. (The reticle/range framing now lives in the
  // sharper SVG overlay — CoreHud — so the redundant canvas rings were removed
  // to declutter the concentric stack.)
  const spectrum = useRef<THREE.LineSegments>(null);
  // Reactor depth layers: a counter-rotating octahedral cage + 2 orbital rings.
  const shell2 = useRef<THREE.LineSegments>(null);
  const orbit1 = useRef<THREE.Group>(null);
  const orbit2 = useRef<THREE.Group>(null);
  // Previous discrete state, to fire the one-shot "iris" flourish on a change.
  const prevState = useRef<CoreState>(coreState);
  const iris = useRef(0);
  // Pixel ratio is set ONCE via the Canvas dpr prop (FIXED_DPR) — do not call
  // gl.setPixelRatio here; a second writer desyncs the composer's buffers
  // from the canvas size and the mismatch reads as flicker.

  const wireGeo = useMemo(
    () => new THREE.WireframeGeometry(new THREE.IcosahedronGeometry(1.18, 2)),
    [],
  );
  const wireMat = useMemo(
    () =>
      new THREE.LineBasicMaterial({
        color: new THREE.Color("#36C6E3"),
        transparent: true,
        opacity: 0.55,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      }),
    [],
  );
  const sphereMat = useMemo(
    () =>
      new THREE.ShaderMaterial({
        vertexShader: SPHERE_VERT,
        fragmentShader: SPHERE_FRAG,
        uniforms: {
          uColor: { value: new THREE.Color("#36C6E3") },
          uIntensity: { value: 0.5 },
        },
        transparent: true,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      }),
    [],
  );
  // Allocated once at the FULL tier-0 count; tiers shed via setDrawRange.
  const particleGeo = useMemo(() => makeParticles(PERF_TIERS[0].particles), []);
  const particleMat = useMemo(
    () =>
      new THREE.ShaderMaterial({
        vertexShader: PARTICLE_VERT,
        fragmentShader: PARTICLE_FRAG,
        uniforms: {
          uTime: { value: 0 },
          uUpward: { value: 0 },
          uConverge: { value: 0 },
          uFlow: { value: 0 },
          uSize: { value: 0.62 },
          uColor: { value: new THREE.Color("#7DF3FF") },
          uOpacity: { value: 0.62 },
        },
        transparent: true,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      }),
    [],
  );

  // Spectrum ring: positions updated in place every frame (no re-alloc).
  const spectrumGeo = useMemo(() => makeSpectrum(SPECTRUM_BARS), []);
  const spectrumMat = useMemo(
    () =>
      new THREE.LineBasicMaterial({
        color: new THREE.Color("#7DF3FF"),
        transparent: true,
        opacity: 0.6,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      }),
    [],
  );
  // Second wireframe shell (octahedral cage) + orbital-ring material + satellite
  // node material — the reactor/gyroscope depth layers around the orb.
  const shell2Geo = useMemo(
    () => new THREE.WireframeGeometry(new THREE.OctahedronGeometry(1.5, 0)),
    [],
  );
  const shell2Mat = useMemo(
    () =>
      new THREE.LineBasicMaterial({
        color: new THREE.Color("#36C6E3"),
        transparent: true,
        opacity: 0.2,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      }),
    [],
  );
  const orbitMat = useMemo(
    () =>
      new THREE.MeshBasicMaterial({
        color: new THREE.Color("#7DF3FF"),
        transparent: true,
        opacity: 0.3,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      }),
    [],
  );
  const satMat = useMemo(
    () =>
      new THREE.MeshBasicMaterial({
        color: new THREE.Color("#CFFAFF"),
        transparent: true,
        opacity: 0.95,
        blending: THREE.AdditiveBlending,
        depthWrite: false,
      }),
    [],
  );

  // Pooled per-frame state — zero allocations in the loop.
  const live = useRef<LiveVisual>({
    hue: 190,
    intensity: 0.2,
    spin: 0.02,
    pulseHz: 0.1,
    pulseDepth: 0.05,
    upward: 0,
    converge: 0,
    scale: 1,
    bloom: BLOOM_INTENSITY,
    particleCount: PERF_TIERS[0].particles,
    ampEnv: 0,
    simTime: 0,
    tiltX: 0,
    tiltZ: 0,
  });
  const tmpColor = useRef(new THREE.Color());
  const stateRef = useRef(coreState);
  stateRef.current = coreState;
  // Active-agent hue read through a ref inside useFrame so the override is
  // picked up without any extra reconciliation work (same pattern as
  // stateRef). dampHue still sweeps the core toward it — never a hard cut.
  const agentHueRef = useRef(agentHue);
  agentHueRef.current = agentHue;

  useFrame((state, delta) => {
    const dt = Math.min(delta, 0.1);
    const rm = reducedMotion();
    const coreState = stateRef.current;
    const rms = audioStore.lastRms; // refs, not React — no re-render path
    const v = live.current;
    // Motion clock: frozen under reduced-motion so every t-driven oscillation
    // (drift, breath, shimmer, parallax) settles to a still frame. Color and
    // intensity still damp — those are state, not motion.
    v.simTime += rm ? 0 : dt;
    const t = v.simTime;
    const target = coreVisualTarget(coreState, rms, agentHueRef.current);

    // One-shot IRIS flourish: a discrete state change snaps the targeting
    // reticle (brief opacity+scale kick that decays away). Never fires on the
    // 15Hz audio frames — only on the discrete coreState transition.
    if (coreState !== prevState.current) {
      iris.current = rm ? 0 : 1;
      prevState.current = coreState;
    }
    iris.current = damp(iris.current, 0, 4, dt);

    // Smooth crossfades — lerp hue/intensity, never hard cuts.
    if (import.meta.env.DEV) {
      (window as unknown as Record<string, unknown>).__live = {
        scale: v.scale, intensity: v.intensity, rms, ampEnv: v.ampEnv,
        groupScale: group.current ? group.current.scale.x : null,
        coreState,
      };
    }
    v.hue = dampHue(v.hue, target.hue, 5, dt);
    v.intensity = damp(v.intensity, target.intensity, 5, dt);
    v.spin = damp(v.spin, target.spin, 4, dt);
    v.pulseHz = damp(v.pulseHz, target.pulseHz, 4, dt);
    v.pulseDepth = damp(v.pulseDepth, target.pulseDepth, 4, dt);
    v.upward = damp(v.upward, target.upward, 3, dt);
    v.converge = damp(v.converge, target.converge, 3, dt);

    // Amplitude source: live rms when audible (gain 5, was 12), synthetic
    // envelope while speaking. Routed through an ASYMMETRIC envelope follower
    // (ampFollow: gentle attack, slower release) so the core tracks the
    // voice's loudness contour, never the per-frame spikes that caused the
    // "epileptic" strobing.
    let ampTarget = Math.min(1, rms * 5);
    if (coreState === "speaking") {
      ampTarget = Math.max(ampTarget, syntheticSpeechEnvelope(t));
    }
    v.ampEnv = ampFollow(v.ampEnv, ampTarget, dt);
    const amp = v.ampEnv;

    // The ORB holds still during audio (user directive): its size is a tiny,
    // slow breath INDEPENDENT of amplitude — the audio energy goes to the
    // particle shell (uFlow) instead, not the orb's scale.
    const pulse = 1 + Math.sin(t * Math.PI * 2 * v.pulseHz) * v.pulseDepth * 0.5;
    v.scale = Math.min(1.05, Math.max(0.96, damp(v.scale, pulse, 5, dt)));

    // Pointer parallax: the whole assembly tilts a few degrees toward the
    // cursor for depth, damped so it glides (never snaps). Zeroed under
    // reduced-motion. state.pointer is R3F's normalized -1..1 canvas cursor.
    const targetTiltX = rm ? 0 : -state.pointer.y * 0.2;
    const targetTiltZ = rm ? 0 : state.pointer.x * 0.12;
    v.tiltX = damp(v.tiltX, targetTiltX, 4, dt);
    v.tiltZ = damp(v.tiltZ, targetTiltZ, 4, dt);

    if (group.current) {
      // Spin gated by reduced-motion (was ungated) so the orb truly freezes.
      group.current.rotation.y += v.spin * (rm ? 0 : dt);
      group.current.rotation.x = Math.sin(t * 0.07) * 0.18 + v.tiltX;
      group.current.rotation.z = v.tiltZ;
      group.current.scale.setScalar(v.scale * CORE_BASE_SCALE);
    }
    if (ring.current) {
      // Ring is part of the orb — steady size (no audio-driven movement); only
      // a faint brightness glow tracks the smoothed amplitude.
      ring.current.scale.setScalar(1.42);
      (ring.current.material as THREE.MeshBasicMaterial).opacity =
        0.05 + amp * 0.12 * Math.min(1, v.intensity);
    }

    // --- Reactor depth layers: the octahedral cage counter-rotates (net slow,
    // opposite the orb) and the two inclined orbital rings spin at their own
    // rates, carrying their satellite nodes around. All gated by reduced-motion;
    // hue/brightness track the core like every other element. ---
    const dtm = rm ? 0 : dt;
    const intB = Math.min(1, v.intensity);
    if (shell2.current) {
      shell2.current.rotation.y -= v.spin * dtm * 1.7;
      shell2.current.rotation.x += dtm * 0.06;
      tmpColor.current.setHSL(v.hue / 360, 0.75, 0.58);
      shell2Mat.color.copy(tmpColor.current);
      shell2Mat.opacity = 0.12 + intB * 0.22;
    }
    if (orbit1.current) orbit1.current.rotation.y += dtm * 0.6;
    if (orbit2.current) orbit2.current.rotation.y -= dtm * 0.46;
    tmpColor.current.setHSL(v.hue / 360, 0.95, 0.72);
    orbitMat.color.copy(tmpColor.current);
    orbitMat.opacity = 0.18 + intB * 0.28;
    tmpColor.current.setHSL(v.hue / 360, 0.55, 0.92);
    satMat.color.copy(tmpColor.current);

    tmpColor.current.setHSL(v.hue / 360, 0.85, 0.62);
    wireMat.color.copy(tmpColor.current);
    wireMat.opacity = Math.min(1, 0.46 + v.intensity * 0.4);
    (sphereMat.uniforms.uColor.value as THREE.Color).copy(tmpColor.current);
    sphereMat.uniforms.uIntensity.value = v.intensity;
    (particleMat.uniforms.uColor.value as THREE.Color).setHSL(v.hue / 360, 0.9, 0.72);
    particleMat.uniforms.uTime.value = t;
    particleMat.uniforms.uUpward.value = v.upward;
    particleMat.uniforms.uConverge.value = v.converge;
    // Audio drives the particle current (capped), not the orb's size.
    particleMat.uniforms.uFlow.value = Math.min(0.8, amp);
    particleMat.uniforms.uOpacity.value = 0.34 + Math.min(1, v.intensity) * 0.32;

    const intC = Math.min(1, v.intensity);

    // --- Spectrum ring: outer vertex of each radial tick is pushed out by an
    // idle shimmer plus an audio-driven peak (a traveling wave so it scintillates
    // like an equalizer). Positions rewritten in place every frame — same
    // no-realloc discipline as the particle buffer. ---
    if (spectrum.current) {
      spectrum.current.rotation.z += (rm ? 0 : dt) * 0.02;
      const sp = spectrumGeo.attributes.position.array as Float32Array;
      // Audio term gated by reduced-motion: with flow=0 the bars collapse to the
      // (frozen-t) shimmer baseline and the ring holds a still frame, so a
      // reduced-motion user never sees the equalizer pulse with their voice.
      const flow = rm ? 0 : Math.min(1, amp * 1.25);
      const gain = Math.min(1.25, 0.55 + v.intensity);
      for (let i = 0; i < SPECTRUM_BARS; i++) {
        const a = (i / SPECTRUM_BARS) * Math.PI * 2;
        const shimmer = 0.12 + 0.07 * Math.sin(i * 0.7 + t * 1.3);
        const peak = flow * (0.35 + 0.65 * Math.abs(Math.sin(i * 2.1 + t * 4.0)));
        const r = SPECTRUM_R + (shimmer + peak) * SPECTRUM_MAX * gain;
        sp[i * 6 + 3] = Math.cos(a) * r;
        sp[i * 6 + 4] = Math.sin(a) * r;
      }
      spectrumGeo.attributes.position.needsUpdate = true;
      tmpColor.current.setHSL(v.hue / 360, 0.95, 0.74);
      spectrumMat.color.copy(tmpColor.current);
      // iris one-shot now flares the spectrum on a state change (the reticle it
      // used to drive is gone).
      spectrumMat.opacity = 0.34 + intC * 0.36 + iris.current * 0.3;
    }

    // Adaptive degradation — crossfaded, never a hard cut:
    // bloom intensity lerps toward the tier target (composer stays mounted),
    // particle count damps toward the tier budget via setDrawRange.
    const tier = PERF_TIERS[governor.sample(dt * 1000)];
    v.bloom = damp(v.bloom, tier.bloom ? BLOOM_INTENSITY : 0, 2, dt);
    bloomEffect.intensity = v.bloom;
    // Film grain is animated noise — mute it entirely under reduced-motion, and
    // shed it with bloom on the low perf tier so a struggling GPU drops it too.
    noiseEffect.blendMode.opacity.value = rm || !tier.bloom ? 0 : NOISE_OPACITY;
    v.particleCount = damp(v.particleCount, tier.particles, 2, dt);
    particleGeo.setDrawRange(0, Math.round(v.particleCount));
  });

  return (
    // Lift the whole core a touch above the viewport center so it sits in the
    // OPEN upper zone (clear of the bottom intel panel) — the overlay/aura are
    // shifted up to match in CSS.
    <group position={[0, 0.22, 0]}>
      {/* The orb: spins on Y, tilts with breath + pointer parallax, breathes. */}
      <group ref={group}>
        <lineSegments geometry={wireGeo} material={wireMat} />
        {/* counter-rotating octahedral cage */}
        <lineSegments ref={shell2} geometry={shell2Geo} material={shell2Mat} />
        <mesh material={sphereMat}>
          <sphereGeometry args={[0.82, 48, 48]} />
        </mesh>
        <mesh ref={ring} rotation={[Math.PI / 2, 0, 0]}>
          <torusGeometry args={[1, 0.006, 8, 96]} />
          <meshBasicMaterial
            color="#7DF3FF"
            transparent
            opacity={0.1}
            blending={THREE.AdditiveBlending}
            depthWrite={false}
          />
        </mesh>
        {/* two inclined orbital rings, each carrying a satellite node */}
        <group ref={orbit1} rotation={[1.15, 0, 0.4]}>
          <mesh material={orbitMat}>
            <torusGeometry args={[1.5, 0.004, 6, 96]} />
          </mesh>
          <mesh material={satMat} position={[1.5, 0, 0]}>
            <sphereGeometry args={[0.032, 10, 10]} />
          </mesh>
        </group>
        <group ref={orbit2} rotation={[-0.7, 0.6, -0.35]}>
          <mesh material={orbitMat}>
            <torusGeometry args={[1.74, 0.004, 6, 96]} />
          </mesh>
          <mesh material={satMat} position={[1.74, 0, 0]}>
            <sphereGeometry args={[0.026, 10, 10]} />
          </mesh>
        </group>
        <points geometry={particleGeo} material={particleMat} />
      </group>
      {/* Spectrum: steady, camera-facing energy ring (in-plane spin only) that
          frames the breathing orb. Scaled to match the orb but never pulsed, so
          the ring holds while the core breathes. */}
      <group scale={CORE_BASE_SCALE}>
        <lineSegments ref={spectrum} geometry={spectrumGeo} material={spectrumMat} />
      </group>
    </group>
  );
}

// Stable references: fresh object/array literals on these Canvas props make
// R3F reconfigure the renderer every parent render. Hoisted to module scope
// so they never change identity. dpr is PINNED (not a [min,max] range) so
// R3F's performance regressor cannot step resolution up/down — that stepping
// reads as a whole-canvas flicker.
// alpha:false — an OPAQUE canvas. Transparent WebGL composited over DOM is
// WKWebView's (Tauri's webview) primary flicker trigger when a postprocessing
// composer is present; the scene paints its own navy background instead and
// the dotted-grid backdrop sits ABOVE the canvas (see .hud-backdrop z-index).
const GL_PROPS = {
  antialias: false,
  powerPreference: "high-performance" as const,
  alpha: false,
  stencil: false,
};
const CAMERA_PROPS = { position: [0, 0, 4.6] as [number, number, number], fov: 42 };
const CANVAS_STYLE = { position: "absolute" as const, inset: 0 };
// Single source of truth for pixel ratio, capped at 1.5 for integrated GPUs.
// This MUST be the only place dpr is set: a second gl.setPixelRatio elsewhere
// fights R3F's sizing and the buffer churn reads as whole-canvas flicker.
const FIXED_DPR = Math.min(
  typeof window === "undefined" ? 1.5 : window.devicePixelRatio || 1.5,
  1.5,
);

function CoreScene({ coreState, agentHue }: CoreSceneProps) {
  const governor = useRef(new PerfGovernor()).current;
  // One BloomEffect for the life of the scene; tier changes only write its
  // `intensity` (a uniform) — zero-intensity bloom is cheap relative to the
  // single-frame luminance cut of unmounting the composer.
  const bloomEffect = useMemo(() => {
    const effect = new BloomEffect({
      blendFunction: BlendFunction.ADD,
      intensity: BLOOM_INTENSITY,
      luminanceThreshold: 0.3,
      mipmapBlur: true,
      radius: 0.7,
    });
    if (import.meta.env.DEV) {
      (window as unknown as Record<string, unknown>).__bloom = effect;
    }
    return effect;
  }, []);

  // Holographic fringe: a SMALL, radially-modulated chromatic aberration so the
  // bloomed core edges split into faint cyan/magenta — the "projected hologram"
  // tell. Radial modulation keeps the center crisp and only fringes the edges.
  // Static (no time term) so it adds no motion and no flicker surface.
  const chromaticAberration = useMemo(
    () =>
      new ChromaticAberrationEffect({
        offset: new THREE.Vector2(0.0007, 0.0007),
        radialModulation: true,
        modulationOffset: 0.3,
      }),
    [],
  );
  // Fine film grain over the whole frame — breaks up the flat void into a lit
  // surface. Opacity is driven per-frame in CoreAssembly (0 under reduced-motion
  // or the low perf tier).
  const noiseEffect = useMemo(() => {
    const effect = new NoiseEffect({ blendFunction: BlendFunction.SCREEN, premultiply: true });
    effect.blendMode.opacity.value = NOISE_OPACITY;
    return effect;
  }, []);

  return (
    <Canvas gl={GL_PROPS} camera={CAMERA_PROPS} dpr={FIXED_DPR} style={CANVAS_STYLE}>
      {/* Opaque scene background (pairs with alpha:false above). */}
      <color attach="background" args={["#05080c"]} />
      <CoreAssembly
        coreState={coreState}
        agentHue={agentHue}
        governor={governor}
        bloomEffect={bloomEffect}
        noiseEffect={noiseEffect}
      />
      {/* multisampling=0: MSAA framebuffer blits are a WKWebView flicker
          class; bloom's mipmap blur supplies the smoothing instead.
          ORDER MATTERS: chromatic aberration is a CONVOLUTION effect, which can
          never share an EffectPass with its neighbours, so it goes LAST — that
          lets bloom + grain (both non-convolution) merge into ONE pass, leaving
          exactly TWO passes total ([bloom+grain], then [chromatic aberration]).
          Putting it in the middle would split this into THREE passes and add a
          render-target ping-pong per frame (the WKWebView composer cost we
          minimize). Both added passes are STATIC (CA) or opacity-gated (grain),
          so the opaque-canvas anti-flash contract still holds. */}
      <EffectComposer multisampling={0}>
        <primitive object={bloomEffect} />
        <primitive object={noiseEffect} />
        <primitive object={chromaticAberration} />
      </EffectComposer>
    </Canvas>
  );
}

// Only re-render (and thus reconcile the WebGL subtree) when the discrete
// coreState changes — never on the App's 250ms housekeeping tick or the
// per-envelope telemetry dispatches that left the Canvas churning ~4+ Hz.
export default memo(CoreScene);
