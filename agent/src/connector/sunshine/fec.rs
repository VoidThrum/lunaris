//! Reed-Solomon erasure code over GF(2^8) as GameStream uses it for video
//! and audio FEC.
//!
//! Ported from nanors `rs.c` (the code both Moonlight and Sunshine link, MIT,
//! sleepybishop/nanors, in moonlight-common-c/nanors) and its `deps/obl`
//! field tables. The parity matrix construction is part of the wire format:
//! a generic Reed-Solomon crate with another matrix does not interoperate.
//! Checked byte for byte against vectors printed by nanors itself (see
//! `fec_tests.rs`).

use std::sync::OnceLock;

/// nanors rs.h: `RS8_DATA_SHARDS_MAX` (data + parity shards of one block).
pub const MAX_SHARDS: usize = 255;

/// The field tables of nanors `deps/obl/gf2_8_tables.h`: "generated with
/// polynomial: 285" (x^8 + x^4 + x^3 + x^2 + 1), generator 2.
struct Gf {
    inv: [u8; 256],
    /// `mul[(a << 8) | b]`, as `GF2_8_MUL` in oblas_lite.c: `oblas_lite_init`.
    mul: Vec<u8>,
}

fn gf() -> &'static Gf {
    static GF: OnceLock<Gf> = OnceLock::new();
    GF.get_or_init(|| {
        let mut exp = [0u8; 510];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for (i, slot) in exp.iter_mut().enumerate().take(255) {
            *slot = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 285;
            }
        }
        for i in 255..510 {
            exp[i] = exp[i - 255];
        }
        let mut inv = [0u8; 256];
        for a in 1..256usize {
            inv[a] = exp[(255 - log[a] as usize) % 255];
        }
        let mut mul = vec![0u8; 65536];
        for a in 1..256usize {
            for b in 1..256usize {
                mul[(a << 8) | b] = exp[log[a] as usize + log[b] as usize];
            }
        }
        Gf { inv, mul }
    })
}

/// rs.c: `axpy` (a ^= u * b).
fn axpy(a: &mut [u8], b: &[u8], u: u8) {
    if u == 0 {
        return;
    }
    if u == 1 {
        for (x, y) in a.iter_mut().zip(b) {
            *x ^= *y;
        }
        return;
    }
    let row = &gf().mul[(u as usize) << 8..((u as usize) << 8) + 256];
    for (x, y) in a.iter_mut().zip(b) {
        *x ^= row[*y as usize];
    }
}

/// rs.c: `scal` (a *= u).
fn scal(a: &mut [u8], u: u8) {
    if u < 2 {
        return;
    }
    let row = &gf().mul[(u as usize) << 8..((u as usize) << 8) + 256];
    for x in a.iter_mut() {
        *x = row[*x as usize];
    }
}

