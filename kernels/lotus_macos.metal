#include <metal_stdlib>
using namespace metal;

// ============================================================================
// Lotus Mining Kernel - Metal Compute Shader for macOS
// Translated from lotus_og.cl (OpenCL) to Metal
// ============================================================================

// SHA256 initial hash values
constant uint H[8] = { 
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 
    0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19
};

// SHA256 round constants
constant uint K[64] = { 
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 
    0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 
    0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 
    0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 
    0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 
    0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 
    0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 
    0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2
};

// POW layer padding (indices 13-15 of pow_layer schedule)
constant uint POW_LAYER_PAD[3] = {
    0x80000000, 0x00000000, 0x000001a0
};

// Chain layer constant schedule array (64 words for second compress)
constant uint CHAIN_LAYER_SCHEDULE_ARRAY[64] = {
    0x80000000, 0x00000000, 0x00000000, 0x00000000, 
    0x00000000, 0x00000000, 0x00000000, 0x00000000,
    0x00000000, 0x00000000, 0x00000000, 0x00000000, 
    0x00000000, 0x00000000, 0x00000000, 0x00000200,
    0x80000000, 0x01400000, 0x00205000, 0x00005088, 
    0x22000800, 0x22550014, 0x05089742, 0xa0000020,
    0x5a880000, 0x005c9400, 0x0016d49d, 0xfa801f00, 
    0xd33225d0, 0x11675959, 0xf6e6bfda, 0xb30c1549,
    0x08b2b050, 0x9d7c4c27, 0x0ce2a393, 0x88e6e1ea, 
    0xa52b4335, 0x67a16f49, 0xd732016f, 0x4eeb2e91,
    0x5dbf55e5, 0x8eee2335, 0xe2bc5ec2, 0xa83f4394, 
    0x45ad78f7, 0x36f3d0cd, 0xd99c05e8, 0xb0511dc7,
    0x69bc7ac4, 0xbd11375b, 0xe3ba71e5, 0x3b209ff2, 
    0x18feee17, 0xe25ad9e7, 0x13375046, 0x0515089d,
    0x4f0d0f04, 0x2627484e, 0x310128d2, 0xc668b434, 
    0x420841cc, 0x62d311b8, 0xe59ba771, 0x85a7a484,
};

// Output buffer indices
constant uint FOUND = 0x80;
constant uint NFLAG = 0x7F;

// Default iterations (matches Rust default inner_iter_size = 16)
constant uint ITERATIONS = 16;

// ============================================================================
// SHA256 Helper Functions
// ============================================================================

// Right rotate
metal::uint rotr(metal::uint x, metal::uint y) {
    return (x >> y) | (x << (32 - y));
}

// Sigma 0: ROTR(2) ^ ROTR(13) ^ ROTR(22)
metal::uint sigma0(metal::uint a) {
    return rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22);
}

// Sigma 1: ROTR(6) ^ ROTR(11) ^ ROTR(25)
metal::uint sigma1(metal::uint e) {
    return rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25);
}

// Choose: (E AND F) XOR ((NOT E) AND G)
metal::uint choose(metal::uint e, metal::uint f, metal::uint g) {
    return (e & f) ^ (~e & g);
}

// Majority: (A AND B) XOR (A AND C) XOR (B AND C)
metal::uint majority(metal::uint a, metal::uint b, metal::uint c) {
    return (a & b) ^ (a & c) ^ (b & c);
}

// ============================================================================
// SHA256 Core Functions
// ============================================================================

// Extend message schedule from 16 to 64 words
void sha256_extend(thread metal::uint *schedule_array) {
    for (uint i = 16; i < 64; ++i) {
        metal::uint s0 = rotr(schedule_array[i-15],  7) ^ 
                         rotr(schedule_array[i-15], 18) ^ 
                         (schedule_array[i-15] >> 3);
        metal::uint s1 = rotr(schedule_array[i- 2], 17) ^ 
                         rotr(schedule_array[i- 2], 19) ^ 
                         (schedule_array[i- 2] >> 10);
        schedule_array[i] = schedule_array[i-16] + s0 + 
                            schedule_array[i-7] + s1;
    }
}

