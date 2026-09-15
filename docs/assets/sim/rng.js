// A port of the random number generator the Rust nodes use, accurate to the bit.
//
// `dataset::generate` and `MlpSpec::initialize` both draw from `StdRng`, which
// in rand 0.8 is ChaCha12 seeded through `SeedableRng::seed_from_u64`. Every
// node rebuilds the same dataset from the same seed rather than shipping it, so
// the browser can only stand in for a worker if it draws the identical numbers.
// Reimplementing the generator is what makes that true: the demo trains on the
// dataset the cluster would have trained on, not on a lookalike.
//
// Verified against values produced by the crate itself — see
// `docs/tests/rng.test.mjs` and `src/bin/demo_fixture.rs`.

const MUL = 6364136223846793005n;
const INC = 11634580027462260723n;
const MASK64 = (1n << 64n) - 1n;

/** ChaCha's "expand 32-byte k" constants. */
const CONSTANTS = [0x61707865, 0x3320646e, 0x79622d32, 0x6b206574];

/** Words the generator produces per refill: four 16-word blocks, as rand_chacha does. */
const BUFFER_WORDS = 64;

/** Doubled rounds for ChaCha12, which is what `StdRng` resolves to. */
const DOUBLE_ROUNDS = 6;

const scratch = new ArrayBuffer(4);
const scratchFloat = new Float32Array(scratch);
const scratchUint = new Uint32Array(scratch);

/** Reinterprets 32 bits as an `f32`, the way `f32::from_bits` does. */
function floatFromBits(bits) {
  scratchUint[0] = bits >>> 0;
  return scratchFloat[0];
}

function rotateLeft(value, bits) {
  return ((value << bits) | (value >>> (32 - bits))) >>> 0;
}

function quarterRound(state, a, b, c, d) {
  state[a] = (state[a] + state[b]) >>> 0;
  state[d] = rotateLeft(state[d] ^ state[a], 16);
  state[c] = (state[c] + state[d]) >>> 0;
  state[b] = rotateLeft(state[b] ^ state[c], 12);
  state[a] = (state[a] + state[b]) >>> 0;
  state[d] = rotateLeft(state[d] ^ state[a], 8);
  state[c] = (state[c] + state[d]) >>> 0;
  state[b] = rotateLeft(state[b] ^ state[c], 7);
}

/**
 * Expands a `u64` into a 32-byte key exactly as `SeedableRng::seed_from_u64`
 * does: eight PCG32 outputs written little-endian.
 *
 * The state is advanced before the first output is taken, which is why a seed
 * of 7 does not produce a key that looks anything like 7.
 */
export function seedFromU64(seed) {
  let state = BigInt.asUintN(64, BigInt(seed));
  const key = new Uint32Array(8);

  for (let index = 0; index < 8; index += 1) {
    state = (state * MUL + INC) & MASK64;
    const xorshifted = Number((((state >> 18n) ^ state) >> 27n) & 0xffffffffn) >>> 0;
    const rotation = Number((state >> 59n) & 0x1fn);
    key[index] =
      rotation === 0
        ? xorshifted
        : ((xorshifted >>> rotation) | (xorshifted << (32 - rotation))) >>> 0;
    // `to_le_bytes` then a little-endian read back is the identity on the word,
    // so the PCG output lands in the key unchanged.
  }

  return key;
}

/**
 * ChaCha12 in the arrangement `StdRng` uses: a 64-bit counter, a zero nonce,
 * and words handed out in the order four blocks are generated.
 */
export class StdRng {
  /** Seeds the generator from a `u64`, matching `StdRng::seed_from_u64`. */
  constructor(seed) {
    this.key = seedFromU64(seed);
    this.counter = 0n;
    this.buffer = new Uint32Array(BUFFER_WORDS);
    // Start past the end so the first draw refills rather than returning zeros.
    this.index = BUFFER_WORDS;
    this.state = new Uint32Array(16);
    this.working = new Uint32Array(16);
  }

  /** Fills one 16-word block for `counter` into `buffer` at `offset`. */
  fillBlock(offset, counter) {
    const state = this.state;
    state[0] = CONSTANTS[0];
    state[1] = CONSTANTS[1];
    state[2] = CONSTANTS[2];
    state[3] = CONSTANTS[3];
    state.set(this.key, 4);
    state[12] = Number(counter & 0xffffffffn) >>> 0;
    state[13] = Number((counter >> 32n) & 0xffffffffn) >>> 0;
    state[14] = 0;
    state[15] = 0;

    const working = this.working;
    working.set(state);

    for (let round = 0; round < DOUBLE_ROUNDS; round += 1) {
      quarterRound(working, 0, 4, 8, 12);
      quarterRound(working, 1, 5, 9, 13);
      quarterRound(working, 2, 6, 10, 14);
      quarterRound(working, 3, 7, 11, 15);
      quarterRound(working, 0, 5, 10, 15);
      quarterRound(working, 1, 6, 11, 12);
      quarterRound(working, 2, 7, 8, 13);
      quarterRound(working, 3, 4, 9, 14);
    }

    for (let word = 0; word < 16; word += 1) {
      this.buffer[offset + word] = (working[word] + state[word]) >>> 0;
    }
  }

  refill() {
    for (let block = 0; block < 4; block += 1) {
      this.fillBlock(block * 16, this.counter + BigInt(block));
    }
    this.counter += 4n;
    this.index = 0;
  }

  /** The next `u32`, as `next_u32` would return it. */
  nextU32() {
    if (this.index >= BUFFER_WORDS) {
      this.refill();
    }
    const value = this.buffer[this.index];
    this.index += 1;
    return value >>> 0;
  }

  /**
   * `rng.gen_range(low..high)` for `f32`.
   *
   * Follows `UniformFloat::sample_single`: take 23 bits into a float in [1, 2),
   * subtract one, then multiply before adding — in `f32` throughout, because
   * doing the arithmetic in double precision would round differently and the
   * sample values would drift from the cluster's.
   */
  genRangeF32(low, high) {
    const scale = Math.fround(high - low);

    for (;;) {
      const bits = this.nextU32() >>> 9;
      const value1to2 = floatFromBits(bits | (127 << 23));
      const value0to1 = Math.fround(value1to2 - 1.0);
      const result = Math.fround(Math.fround(value0to1 * scale) + low);
      // Rounding can land exactly on `high`; rand redraws rather than returning
      // a value outside the half-open range.
      if (result < high) {
        return result;
      }
    }
  }
}
