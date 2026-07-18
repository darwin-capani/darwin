//! IN-PROCESS Silero VAD v5 (16 kHz) — the learned per-frame speech-probability
//! model the capture loop's VAD consults, ported to pure Rust so it runs ON the
//! audio processing thread with deterministic latency.
//!
//! ## Why in-process (the measured transport decision)
//!
//! The daemon's other model calls go over the inference-server socket (op=embed
//! etc.). For the PER-FRAME VAD that transport was MEASURED unsafe on this M1
//! Pro (2026-07-18): an op=vad RPC round-tripped 0.98 ms median idle —
//! realtime-safe alone — but **131 ms median (p99 158 ms) under a concurrent
//! op=embed load** (Python GIL + backend locks), 4x over the ~32 ms frame
//! budget, meaning the learned VAD would have degraded to the RMS fallback
//! exactly when the machine was busy. This port runs the identical elementary-op
//! core (verified against Silero's own TorchScript — see the parity test) with
//! no IPC, no server coupling, and sub-millisecond deterministic latency. The
//! cost is the ANE (Core ML) for the VAD; the "tiny aux model frees the GPU"
//! goal still holds — this never touches the MLX GPU.
//!
//! ## Architecture (fixed; extracted from the Silero v5 TorchScript)
//!
//! input x[576] = 64-sample context + 512-sample chunk (32 ms @ 16 kHz),
//! recurrent state = LSTM (h, c) of 128 each:
//!   1. STFT: reflect-pad right 64 → conv1d(basis (258,1,256), stride 128) → 4
//!      time cols; magnitude = sqrt(real² + imag²) over 129 bins → (129, 4)
//!   2. Encoder: 4 × [conv1d(k=3, pad=1) + ReLU], strides 1/2/2/1, channels
//!      129→128→64→64→128, time 4→4→2→1→1 → (128,)
//!   3. LSTM cell (gate order i, f, g, o)
//!   4. Decoder: ReLU → conv1d(128→1, k=1) → sigmoid → speech probability
//!
//! ## Weights (LOCKSTEP contract)
//!
//! Loaded from the flat file `inference/coreml_vad.py::export_native_weights`
//! writes (magic `DVADRS1\n`, u32 tensor count, then per tensor u32 element
//! count + f32 LE data, in `NATIVE_TENSORS` order). [`WEIGHT_SPEC`] here MUST
//! stay in lockstep with that table. The exporter publishes to
//! `state/models/silero_vad_v5_f32.bin` at inference-server preload (atomic,
//! idempotent); the loader validates magic + every tensor length + exact total
//! size, so a truncated/foreign file is rejected (the capture loop then stays on
//! its RMS gate, surfaced — never a garbage-weights model).

use std::path::Path;

/// New samples per streaming step (32 ms @ 16 kHz).
pub const CHUNK: usize = 512;
/// STFT look-back prepended to each chunk.
pub const CONTEXT: usize = 64;
/// The model input length: context + chunk.
pub const MODEL_INPUT: usize = CONTEXT + CHUNK;
/// Flattened recurrent state (h then c, 128 each).
pub const STATE_LEN: usize = 256;
/// The only sample rate this model supports.
pub const SAMPLE_RATE: u32 = 16_000;

/// File magic of the native weights export (coreml_vad.NATIVE_WEIGHTS_MAGIC).
const MAGIC: &[u8; 8] = b"DVADRS1\n";

// STFT geometry (fixed by the checkpoint).
const STFT_FILTERS: usize = 258; // 129 real rows + 129 imag rows
const STFT_KERNEL: usize = 256;
const STFT_STRIDE: usize = 128;
const STFT_PAD_RIGHT: usize = 64; // ReflectionPad1d((0, 64))
const BINS: usize = 129;
const STFT_T: usize = 4; // (MODEL_INPUT + pad - kernel) / stride + 1

const LSTM_H: usize = 128;

