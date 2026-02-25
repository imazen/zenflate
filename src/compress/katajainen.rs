//! Bounded package merge algorithm for optimal length-limited Huffman codes.
//!
//! Based on the paper "A Fast and Space-Economical Algorithm for Length-Limited Coding"
//! by Jyrki Katajainen, Alistair Moffat, and Andrew Turpin.
//!
//! Ported from zenzop's `katajainen.rs`, adapted for zenflate's type conventions
//! (frequencies as `&[u32]`, output lengths as `&mut [u8]`).

#[cfg(not(feature = "std"))]
use alloc::vec::Vec;

use core::cmp::{self, Ordering};

const NONE: u32 = u32::MAX;

struct Thing<'a> {
    nodes: &'a mut Vec<Node>,
    leaves: &'a mut Vec<Leaf>,
    lists: [List; 15],
}

struct Node {
    weight: usize,
    count: usize,
    tail: u32, // Index into nodes Vec. NONE = no tail.
}

struct Leaf {
    weight: usize,
    count: usize,
}

impl PartialEq for Leaf {
    fn eq(&self, other: &Self) -> bool {
        self.weight == other.weight
    }
}
impl Eq for Leaf {}
impl Ord for Leaf {
    fn cmp(&self, other: &Self) -> Ordering {
        self.weight.cmp(&other.weight)
    }
}
impl PartialOrd for Leaf {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy)]
struct List {
    lookahead0: u32, // Index into nodes Vec.
    lookahead1: u32, // Index into nodes Vec.
}

/// Reusable scratch buffers for `length_limited_code_lengths`.
/// Avoids repeated allocation of leaves and nodes Vecs in hot loops.
pub(crate) struct HuffmanScratch {
    leaves: Vec<Leaf>,
    nodes: Vec<Node>,
}

impl HuffmanScratch {
    pub(crate) fn new() -> Self {
        Self {
            leaves: Vec::new(),
            nodes: Vec::new(),
        }
    }
}

/// Calculates optimal length-limited Huffman code lengths using bounded package merge.
///
/// Writes results into `bit_lengths` which must be `frequencies.len()` long.
/// Uses `scratch` to avoid allocations across repeated calls.
pub(crate) fn length_limited_code_lengths(
    frequencies: &[u32],
    max_bits: usize,
    scratch: &mut HuffmanScratch,
    bit_lengths: &mut [u8],
) {
    let num_freqs = frequencies.len();
    assert!(num_freqs <= 288);
    assert!(bit_lengths.len() >= num_freqs);

    // Zero the output.
    bit_lengths[..num_freqs].fill(0);

    // Count used symbols and place them in the leaves.
    scratch.leaves.clear();
    for (i, &freq) in frequencies.iter().enumerate() {
        if freq != 0 {
            scratch.leaves.push(Leaf {
                weight: freq as usize,
                count: i,
            });
        }
    }

    let num_symbols = scratch.leaves.len();

    // Short circuit some special cases
    if num_symbols <= 2 {
        for i in 0..num_symbols {
            bit_lengths[scratch.leaves[i].count] = 1;
        }
        return;
    }

    // Sort the leaves from least frequent to most frequent.
    scratch.leaves.sort();

    let max_bits = cmp::min(num_symbols - 1, max_bits);
    assert!(max_bits <= 15);

    let capacity = max_bits * 2 * num_symbols;
    scratch.nodes.clear();
    scratch
        .nodes
        .reserve(capacity.saturating_sub(scratch.nodes.capacity()));

    scratch.nodes.push(Node {
        weight: scratch.leaves[0].weight,
        count: 1,
        tail: NONE,
    });
    scratch.nodes.push(Node {
        weight: scratch.leaves[1].weight,
        count: 2,
        tail: NONE,
    });

    let lists = [List {
        lookahead0: 0,
        lookahead1: 1,
    }; 15];

    let mut thing = Thing {
        nodes: &mut scratch.nodes,
        leaves: &mut scratch.leaves,
        lists,
    };

    // In the last list, 2 * num_symbols - 2 active chains need to be created. Two
    // are already created in the initialization. Each boundary_pm run creates one.
    let num_boundary_pm_runs = 2 * num_symbols - 4;
    for _ in 0..num_boundary_pm_runs - 1 {
        thing.boundary_pm(max_bits - 1);
    }

    thing.boundary_pm_final(max_bits - 1);

    thing.extract_bit_lengths(max_bits, bit_lengths);
}