// SHA256 compression function using schedule array
void sha256_compress(
    const thread metal::uint *schedule_array,
    thread metal::uint *hash
) {
    metal::uint a = hash[0], b = hash[1], c = hash[2], d = hash[3],
                e = hash[4], f = hash[5], g = hash[6], h = hash[7];
    
    for (uint i = 0; i < 64; ++i) {
        metal::uint tmp1 = h + sigma1(e) + choose(e, f, g) + K[i] + schedule_array[i];
        metal::uint tmp2 = sigma0(a) + majority(a, b, c);
        h = g;
        g = f;
        f = e;
        e = d + tmp1;
        d = c;
        c = b;
        b = a;
        a = tmp1 + tmp2;
    }
    
    hash[0] += a; hash[1] += b; hash[2] += c; hash[3] += d;
    hash[4] += e; hash[5] += f; hash[6] += g; hash[7] += h;
}

// SHA256 compression function using constant schedule array
void sha256_compress_const(thread metal::uint *hash) {
    metal::uint a = hash[0], b = hash[1], c = hash[2], d = hash[3],
                e = hash[4], f = hash[5], g = hash[6], h = hash[7];
    
    for (uint i = 0; i < 64; ++i) {
        metal::uint tmp1 = h + sigma1(e) + choose(e, f, g) + K[i] + CHAIN_LAYER_SCHEDULE_ARRAY[i];
        metal::uint tmp2 = sigma0(a) + majority(a, b, c);
        h = g;
        g = f;
        f = e;
        e = d + tmp1;
        d = c;
        c = b;
        b = a;
        a = tmp1 + tmp2;
    }
    
    hash[0] += a; hash[1] += b; hash[2] += c; hash[3] += d;
    hash[4] += e; hash[5] += f; hash[6] += g; hash[7] += h;
}

// POW layer: SHA256 with message schedule from pow_layer
void sha256_pow_layer(
    const thread metal::uint *pow_layer,
    thread metal::uint *hash
) {
    for (uint i = 0; i < 8; ++i) {
        hash[i] = H[i];
    }
    
    metal::uint schedule[64];
    for (uint i = 0; i < 16; ++i) {
        schedule[i] = pow_layer[i];
    }
    
    sha256_extend(schedule);
    sha256_compress(schedule, hash);
}

// Chain layer: Two-round SHA256 (extend+compress, then const compress)
void sha256_chain_layer(
    const thread metal::uint *chain_layer_input,
    thread metal::uint *hash
) {
    for (uint i = 0; i < 8; ++i) {
        hash[i] = H[i];
    }
    
    metal::uint schedule[64];
    for (uint i = 0; i < 16; ++i) {
        schedule[i] = chain_layer_input[i];
    }
    
    sha256_extend(schedule);
    sha256_compress(schedule, hash);
    sha256_compress_const(hash);
}

// ============================================================================
// Main Mining Kernel
// ============================================================================

kernel void search(
    const device uint& offset [[buffer(0)]],
    const device uint* partial_header [[buffer(1)]],
    device uint* output [[buffer(2)]],
    uint gid [[thread_position_in_grid]]
) {
    metal::uint pow_layer[16];
    metal::uint chain_layer[8];
    metal::uint hash[8];
    
    for (uint i = 0; i < 8; ++i) {
        chain_layer[i] = partial_header[i];
    }
    
    for (uint i = 0; i < 13; ++i) {
        pow_layer[i] = partial_header[i + 8];
    }
    for (uint i = 0; i < 3; ++i) {
        pow_layer[i + 13] = POW_LAYER_PAD[i];
    }

    for (uint iteration = 0; iteration < ITERATIONS; ++iteration) {
        metal::uint nonce = offset + gid * ITERATIONS + iteration;
        pow_layer[3] = nonce;

        metal::uint pow_hash[8];
        sha256_pow_layer(pow_layer, pow_hash);
        
        metal::uint chain_input[16];
        for (uint i = 0; i < 8; ++i) {
            chain_input[i] = chain_layer[i];
        }
        for (uint i = 0; i < 8; ++i) {
            chain_input[i + 8] = pow_hash[i];
        }
        
        sha256_chain_layer(chain_input, hash);
        
        if (hash[7] == 0) {
            device atomic_uint* found_ptr = reinterpret_cast<device atomic_uint*>(&output[FOUND]);
            device atomic_uint* nonce_ptr = reinterpret_cast<device atomic_uint*>(&output[nonce & NFLAG]);
            atomic_fetch_add_explicit(found_ptr, 1u, memory_order_relaxed);
            atomic_store_explicit(nonce_ptr, nonce, memory_order_relaxed);
        }
    }
}