/// (name, element count) in FILE ORDER — MUST match coreml_vad.NATIVE_TENSORS.
const WEIGHT_SPEC: &[(&str, usize)] = &[
    ("stft_basis", STFT_FILTERS * STFT_KERNEL),
    ("enc0_w", 128 * 129 * 3),
    ("enc0_b", 128),
    ("enc1_w", 64 * 128 * 3),
    ("enc1_b", 64),
    ("enc2_w", 64 * 64 * 3),
    ("enc2_b", 64),
    ("enc3_w", 128 * 64 * 3),
    ("enc3_b", 128),
    ("lstm_wih", 4 * LSTM_H * LSTM_H),
    ("lstm_whh", 4 * LSTM_H * LSTM_H),
    ("lstm_bih", 4 * LSTM_H),
    ("lstm_bhh", 4 * LSTM_H),
    ("dec_w", LSTM_H),
    ("dec_b", 1),
];

/// Encoder block hyperparameters: (in_ch, out_ch, stride). Kernel 3, pad 1.
const ENC_BLOCKS: [(usize, usize, usize); 4] =
    [(129, 128, 1), (128, 64, 2), (64, 64, 2), (64, 128, 1)];

/// The loaded Silero core: weights + preallocated scratch (no per-frame heap
/// allocation on the audio thread once constructed).
pub struct SileroModel {
    tensors: Vec<Vec<f32>>,
    // Scratch buffers, sized once.
    stft_out: Vec<f32>,   // BINS * STFT_T magnitudes
    enc_a: Vec<f32>,      // ping-pong conv buffers
    enc_b: Vec<f32>,
    padded: Vec<f32>,     // MODEL_INPUT + STFT_PAD_RIGHT
}

impl SileroModel {
    /// Parse a native weights blob (magic + count + length-prefixed fp32 LE
    /// tensors in [`WEIGHT_SPEC`] order, exact total size). Rejects anything
    /// malformed so a truncated/foreign file can never become a garbage model.
    pub fn parse(data: &[u8]) -> Result<Self, String> {
        if data.len() < MAGIC.len() + 4 {
            return Err("weights file too short for header".to_string());
        }
        if &data[..MAGIC.len()] != MAGIC {
            return Err("weights file has wrong magic (not a DVADRS1 export)".to_string());
        }
        let mut off = MAGIC.len();
        let count = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
        off += 4;
        if count != WEIGHT_SPEC.len() {
            return Err(format!(
                "weights file has {count} tensors, expected {}",
                WEIGHT_SPEC.len()
            ));
        }
        let mut tensors = Vec::with_capacity(WEIGHT_SPEC.len());
        for (name, expect) in WEIGHT_SPEC {
            if data.len() < off + 4 {
                return Err(format!("weights file truncated before tensor {name}"));
            }
            let n = u32::from_le_bytes(data[off..off + 4].try_into().unwrap()) as usize;
            off += 4;
            if n != *expect {
                return Err(format!(
                    "tensor {name} has {n} elements, expected {expect}"
                ));
            }
            let bytes = 4 * n;
            if data.len() < off + bytes {
                return Err(format!("weights file truncated inside tensor {name}"));
            }
            let mut t = Vec::with_capacity(n);
            for c in data[off..off + bytes].chunks_exact(4) {
                t.push(f32::from_le_bytes(c.try_into().unwrap()));
            }
            off += bytes;
            tensors.push(t);
        }
        if off != data.len() {
            return Err(format!(
                "weights file has {} trailing bytes after the last tensor",
                data.len() - off
            ));
        }
        Ok(Self {
            tensors,
            stft_out: vec![0.0; BINS * STFT_T],
            enc_a: vec![0.0; 128 * STFT_T],
            enc_b: vec![0.0; 128 * STFT_T],
            padded: vec![0.0; MODEL_INPUT + STFT_PAD_RIGHT],
        })
    }

    /// Load + validate the native weights file (see [`Self::parse`]).
    pub fn load(path: &Path) -> Result<Self, String> {
        let data = std::fs::read(path)
            .map_err(|e| format!("cannot read weights file {}: {e}", path.display()))?;
        Self::parse(&data)
    }

    fn t(&self, idx: usize) -> &[f32] {
        &self.tensors[idx]
    }