/// `shards[a] ^= u * shards[b]` over the first `bs` bytes, a != b.
fn axpy_shards(shards: &mut [Vec<u8>], a: usize, b: usize, u: u8, bs: usize) {
    debug_assert_ne!(a, b);
    if a < b {
        let (lo, hi) = shards.split_at_mut(b);
        axpy(&mut lo[a][..bs], &hi[0][..bs], u);
    } else {
        let (lo, hi) = shards.split_at_mut(a);
        axpy(&mut hi[0][..bs], &lo[b][..bs], u);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FecError {
    /// Shard counts outside what the code allows.
    BadParameters,
    /// Fewer shards present than data shards.
    NotEnoughShards,
    /// A zero pivot (cannot happen with nanors' own matrix).
    Singular,
}

/// One (data shards, parity shards) code. rs.c: `reed_solomon`.
pub struct ReedSolomon {
    ds: usize,
    ps: usize,
    /// Parity matrix, `ps` rows of `ds` coefficients. rs.c: `rs->p`.
    p: Vec<u8>,
}

impl ReedSolomon {
    /// rs.c: `reed_solomon_new` / `reed_solomon_new_static`: the parity
    /// matrix is `p[j][i] = INV[(ps + i) ^ j]` (a Cauchy matrix).
    pub fn new(data_shards: usize, parity_shards: usize) -> Result<Self, FecError> {
        if data_shards == 0 || parity_shards == 0 || data_shards + parity_shards > MAX_SHARDS {
            return Err(FecError::BadParameters);
        }
        let inv = &gf().inv;
        let mut p = vec![0u8; data_shards * parity_shards];
        for j in 0..parity_shards {
            for i in 0..data_shards {
                p[j * data_shards + i] = inv[(parity_shards + i) ^ j];
            }
        }
        Ok(Self { ds: data_shards, ps: parity_shards, p })
    }

    /// The same code with the parity matrix replaced, as the audio stream
    /// does on both sides (RtpAudioQueue.c: `RtpaInitializeQueue`,
    /// stream.cpp: `audioBroadcastThread`: `memcpy(rs->p, parity, ...)`).
    pub fn with_parity_matrix(data_shards: usize, parity_shards: usize, matrix: &[u8]) -> Result<Self, FecError> {
        let mut rs = Self::new(data_shards, parity_shards)?;
        if matrix.len() != rs.p.len() {
            return Err(FecError::BadParameters);
        }
        rs.p.copy_from_slice(matrix);
        Ok(rs)
    }

    pub fn data_shards(&self) -> usize {
        self.ds
    }

    pub fn parity_shards(&self) -> usize {
        self.ps
    }

    pub fn parity_matrix(&self) -> &[u8] {
        &self.p
    }

    /// Computes the parity shards `shards[ds..ds+ps]` from the data shards
    /// `shards[..ds]`, over the first `bs` bytes of each.
    /// rs.c: `reed_solomon_encode` -> `gemm`.
    pub fn encode(&self, shards: &mut [Vec<u8>], bs: usize) -> Result<(), FecError> {
        if shards.len() < self.ds + self.ps || bs == 0 || shards.iter().take(self.ds + self.ps).any(|s| s.len() < bs) {
            return Err(FecError::BadParameters);
        }
        let (data, parity) = shards.split_at_mut(self.ds);
        for row in 0..self.ps {
            let out = &mut parity[row][..bs];
            out.fill(0);
            for (idx, d) in data.iter().enumerate() {
                axpy(out, &d[..bs], self.p[row * self.ds + idx]);
            }
        }
        Ok(())
    }

    /// Rebuilds the data shards marked missing (`marks[i] == true`) from the
    /// shards present; parity shards are left as they are. Every shard, also
    /// a missing one, must be a buffer of at least `bs` bytes.
    /// rs.c: `reed_solomon_decode` -> `invert_mat`.
    pub fn decode(&self, shards: &mut [Vec<u8>], marks: &[bool], bs: usize) -> Result<(), FecError> {
        let (ds, ts) = (self.ds, self.ds + self.ps);
        if shards.len() < ts || marks.len() < ts || bs == 0 || shards.iter().take(ts).any(|s| s.len() < bs) {
            return Err(FecError::BadParameters);
        }

        // Column permutation: the data shards present first, then the erased.
        let erasures: Vec<usize> = (0..ds).filter(|&i| marks[i]).collect();
        let gaps = erasures.len();
        if gaps == 0 {
            return Ok(());
        }
        let mut colperm: Vec<usize> = (0..ds).filter(|&i| !marks[i]).collect();
        colperm.extend_from_slice(&erasures);

        // One present parity shard for each erased data shard, in order.
        let mut rowperm = Vec::with_capacity(gaps);
        let mut j = ds;
        for _ in 0..gaps {
            while j < ts && marks[j] {
                j += 1;
            }
            if j >= ts {
                return Err(FecError::NotEnoughShards);
            }
            rowperm.push(j - ds);
            j += 1;
        }
        for (i, &e) in erasures.iter().enumerate() {
            let src = ds + rowperm[i];
            let (lo, hi) = shards.split_at_mut(src);
            lo[e][..bs].copy_from_slice(&hi[0][..bs]);
        }

        // rs.c: invert_mat(rs, rs->p, wrk, data, ds - gaps, ds, bs, colperm, rowperm)
        let v0 = ds - gaps;
        let w = gaps;
        let inv = &gf().inv;
        let mut wrk = vec![0u8; w * w];
        for i in 0..w {
            let dr = rowperm[i] * ds;
            for j in 0..w {
                wrk[i * w + j] = self.p[dr + colperm[v0 + j]];
            }
        }
        // Take the part of the present data shards out of the parity copies.
        for (i, &dr_row) in rowperm.iter().enumerate() {
            let dr = dr_row * ds;
            for &col in colperm.iter().take(v0) {
                let u = self.p[dr + col];
                axpy_shards(shards, colperm[v0 + i], col, u, bs);
            }
        }
        // Forward elimination (no pivoting, as in nanors).
        for x in 0..w {
            let pivot = wrk[x * w + x];
            if pivot == 0 {
                return Err(FecError::Singular);
            }
            let u = inv[pivot as usize];
            scal(&mut wrk[x * w + x..x * w + w], u);
            scal(&mut shards[colperm[v0 + x]][..bs], u);
            for row in x + 1..w {
                let u = wrk[row * w + x];
                let (upper, lower) = wrk.split_at_mut(row * w);
                axpy(&mut lower[..w], &upper[x * w..x * w + w], u);
                axpy_shards(shards, colperm[v0 + row], colperm[v0 + x], u, bs);
            }
        }
        // Back substitution.
        for x in (0..w).rev() {
            for row in 0..x {
                let u = wrk[row * w + x];
                axpy_shards(shards, colperm[v0 + row], colperm[v0 + x], u, bs);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "fec_tests.rs"]
mod tests;
