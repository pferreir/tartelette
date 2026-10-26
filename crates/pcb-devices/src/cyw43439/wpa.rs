//! WPA2-PSK key derivation (PBKDF2-HMAC-SHA1), so a join with a precomputed
//! PSK can be checked against the mock network's passphrase.

fn sha1(data: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [0x6745_2301, 0xEFCD_AB89, 0x98BA_DCFE, 0x1032_5476, 0xC3D2_E1F0];
    let mut block = [0u8; 64];
    let total = data.len() + 1 + 8;
    let padded = total.div_ceil(64) * 64;
    let bits = (data.len() as u64) * 8;
    let mut i = 0;
    while i < padded {
        for (j, b) in block.iter_mut().enumerate() {
            let at = i + j;
            *b = if at < data.len() {
                data[at]
            } else if at == data.len() {
                0x80
            } else if at >= padded - 8 {
                (bits >> (8 * (padded - 1 - at))) as u8
            } else {
                0
            };
        }
        compress(&mut h, &block);
        i += 64;
    }
    let mut out = [0u8; 20];
    for (o, w) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        o.copy_from_slice(&w.to_be_bytes());
    }
    out
}

fn compress(h: &mut [u32; 5], block: &[u8; 64]) {
    let mut w = [0u32; 80];
    for (i, c) in block.as_chunks::<4>().0.iter().enumerate() {
        w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
    }
    for i in 16..80 {
        w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
    }
    let [mut a, mut b, mut c, mut d, mut e] = *h;
    for (i, wi) in w.iter().enumerate() {
        let (f, k) = match i {
            0..20 => ((b & c) | (!b & d), 0x5A82_7999),
            20..40 => (b ^ c ^ d, 0x6ED9_EBA1),
            40..60 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
            _ => (b ^ c ^ d, 0xCA62_C1D6u32),
        };
        let t = a.rotate_left(5).wrapping_add(f).wrapping_add(e).wrapping_add(k).wrapping_add(*wi);
        (e, d, c, b, a) = (d, c, b.rotate_left(30), a, t);
    }
    for (x, y) in h.iter_mut().zip([a, b, c, d, e]) {
        *x = x.wrapping_add(y);
    }
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = alloc::vec::Vec::with_capacity(64 + msg.len());
    inner.extend(k.iter().map(|b| b ^ 0x36));
    inner.extend_from_slice(msg);
    let ih = sha1(&inner);
    let mut outer = alloc::vec::Vec::with_capacity(84);
    outer.extend(k.iter().map(|b| b ^ 0x5C));
    outer.extend_from_slice(&ih);
    sha1(&outer)
}

/// The 256-bit pairwise master key for `passphrase` on network `ssid`.
pub fn psk(passphrase: &[u8], ssid: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (block, chunk) in out.chunks_mut(20).enumerate() {
        let mut salt = alloc::vec::Vec::from(ssid);
        salt.extend_from_slice(&(block as u32 + 1).to_be_bytes());
        let mut u = hmac_sha1(passphrase, &salt);
        let mut t = u;
        for _ in 1..4096 {
            u = hmac_sha1(passphrase, &u);
            for (a, b) in t.iter_mut().zip(u) {
                *a ^= b;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> alloc::string::String {
        b.iter().map(|x| alloc::format!("{x:02x}")).collect()
    }

    #[test]
    fn sha1_known_answers() {
        assert_eq!(hex(&sha1(b"abc")), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(hex(&sha1(b"")), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        let long = [b'a'; 1000];
        assert_eq!(hex(&sha1(&long)), "291e9a6c66994949b57ba5e650361e98fc36b1ba");
    }

    #[test]
    fn ieee_80211i_test_vector() {
        assert_eq!(hex(&psk(b"password", b"IEEE")), "f42c6fc52df0ebef9ebb4b90b38a5f902e83fe1b135a70e23aed762e9710a12e");
    }
}