    /// One streaming step: `x` is the 576-sample model input (context + chunk),
    /// `state` the 256-float recurrent state (h then c) from the previous step
    /// (zeros for a fresh stream) — UPDATED in place. Returns the speech
    /// probability. Pure math, no I/O, no allocation (scratch is preallocated);
    /// ~0.7M MACs, measured sub-millisecond on the M1 Pro (see the parity test).
    pub fn step(&mut self, x: &[f32; MODEL_INPUT], state: &mut [f32; STATE_LEN]) -> f32 {
        // ---- STFT: reflect-pad right, strided conv against the basis, magnitude.
        self.padded[..MODEL_INPUT].copy_from_slice(x);
        for i in 0..STFT_PAD_RIGHT {
            // torch ReflectionPad1d right edge: pad[j] = x[n - 2 - j]
            self.padded[MODEL_INPUT + i] = x[MODEL_INPUT - 2 - i];
        }
        let basis = &self.tensors[0];
        for tcol in 0..STFT_T {
            let win = &self.padded[tcol * STFT_STRIDE..tcol * STFT_STRIDE + STFT_KERNEL];
            for b in 0..BINS {
                let re_row = &basis[b * STFT_KERNEL..(b + 1) * STFT_KERNEL];
                let im_row =
                    &basis[(b + BINS) * STFT_KERNEL..(b + BINS + 1) * STFT_KERNEL];
                let mut re = 0.0f32;
                let mut im = 0.0f32;
                for k in 0..STFT_KERNEL {
                    re += re_row[k] * win[k];
                    im += im_row[k] * win[k];
                }
                self.stft_out[b * STFT_T + tcol] = (re * re + im * im).sqrt();
            }
        }

        // ---- Encoder: 4 conv1d(k=3, pad=1) + ReLU blocks (strides 1,2,2,1).
        // Ping-pong between enc_a/enc_b; input starts as stft_out (129 x 4).
        // Direct field borrows (not a self method) so src/dst borrows stay
        // disjoint for the borrow checker.
        let mut t_in = STFT_T;
        for (blk, &(cin, cout, stride)) in ENC_BLOCKS.iter().enumerate() {
            let t_out = (t_in + 2 - 3) / stride + 1;
            let w = &self.tensors[1 + blk * 2];
            let bias = &self.tensors[2 + blk * 2];
            let (src, dst): (&[f32], &mut [f32]) = match blk {
                0 => (&self.stft_out, &mut self.enc_a),
                1 => (&self.enc_a, &mut self.enc_b),
                2 => (&self.enc_b, &mut self.enc_a),
                _ => (&self.enc_a, &mut self.enc_b),
            };
            for oc in 0..cout {
                for ot in 0..t_out {
                    let mut acc = bias[oc];
                    let in_start = ot * stride; // padded index; pad = 1
                    for ic in 0..cin {
                        let wrow = &w[(oc * cin + ic) * 3..(oc * cin + ic) * 3 + 3];
                        let in_row_off = ic * t_in;
                        for (k, &wk) in wrow.iter().enumerate() {
                            // padded position in_start + k maps to input index
                            // (in_start + k - 1); positions outside [0, t_in) are 0.
                            let ip = (in_start + k) as isize - 1;
                            if ip >= 0 && (ip as usize) < t_in {
                                acc += wk * src[in_row_off + ip as usize];
                            }
                        }
                    }
                    dst[oc * t_out + ot] = acc.max(0.0);
                }
            }
            t_in = t_out;
        }
        debug_assert_eq!(t_in, 1, "encoder must reduce time 4 -> 1");
        // After 4 blocks (0..3), the last output landed in enc_b (blk 3 odd).
        // enc output = 128 channels x 1 time step.

        // ---- LSTM cell (gate order i, f, g, o).
        let wih = self.t(9);
        let whh = self.t(10);
        let bih = self.t(11);
        let bhh = self.t(12);
        let (h_prev, c_prev) = state.split_at(LSTM_H);
        let mut gates = [0.0f32; 4 * LSTM_H];
        for (g, gate) in gates.iter_mut().enumerate() {
            let wi = &wih[g * LSTM_H..(g + 1) * LSTM_H];
            let wh = &whh[g * LSTM_H..(g + 1) * LSTM_H];
            let mut acc = bih[g] + bhh[g];
            for j in 0..LSTM_H {
                acc += wi[j] * self.enc_b[j] + wh[j] * h_prev[j];
            }
            *gate = acc;
        }
        let mut h_new = [0.0f32; LSTM_H];
        let mut c_new = [0.0f32; LSTM_H];
        for j in 0..LSTM_H {
            let i_g = sigmoid(gates[j]);
            let f_g = sigmoid(gates[LSTM_H + j]);
            let g_g = gates[2 * LSTM_H + j].tanh();
            let o_g = sigmoid(gates[3 * LSTM_H + j]);
            let c = f_g * c_prev[j] + i_g * g_g;
            c_new[j] = c;
            h_new[j] = o_g * c.tanh();
        }
        state[..LSTM_H].copy_from_slice(&h_new);
        state[LSTM_H..].copy_from_slice(&c_new);

        // ---- Decoder: ReLU -> conv1d(128 -> 1, k=1) -> sigmoid.
        let dec_w = self.t(13);
        let dec_b = self.t(14)[0];
        let mut acc = dec_b;
        for j in 0..LSTM_H {
            acc += dec_w[j] * h_new[j].max(0.0);
        }
        sigmoid(acc)
    }
}

