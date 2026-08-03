//! MILESTONE 38: ternary (base-3) weight compression for synapse
//! persistence, porting the real, benchmarked `pack_ternary`/
//! `unpack_ternary` bit-packing scheme from OBSERVE (`012-trit-search`,
//! a separate semantic-code-search project of the same author): quantize
//! values to {-1,0,+1} "trits", pack 5 trits into 1 byte (3^5=243<256)
//! for a real size reduction vs. float32. Pure integer arithmetic, no
//! external crate -- exactly the reference Python's %3 / /=3 digit
//! extraction, ported line-for-line in spirit.
//!
//! CRITICAL DIFFERENCE FROM OBSERVE'S USE CASE, WORKED THROUGH FOR REAL:
//! OBSERVE compresses 384-DIMENSION embedding VECTORS, ONE trit per
//! dimension -- coarse (3-state) per-dimension quantization is fine
//! there because real information survives in AGGREGATE across 384
//! trits; no single dimension needs to be reconstructed precisely. This
//! kernel instead persists INDIVIDUAL SCALAR synapse weights (real
//! numbers like 0.5021, learned one STDP delta at a time -- see
//! network.rs::apply_stdp; the smallest real per-trial delta this
//! kernel has actually produced, gap=2 ticks / dt~=109.89ms, is
//! `0.1 * exp(-109.89/20) ~= 0.000411`, verified in Milestone 17). ONE
//! trit per weight would only ever represent 3 distinct weight values
//! total -- a real precision disaster, not compression. So here, MANY
//! trits encode ONE scalar's magnitude as a base-3 FIXED-POINT number
//! (positional digits, exactly like decimal fixed-point but base 3) --
//! same pack_ternary/unpack_ternary bit math as OBSERVE, just a
//! different choice of what the trits collectively mean (digits of one
//! value's magnitude, not one trit per vector dimension).
//!
//! A REAL, RELATED FAILURE MODE CHECKED AND RULED OUT: some ternary
//! weight-quantization schemes (e.g. 1-trit-per-weight neural net
//! quantization, `w ~= alpha * trit` with `trit` in {-1,0,1}) need a
//! SEPARATELY stored/estimated scale factor `alpha`, and a documented
//! real bug class there is losing/omitting that scale (reconstructed
//! magnitudes wrong by ~1/alpha). That failure mode does NOT apply to
//! this design: the "scale" here is `WEIGHT_MAX_DIGIT` (a fixed,
//! EXACTLY KNOWN constant derived purely from TRITS_PER_WEIGHT, not
//! data-dependent or estimated), and it is explicitly applied on BOTH
//! sides -- multiplied in at encode (`clamped * WEIGHT_MAX_DIGIT`) and
//! divided back out at decode (`scaled / WEIGHT_MAX_DIGIT`) -- so there
//! is no missing-scale-factor bug possible by construction. Verified
//! numerically before writing this Rust (see the milestone report):
//! round-tripping 0.0, 0.5, 0.5021, 0.502053, 1.0, 0.0004106, 0.999,
//! 0.30000001 through this exact encode/decode arithmetic in Python
//! gave max reconstruction error ~3.82e-6 -- no blow-up, no
//! multiplicative drift, well inside the resolution bound below.
//!
//! TRIT COUNT, CHOSEN AND JUSTIFIED: STDP weights are clamped to [0,1]
//! (network.rs::apply_stdp's `.min(1.0)` / `.max(0.0)`). 10 trits give
//! 3^10 = 59,049 representable levels over [0,1] -- resolution
//! 1/59,048 ~= 1.694e-5. That is ~24x finer than the smallest real STDP
//! delta this kernel has verified (~4.11e-4, above), and ~118x finer
//! than a typical accumulated `train` (5-trial) change (~=0.002).
//! Quantization error introduced ONLY at the save/load boundary can
//! therefore never be mistaken for, or erase, a real learning step.
//! 10 trits pack into exactly 2 bytes with NO padding (10 is a multiple
//! of 5) via the same 5-trits-per-byte scheme OBSERVE uses -- vs. 4
//! bytes for the f32 this replaces: a real, modest 2x reduction per
//! weight (honestly smaller than OBSERVE's ~20x, because OBSERVE spends
//! 1 coarse trit per DIMENSION across a 384-dim vector, whereas this
//! spends 10 trits of PRECISION on a single scalar -- see the module
//! header above).
//!
//! Applies ONLY at the save-to-disk / load-from-disk boundary (see
//! ata.rs). The live, actively-learning weights in network.rs's
//! `Synapse.weight: f32` are never touched by this module -- STDP
//! always trains on full-precision f32, exactly as Milestone 9/10/17
//! verified.

