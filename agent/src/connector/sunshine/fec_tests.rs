//! Reed-Solomon tests: parity byte for byte against nanors (vectors printed
//! by a throwaway C program linking moonlight-common-c/nanors/rs.c and
//! deps/obl/oblas_lite.c, the build Moonlight and Sunshine use), and
//! encode/decode cross-checks.

use super::*;

/// The data generator of the vector program: `st = st * 1103515245 + 12345;
/// byte = st >> 16`, seeded with `ds * 1000 + ps + bs`.
pub(crate) struct Lcg(pub u32);
impl Lcg {
    pub fn byte(&mut self) -> u8 {
        self.0 = self.0.wrapping_mul(1103515245).wrapping_add(12345);
        (self.0 >> 16) as u8
    }
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.byte()).collect()
    }
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
}

const AUDIO_MATRIX: [u8; 8] = [0x77, 0x40, 0x38, 0x0e, 0xc7, 0xa7, 0x0d, 0x6c];

/// (data shards, parity shards, block size, audio matrix, parity shards in hex)
const NANORS_VECTORS: &[(usize, usize, usize, u8, &str)] = &[
    (4, 2, 16, 0, "35bb9138230b88e708c75ac5a74d591de792f2277957b516f36adc62643ed718"),
    (4, 2, 16, 1, "c39db7f79be32358ad0a44c166c7770d9262785291a201a3ca6e470b026e3a37"),
    (1, 2, 8, 0, "bebe0b03f239073cd4d4f902572ef128"),
    (10, 2, 32, 0, "4bc7b7c780624669fd9c4ce5a17324176e1ece2bde7ad985587ad6d338b15b8f15452132781c713187e14780200a2f00a2a196692f687f71a470b115114f11ea"),
    (20, 4, 24, 0, "060e45f683427e564ec627e7a164324437c9c28ba76eb087c05dd09c84ab08682c8fbcd7f0606e92efb522db121d8a72be38e2a8974e5b6371706cbc1e851d29d7c41f1063dd871bb2649649ba45d44f8aa3a16dff2ef1f78b6a7d87ecf4564c"),
    (7, 2, 33, 0, "47cc5f085103a89c22ae8006eb9f9cfcff83e00b25eefb3dc4d04df98f9836458a01d0052918c6c30fc05d031886f800f7a4b8c15521a9d7d83f05e7b908409c4e4c"),
    (100, 20, 8, 0, "2cab9ad4679ee3138ae125d608ab6119a6675fcf2ece3faa06dbb9a160a2b0c58879685a61886e2b779b87b75dad5c2b1baa8191672f579f612dba905ae6f44c87054d69aaad5a4f9e3076b73a4341fa906de77957b1814a3e2c6069fb1b44990f7fcc3ff4d700856d1ae6bed993aa13e57d8627491d133ef001eb47c18e75a426f5b70e89ddd5dd22acd1384f31b24e75e915b46406635df3c994bb6b10bc28"),
    (203, 52, 4, 0, "abbc0876ecd278033ad3ef47c242af744da7e197a6b6c5d401de9f570e0efb7eb0097a715b7be0e626006b68c28c7524d3015cb4cd4eb46c1dfb52e99e24bdb3f93fdc5cc1e7438a868c84c61a4fbea2ea533e9c8ed43639b69305db2ec75e5e427bca993e50b6b07db288e03a8a34f570e1976a4461d82b0c96a77a09c848ce9d240d6a281e64aa079182c3c04996553a1a06241ddfd2c59897ccdf934260145f17c378d31c08ff96e3c47874bdcdfe7ab6d86d38792baa00634157402cf5e03080f962078dc187d5e13d2a6b13199e"),
    (254, 1, 3, 0, "119c1a"),
];

fn make(ds: usize, ps: usize, audio: bool) -> ReedSolomon {
    if audio {
        ReedSolomon::with_parity_matrix(ds, ps, &AUDIO_MATRIX).unwrap()
    } else {
        ReedSolomon::new(ds, ps).unwrap()
    }
}

fn data_for(ds: usize, ps: usize, bs: usize) -> Vec<Vec<u8>> {
    let mut lcg = Lcg((ds * 1000 + ps + bs) as u32);
    let mut shards: Vec<Vec<u8>> = (0..ds).map(|_| lcg.bytes(bs)).collect();
    shards.extend((0..ps).map(|_| vec![0u8; bs]));
    shards
}