impl Thing<'_> {
    fn boundary_pm(&mut self, index: usize) {
        let num_symbols = self.leaves.len();

        let last_count = self.nodes[self.lists[index].lookahead1 as usize].count;

        if index == 0 && last_count >= num_symbols {
            return;
        }

        self.lists[index].lookahead0 = self.lists[index].lookahead1;

        if index == 0 {
            // New leaf node in list 0.
            let tail = self.nodes[self.lists[index].lookahead0 as usize].tail;
            let idx = self.nodes.len() as u32;
            self.nodes.push(Node {
                weight: self.leaves[last_count].weight,
                count: last_count + 1,
                tail,
            });
            self.lists[index].lookahead1 = idx;
        } else {
            let weight_sum = {
                let previous_list = &self.lists[index - 1];
                self.nodes[previous_list.lookahead0 as usize].weight
                    + self.nodes[previous_list.lookahead1 as usize].weight
            };

            if last_count < num_symbols && weight_sum > self.leaves[last_count].weight {
                // New leaf inserted in list, so count is incremented.
                let tail = self.nodes[self.lists[index].lookahead0 as usize].tail;
                let idx = self.nodes.len() as u32;
                self.nodes.push(Node {
                    weight: self.leaves[last_count].weight,
                    count: last_count + 1,
                    tail,
                });
                self.lists[index].lookahead1 = idx;
            } else {
                let tail = self.lists[index - 1].lookahead1;
                let idx = self.nodes.len() as u32;
                self.nodes.push(Node {
                    weight: weight_sum,
                    count: last_count,
                    tail,
                });
                self.lists[index].lookahead1 = idx;

                // Two lookahead chains of previous list used up, create new ones.
                self.boundary_pm(index - 1);
                self.boundary_pm(index - 1);
            }
        }
    }

    fn boundary_pm_final(&mut self, index: usize) {
        let num_symbols = self.leaves.len();

        // Count of last chain of list.
        let last_count = self.nodes[self.lists[index].lookahead1 as usize].count;

        let weight_sum = {
            let previous_list = &self.lists[index - 1];
            self.nodes[previous_list.lookahead0 as usize].weight
                + self.nodes[previous_list.lookahead1 as usize].weight
        };

        if last_count < num_symbols && weight_sum > self.leaves[last_count].weight {
            let tail = self.nodes[self.lists[index].lookahead1 as usize].tail;
            let idx = self.nodes.len() as u32;
            self.nodes.push(Node {
                weight: 0,
                count: last_count + 1,
                tail,
            });
            self.lists[index].lookahead1 = idx;
        } else {
            let node_idx = self.lists[index].lookahead1 as usize;
            self.nodes[node_idx].tail = self.lists[index - 1].lookahead1;
        }
    }

    fn extract_bit_lengths(&self, max_bits: usize, bit_lengths: &mut [u8]) {
        let mut counts = [0usize; 16];
        let mut end = 16;
        let mut ptr = 15;
        let mut value = 1u8;

        let mut node_idx = self.lists[max_bits - 1].lookahead1 as usize;

        end -= 1;
        counts[end] = self.nodes[node_idx].count;

        let mut tail = self.nodes[node_idx].tail;
        while tail != NONE {
            end -= 1;
            node_idx = tail as usize;
            counts[end] = self.nodes[node_idx].count;
            tail = self.nodes[node_idx].tail;
        }

        let mut val = counts[15];

        while ptr >= end {
            while val > counts[ptr - 1] {
                bit_lengths[self.leaves[val - 1].count] = value;
                val -= 1;
            }
            ptr -= 1;
            value += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: allocating convenience wrapper for tests.
    fn llcl(frequencies: &[u32], max_bits: usize) -> Vec<u8> {
        let mut scratch = HuffmanScratch::new();
        let mut result = vec![0u8; frequencies.len()];
        length_limited_code_lengths(frequencies, max_bits, &mut scratch, &mut result);
        result
    }

    #[test]
    fn test_from_paper_3() {
        let input = [1u32, 1, 5, 7, 10, 14];
        let output = llcl(&input, 3);
        assert_eq!(output, [3, 3, 3, 3, 2, 2]);
    }

    #[test]
    fn test_from_paper_4() {
        let input = [1u32, 1, 5, 7, 10, 14];
        let output = llcl(&input, 4);
        assert_eq!(output, [4, 4, 3, 2, 2, 2]);
    }

    #[test]
    fn max_bits_7() {
        let input: [u32; 19] = [252, 0, 1, 6, 9, 10, 6, 3, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let output = llcl(&input, 7);
        assert_eq!(
            output,
            [1, 0, 6, 4, 3, 3, 3, 5, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }

    #[test]
    fn max_bits_15() {
        let input: [u32; 32] = [
            0, 0, 0, 0, 0, 0, 18, 0, 6, 0, 12, 2, 14, 9, 27, 15, 23, 15, 17, 8, 1, 0, 0, 0, 0, 0,
            0, 0, 0, 0, 0, 0,
        ];
        let output = llcl(&input, 15);
        assert_eq!(
            output,
            [
                0, 0, 0, 0, 0, 0, 3, 0, 5, 0, 4, 6, 4, 4, 3, 4, 3, 3, 3, 4, 6, 0, 0, 0, 0, 0, 0, 0,
                0, 0, 0, 0
            ]
        );
    }

    #[test]
    fn no_frequencies() {
        let input = [0u32, 0, 0, 0, 0];
        let output = llcl(&input, 7);
        assert_eq!(output, [0, 0, 0, 0, 0]);
    }

    #[test]
    fn only_one_frequency() {
        let input = [0u32, 10, 0];
        let output = llcl(&input, 7);
        assert_eq!(output, [0, 1, 0]);
    }

    #[test]
    fn only_two_frequencies() {
        let input: [u32; 17] = [0, 0, 0, 0, 252, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let output = llcl(&input, 7);
        assert_eq!(output, [0, 0, 0, 0, 1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn scratch_reuse_produces_same_results() {
        let mut scratch = HuffmanScratch::new();

        let inputs: &[&[u32]] = &[
            &[1, 1, 5, 7, 10, 14],
            &[252, 0, 1, 6, 9, 10, 6, 3, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            &[0, 0, 0, 0, 252, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        ];
        let max_bits = [3, 7, 7];

        for (input, &mb) in inputs.iter().zip(max_bits.iter()) {
            let expected = llcl(input, mb);
            let mut result = vec![0u8; input.len()];
            length_limited_code_lengths(input, mb, &mut scratch, &mut result);
            assert_eq!(result, expected);
        }
    }
}
