use crate::ring::Ring;

pub fn count(ring: &Ring) -> usize {
    ring.items.len()
}