#[test]
fn parity_matrix_matches_nanors() {
    // Printed from `rs->p` after reed_solomon_new().
    assert_eq!(ReedSolomon::new(4, 2).unwrap().parity_matrix(), hex("8ef447a7f48ea747"));
    assert_eq!(ReedSolomon::new(5, 1).unwrap().parity_matrix(), hex("018ef447a7"));
    assert_eq!(ReedSolomon::new(3, 3).unwrap().parity_matrix(), hex("f447a78ea747017aba"));
    assert_eq!(ReedSolomon::new(10, 2).unwrap().parity_matrix(), hex("8ef447a77abaad9ddd98f48ea747ba7a9dad98dd"));
}

#[test]
fn parity_matches_nanors_byte_for_byte() {
    for &(ds, ps, bs, audio, parity) in NANORS_VECTORS {
        let rs = make(ds, ps, audio != 0);
        let mut shards = data_for(ds, ps, bs);
        rs.encode(&mut shards, bs).unwrap();
        let got: Vec<u8> = shards[ds..].concat();
        assert_eq!(got, hex(parity), "ds {ds} ps {ps} bs {bs} audio {audio}");
    }
}

#[test]
fn decode_recovers_from_nanors_parity() {
    // Parity taken from the vectors (not from our encoder), data erased.
    for &(ds, ps, bs, audio, parity) in NANORS_VECTORS {
        let rs = make(ds, ps, audio != 0);
        let original = data_for(ds, ps, bs);
        let parity = hex(parity);
        let gaps = ps.min(ds);
        for variant in 0..3 {
            let mut shards = original.clone();
            for k in 0..ps {
                shards[ds + k] = parity[k * bs..(k + 1) * bs].to_vec();
            }
            let mut marks = vec![false; ds + ps];
            let lost: Vec<usize> = match variant {
                0 => (0..gaps).collect(),
                1 => (0..gaps).map(|k| ds - 1 - k).collect(),
                // one fewer data shard lost, and the first parity shard lost
                _ => (0..gaps.saturating_sub(1)).map(|k| (k * 7 + 1) % ds).chain([ds]).collect(),
            };
            for &i in &lost {
                marks[i] = true;
                shards[i] = vec![0xEE; bs];
            }
            rs.decode(&mut shards, &marks, bs).unwrap();
            assert_eq!(&shards[..ds], &original[..ds], "ds {ds} ps {ps} variant {variant}");
        }
    }
}

#[test]
fn every_erasure_pattern_small_codes() {
    for (ds, ps, audio) in [(4usize, 2usize, true), (4, 2, false), (3, 3, false), (6, 3, false), (1, 2, false)] {
        let rs = make(ds, ps, audio);
        let bs = 9;
        let mut original = data_for(ds, ps, bs);
        rs.encode(&mut original, bs).unwrap();
        let ts = ds + ps;
        for mask in 0u32..(1 << ts) {
            let mut shards = original.clone();
            let marks: Vec<bool> = (0..ts).map(|i| mask & (1 << i) != 0).collect();
            for i in 0..ts {
                if marks[i] {
                    shards[i] = vec![0x55; bs];
                }
            }
            let res = rs.decode(&mut shards, &marks, bs);
            let lost_data = marks[..ds].iter().filter(|m| **m).count();
            let have_parity = marks[ds..].iter().filter(|m| !**m).count();
            if lost_data <= have_parity {
                assert_eq!(res, Ok(()));
                assert_eq!(&shards[..ds], &original[..ds], "ds {ds} ps {ps} mask {mask:b}");
            } else {
                assert_eq!(res, Err(FecError::NotEnoughShards));
            }
        }
    }
}

#[test]
fn rejects_bad_parameters() {
    assert!(ReedSolomon::new(0, 1).is_err());
    assert!(ReedSolomon::new(1, 0).is_err());
    assert!(ReedSolomon::new(200, 56).is_err());
    assert!(ReedSolomon::new(203, 52).is_ok());
    assert!(ReedSolomon::with_parity_matrix(4, 2, &[1, 2, 3]).is_err());
}
