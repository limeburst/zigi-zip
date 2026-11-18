use rand::Rng;

const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";

/// For codes people type by hand: lowercase and digits, without the ones
/// that look alike (0 and o, 1 and l and i).
const TYPEABLE: &[u8] = b"23456789abcdefghjkmnpqrstuvwxyz";

pub fn generate(len: usize) -> String {
    from(ALPHABET, len)
}

/// A code of `len` from the typeable letters.
pub fn generate_typeable(len: usize) -> String {
    from(TYPEABLE, len)
}

fn from(alphabet: &[u8], len: usize) -> String {
    let mut rng = rand::thread_rng();
    (0..len)
        .map(|_| alphabet[rng.gen_range(0..alphabet.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typeable() {
        assert_eq!(TYPEABLE.len(), 31);
        for _ in 0..1000 {
            let c = generate_typeable(10);
            assert_eq!(c.len(), 10);
            assert!(c.chars().all(|ch| TYPEABLE.contains(&(ch as u8))), "{c}");
            assert!(!c.contains(['0', '1', 'o', 'l', 'i']), "{c}");
        }
    }
}