#[inline]
fn sigmoid(v: f32) -> f32 {
    1.0 / (1.0 + (-v).exp())
}

/// TEST-ONLY: build a structurally-valid weights blob with every tensor zeroed
/// except `dec_b` (the decoder bias). With zero weights the whole net collapses
/// to prob = sigmoid(dec_bias) for ANY input — a deterministic synthetic model
/// the headless tests (here and in `vad.rs`) drive the full pipeline with, no
/// real 1.2 MB weights needed.
#[cfg(test)]
pub(crate) fn synthetic_weights(dec_bias: f32) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(WEIGHT_SPEC.len() as u32).to_le_bytes());
    for (name, n) in WEIGHT_SPEC {
        out.extend_from_slice(&(*n as u32).to_le_bytes());
        if *name == "dec_b" {
            out.extend_from_slice(&dec_bias.to_le_bytes());
        } else {
            out.extend(std::iter::repeat_n(0u8, 4 * n));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rejects_malformed_files() {
        assert!(SileroModel::parse(b"short").is_err(), "too short");
        let mut bad_magic = synthetic_weights(0.0);
        bad_magic[0] = b'X';
        assert!(SileroModel::parse(&bad_magic).is_err(), "wrong magic");
        let good = synthetic_weights(0.0);
        let truncated = &good[..good.len() - 10];
        assert!(SileroModel::parse(truncated).is_err(), "truncated");
        let mut trailing = good.clone();
        trailing.extend_from_slice(&[0u8; 8]);
        assert!(SileroModel::parse(&trailing).is_err(), "trailing bytes");
        let mut wrong_count = good.clone();
        wrong_count[MAGIC.len()..MAGIC.len() + 4].copy_from_slice(&99u32.to_le_bytes());
        assert!(SileroModel::parse(&wrong_count).is_err(), "wrong tensor count");
        assert!(SileroModel::parse(&good).is_ok(), "valid blob parses");
    }

    #[test]
    fn zero_weights_collapse_to_sigmoid_of_decoder_bias() {
        // sigmoid(0)=0.5 gates + zero decoder weight => prob = sigmoid(dec_b),
        // independent of the input. This pins the full forward pass end-to-end
        // (STFT -> encoder -> LSTM -> decoder) without real weights.
        for (bias, expect) in [(10.0f32, 0.99995), (-10.0, 4.6e-5), (0.0, 0.5)] {
            let mut m = SileroModel::parse(&synthetic_weights(bias)).unwrap();
            let x = [0.25f32; MODEL_INPUT];
            let mut state = [0.0f32; STATE_LEN];
            let p = m.step(&x, &mut state);
            assert!(
                (p - expect).abs() < 1e-3,
                "dec_b={bias}: prob {p} != sigmoid(bias) {expect}"
            );
        }
    }

    #[test]
    fn lstm_state_is_updated_in_place() {
        // With zero weights: c' = sigmoid(0)*c + sigmoid(0)*tanh(0) = 0.5*c;
        // h' = sigmoid(0)*tanh(c') = 0.5*tanh(0.5*c). Feed a non-zero c and
        // verify the recurrence follows exactly.
        let mut m = SileroModel::parse(&synthetic_weights(0.0)).unwrap();
        let x = [0.0f32; MODEL_INPUT];
        let mut state = [0.0f32; STATE_LEN];
        state[LSTM_H] = 0.8; // c[0]
        m.step(&x, &mut state);
        let c_expect = 0.4f32; // 0.5 * 0.8
        let h_expect = 0.5 * c_expect.tanh();
        assert!((state[LSTM_H] - c_expect).abs() < 1e-6, "c recurrence");
        assert!((state[0] - h_expect).abs() < 1e-6, "h recurrence");
        // Untouched lanes stay zero.
        assert_eq!(state[1], 0.0);
        assert_eq!(state[LSTM_H + 1], 0.0);
    }

    /// DEVICE-GATED parity + latency: load the REAL exported weights
    /// (state/models/silero_vad_v5_f32.bin, written by the inference server's
    /// preload / `coreml_vad.py --export-native`) and replay the committed
    /// fixture (daemon/testdata/silero_parity.bin: 62 real input chunks + the
    /// expected probabilities computed by Silero's own TorchScript). Asserts
    /// every probability matches the torch reference within 2e-3 and prints the
    /// measured per-step latency. Run once by hand:
    ///   cargo test --release --bin darwind silero_parity -- --ignored --nocapture
    #[test]
    #[ignore]
    fn silero_parity_against_torch_reference_and_latency() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
        let weights = root.join("state/models/silero_vad_v5_f32.bin");
        let fixture = root.join("daemon/testdata/silero_parity.bin");
        let mut m = SileroModel::load(&weights).expect("export the weights first");
        let data = std::fs::read(&fixture).expect("committed fixture present");
        assert_eq!(&data[..8], b"DVADFX1\n", "fixture magic");
        let n = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
        let mut off = 12;
        let mut chunks = Vec::with_capacity(n);
        for _ in 0..n {
            let mut c = [0.0f32; CHUNK];
            for (i, b) in data[off..off + 4 * CHUNK].chunks_exact(4).enumerate() {
                c[i] = f32::from_le_bytes(b.try_into().unwrap());
            }
            off += 4 * CHUNK;
            chunks.push(c);
        }
        let expected: Vec<f32> = data[off..]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        assert_eq!(expected.len(), n, "fixture prob count");

        let mut state = [0.0f32; STATE_LEN];
        let mut context = [0.0f32; CONTEXT];
        let mut x = [0.0f32; MODEL_INPUT];
        let mut max_diff = 0.0f32;
        let mut lat_us: Vec<f64> = Vec::with_capacity(n);
        for (i, c) in chunks.iter().enumerate() {
            x[..CONTEXT].copy_from_slice(&context);
            x[CONTEXT..].copy_from_slice(c);
            let t0 = std::time::Instant::now();
            let p = m.step(&x, &mut state);
            lat_us.push(t0.elapsed().as_secs_f64() * 1e6);
            let d = (p - expected[i]).abs();
            if d > max_diff {
                max_diff = d;
            }
            context.copy_from_slice(&c[CHUNK - CONTEXT..]);
        }
        lat_us.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "silero-rs parity: {n} chunks, max |rust - torch| = {max_diff:.6}"
        );
        println!(
            "silero-rs per-step latency: median {:.1} us, p90 {:.1} us, max {:.1} us (frame budget 32000 us)",
            lat_us[n / 2],
            lat_us[(n * 9) / 10],
            lat_us[n - 1]
        );
        assert!(
            max_diff < 2e-3,
            "rust port drifted from the torch reference (max diff {max_diff})"
        );
    }
}
