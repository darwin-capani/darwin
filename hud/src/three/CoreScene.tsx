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
import { BlendFunction, BloomEffect } from "postprocessing";
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
// overwritten by the pulse). Trimmed 0.58 -> 0.42 so the orb reads as a small
// focused centerpiece (user directive: make the orb AND its particles smaller).
const CORE_BASE_SCALE = 0.42;

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

interface LiveVisual extends CoreVisualTarget {
  scale: number;
  bloom: number;
  particleCount: number;
  /** Smoothed audio amplitude (ampFollow output) — drives the gentle swell. */
  ampEnv: number;
}

function CoreAssembly({
  coreState,
  agentHue,
  governor,
  bloomEffect,
}: {
  coreState: CoreState;
  agentHue: number | null;
  governor: PerfGovernor;
  bloomEffect: BloomEffect;
}) {
  const group = useRef<THREE.Group>(null);
  const ring = useRef<THREE.Mesh>(null);
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
    const coreState = stateRef.current;
    const rms = audioStore.lastRms; // refs, not React — no re-render path
    const t = state.clock.elapsedTime;
    const v = live.current;
    const target = coreVisualTarget(coreState, rms, agentHueRef.current);

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

    if (group.current) {
      group.current.rotation.y += v.spin * dt;
      group.current.rotation.x = Math.sin(t * 0.07) * 0.18;
      group.current.scale.setScalar(v.scale * CORE_BASE_SCALE);
    }
    if (ring.current) {
      // Ring is part of the orb — steady size (no audio-driven movement); only
      // a faint brightness glow tracks the smoothed amplitude.
      ring.current.scale.setScalar(1.42);
      (ring.current.material as THREE.MeshBasicMaterial).opacity =
        0.05 + amp * 0.12 * Math.min(1, v.intensity);
    }

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

    // Adaptive degradation — crossfaded, never a hard cut:
    // bloom intensity lerps toward the tier target (composer stays mounted),
    // particle count damps toward the tier budget via setDrawRange.
    const tier = PERF_TIERS[governor.sample(dt * 1000)];
    v.bloom = damp(v.bloom, tier.bloom ? BLOOM_INTENSITY : 0, 2, dt);
    bloomEffect.intensity = v.bloom;
    v.particleCount = damp(v.particleCount, tier.particles, 2, dt);
    particleGeo.setDrawRange(0, Math.round(v.particleCount));
  });

  return (
    <group ref={group}>
      <lineSegments geometry={wireGeo} material={wireMat} />
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
      <points geometry={particleGeo} material={particleMat} />
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

  return (
    <Canvas gl={GL_PROPS} camera={CAMERA_PROPS} dpr={FIXED_DPR} style={CANVAS_STYLE}>
      {/* Opaque scene background (pairs with alpha:false above). */}
      <color attach="background" args={["#05080c"]} />
      <CoreAssembly
        coreState={coreState}
        agentHue={agentHue}
        governor={governor}
        bloomEffect={bloomEffect}
      />
      {/* multisampling=0: MSAA framebuffer blits are a WKWebView flicker
          class; bloom's mipmap blur supplies the smoothing instead. */}
      <EffectComposer multisampling={0}>
        <primitive object={bloomEffect} />
      </EffectComposer>
    </Canvas>
  );
}

// Only re-render (and thus reconcile the WebGL subtree) when the discrete
// coreState changes — never on the App's 250ms housekeeping tick or the
// per-envelope telemetry dispatches that left the Canvas churning ~4+ Hz.
export default memo(CoreScene);