use alloc::vec::Vec;

/// Trits spent per scalar weight -- see module doc for the reasoning.
pub const TRITS_PER_WEIGHT: usize = 10;

/// ceil(TRITS_PER_WEIGHT / 5); exactly 2 for 10 trits, no padding.
pub const PACKED_BYTES_PER_WEIGHT: usize = (TRITS_PER_WEIGHT + 4) / 5;

/// 3^TRITS_PER_WEIGHT - 1 -- the largest representable digit value,
/// and the fixed (not estimated) scale factor applied on both sides of
/// encode_weight/decode_weight. Computed once here rather than via
/// `libm::powf`/a runtime loop since TRITS_PER_WEIGHT is a compile-time
/// constant.
const WEIGHT_MAX_DIGIT: u32 = 59_048; // 3^10 - 1

/// Real port of OBSERVE's `pack_ternary`: trits in {-1,0,1} -> bytes,
/// 5 trits per byte (positional base-3 weights 1,3,9,27,81; pads with
/// trit 0 = digit 1 if the input length isn't a multiple of 5). Kept
/// general (any length) per the porting task, even though
/// encode_weight below always calls it with exactly TRITS_PER_WEIGHT
/// trits (a multiple of 5, so no padding actually occurs there).
pub fn pack_ternary(trits: &[i8]) -> Vec<u8> {
    let mut digits: Vec<i32> = trits.iter().map(|&t| t as i32 + 1).collect();
    while digits.len() % 5 != 0 {
        digits.push(1); // pad with trit 0 (matches the Python reference)
    }
    const W: [i32; 5] = [1, 3, 9, 27, 81];
    digits
        .chunks(5)
        .map(|c| (c[0] * W[0] + c[1] * W[1] + c[2] * W[2] + c[3] * W[3] + c[4] * W[4]) as u8)
        .collect()
}

/// Real port of OBSERVE's `unpack_ternary`: bytes -> trits in {-1,0,1},
/// reading base-3 digits off each byte via repeated `%3` then `/=3`
/// -- exactly like reading the digits of a number in base 3.
/// `n_trits` truncates the trailing padding pack_ternary may have
/// added.
pub fn unpack_ternary(packed: &[u8], n_trits: usize) -> Vec<i8> {
    let mut out = Vec::with_capacity(packed.len() * 5);
    for &b in packed {
        let mut v = b as i32;
        for _ in 0..5 {
            out.push(((v % 3) - 1) as i8);
            v /= 3;
        }
    }
    out.truncate(n_trits);
    out
}

/// Encodes ONE f32 weight as TRITS_PER_WEIGHT base-3 digits (a real
/// fixed-point positional representation of the whole scalar -- see
/// module doc for why this differs from OBSERVE's per-dimension
/// scheme), then packs via pack_ternary. Input is clamped to [0,1]
/// first -- STDP's own operating range. A weight set outside that
/// range via the `addsynapse weight=W` DSL command (which does not go
/// through apply_stdp's clamp) saturates to the nearest representable
/// end and is NOT restored exactly -- an honest, disclosed limitation
/// of persisting through this encoding.
pub fn encode_weight(w: f32) -> [u8; PACKED_BYTES_PER_WEIGHT] {
    let clamped = w.clamp(0.0, 1.0);
    // core's f32 has no `.round()` (needs std/libm); network.rs already
    // pulls in the libm crate for expf, so reuse it here too.
    let scaled = libm::roundf(clamped * WEIGHT_MAX_DIGIT as f32) as u32;
    let mut v = scaled;
    let mut trits = [0i8; TRITS_PER_WEIGHT];
    for t in trits.iter_mut() {
        *t = (v % 3) as i8 - 1;
        v /= 3;
    }
    let packed = pack_ternary(&trits);
    let mut out = [0u8; PACKED_BYTES_PER_WEIGHT];
    out.copy_from_slice(&packed);
    out
}

/// Reverses encode_weight: packed bytes -> reconstructed f32 in [0,1].
pub fn decode_weight(bytes: &[u8]) -> f32 {
    let trits = unpack_ternary(bytes, TRITS_PER_WEIGHT);
    let mut scaled: u32 = 0;
    let mut mul: u32 = 1;
    for &t in &trits {
        let digit = (t as i32 + 1) as u32;
        scaled += digit * mul;
        mul *= 3;
    }
    scaled as f32 / WEIGHT_MAX_DIGIT as f32
}
