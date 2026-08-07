//! Incremental Merkle commitment over WebAssembly linear memory.
//!
//! A snapshot of machine state has to be cheap enough to take every few thousand
//! instructions. Hashing the whole linear memory each time would cost `O(memory)` per
//! snapshot and swallow the efficiency gain the whole protocol exists to produce.
//!
//! Instead memory is committed as a binary Merkle tree over 64 KiB pages. Written pages are
//! marked dirty; a snapshot rehashes only those pages and the `O(log n)` internal nodes
//! above them. Steady-state cost is proportional to *writes*, not to memory size.
//!
//! The same structure does a second job. During dispute arbitration the coordinator needs to
//! check a single state transition without holding either worker's memory image. A page plus
//! its [`PageTree::proof`] is sufficient: [`verify`] confirms the page belongs to a claimed
//! root, and the coordinator re-executes one instruction against it.
//!
//! # Determinism
//!
//! The root depends only on page contents. It does not depend on the order in which pages
//! were written, on how many snapshots were taken along the way, or on anything about the
//! host. Two honest workers reaching the same memory state commit to the same root.
//!
//! # What the root does *not* commit to
//!
//! The tree is padded to a power of two with zero pages, so two memories whose page counts
//! round up to the same capacity — 5 pages and 8 pages, say — produce the same root when
//! their contents match. The page count is therefore **not** authenticated here.
//!
//! This is deliberate rather than overlooked. Memory size is declared in the work unit's
//! manifest, the manifest hash is part of the unit's identity, and both parties to a dispute
//! are by definition arbitrating the same unit. Binding the page count into the root as well
//! would require [`verify`] to take it as a parameter, for no gain the manifest does not
//! already provide. If the tree is ever reused in a context where the page count is *not*
//! independently authenticated, this must be revisited.

// Index arithmetic here is structural: every index is derived from `capacity`, which is a
// power of two, and the node array is allocated as `2 * capacity`. Bounds are guaranteed by
// construction, and routing every access through `get()` would obscure the tree arithmetic
// that is the point of this module.
#![allow(clippy::indexing_slicing)]

use std::collections::BTreeSet;

/// WebAssembly linear memory page size: 64 KiB.
///
/// Pages are committed at exactly this granularity so that a page index in a Merkle proof
/// means the same thing as a WebAssembly page index.
pub const PAGE_SIZE: usize = 65_536;

/// A 256-bit commitment.
pub type Hash = [u8; 32];

/// Domain separator for leaf hashes.
///
/// Leaves and internal nodes are hashed with different prefixes so that a page whose
/// contents happen to look like a concatenation of two hashes cannot be presented as an
/// internal node, or vice versa.
const LEAF_DOMAIN: u8 = 0x00;

/// Domain separator for internal node hashes. See [`LEAF_DOMAIN`].
const NODE_DOMAIN: u8 = 0x01;

/// Hash a leaf: one page of linear memory.
fn hash_leaf(page: &[u8]) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[LEAF_DOMAIN]);
    hasher.update(page);
    *hasher.finalize().as_bytes()
}

/// Hash an internal node from its two children.
fn hash_node(left: &Hash, right: &Hash) -> Hash {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&[NODE_DOMAIN]);
    hasher.update(left);
    hasher.update(right);
    *hasher.finalize().as_bytes()
}

/// Errors produced when updating a [`PageTree`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageError {
    /// The page index is beyond the tree's logical page count.
    OutOfRange {
        /// The index that was requested.
        index: usize,
        /// The number of logical pages in the tree.
        pages: usize,
    },
    /// The supplied buffer was not exactly [`PAGE_SIZE`] bytes.
    WrongSize {
        /// The length that was supplied.
        got: usize,
    },
}

impl std::fmt::Display for PageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OutOfRange { index, pages } => {
                write!(
                    f,
                    "page index {index} out of range for a {pages}-page memory"
                )
            }
            Self::WrongSize { got } => {
                write!(f, "page must be exactly {PAGE_SIZE} bytes, got {got}")
            }
        }
    }
}

impl std::error::Error for PageError {}

/// An incremental Merkle tree over the pages of a linear memory.
///
/// The tree is stored in implicit heap layout: the root lives at index 1, and the children
/// of node `i` are `2i` and `2i + 1`. Leaf `p` therefore lives at `capacity + p`, where
/// `capacity` is the page count rounded up to a power of two. Slot 0 is unused, which keeps
/// the parent/child arithmetic free of adjustments.
///
/// # Examples
///
/// ```
/// use cairn_runtime::merkle::{PageTree, PAGE_SIZE, verify};
///
/// let mut tree = PageTree::new(4);
/// let empty_root = tree.root();
///
/// let mut page = vec![0u8; PAGE_SIZE];
/// page[0] = 42;
/// tree.set_page(2, &page).unwrap();
///
/// assert_ne!(tree.root(), empty_root);
///
/// let proof = tree.proof(2).unwrap();
/// assert!(verify(&tree.root(), 2, &page, &proof));
/// ```
#[derive(Debug, Clone)]
pub struct PageTree {
    /// Logical page count, as declared by the workload manifest.
    pages: usize,
    /// `pages` rounded up to a power of two, minimum 2. Padding leaves hold zero pages.
    capacity: usize,
    /// Heap-layout node array of length `2 * capacity`.
    nodes: Vec<Hash>,
    /// Leaf indices written since the last root computation.
    dirty: BTreeSet<usize>,
}

impl PageTree {
    /// Build a tree committing to `pages` zero-filled pages.
    ///
    /// The page count is fixed for the lifetime of the tree. A workload declares its memory
    /// ceiling up front precisely so that running out of memory happens at the same
    /// instruction on every machine — see ADR-0003.
    #[must_use]
    pub fn new(pages: usize) -> Self {
        // A minimum capacity of two keeps the root a genuine internal node, so the tree
        // arithmetic has no special case for a single-page memory.
        let capacity = pages.next_power_of_two().max(2);

        let zero_leaf = hash_leaf(&vec![0u8; PAGE_SIZE]);
        let mut nodes = vec![[0u8; 32]; capacity * 2];

        for leaf in nodes.iter_mut().skip(capacity) {
            *leaf = zero_leaf;
        }
        for i in (1..capacity).rev() {
            nodes[i] = hash_node(&nodes[i * 2], &nodes[i * 2 + 1]);
        }

        Self {
            pages,
            capacity,
            nodes,
            dirty: BTreeSet::new(),
        }
    }

    /// The number of logical pages committed by this tree.
    #[must_use]
    pub fn pages(&self) -> usize {
        self.pages
    }

    /// Record new contents for one page.
    ///
    /// The page is hashed immediately but the path to the root is not recomputed until
    /// [`root`](Self::root) or [`proof`](Self::proof) is called. Writing the same page many
    /// times between snapshots therefore costs one leaf hash each and one path recomputation
    /// in total.
    ///
    /// # Errors
    ///
    /// Returns [`PageError::OutOfRange`] if `index` is beyond the declared page count, and
    /// [`PageError::WrongSize`] if `page` is not exactly [`PAGE_SIZE`] bytes.
    pub fn set_page(&mut self, index: usize, page: &[u8]) -> Result<(), PageError> {
        if index >= self.pages {
            return Err(PageError::OutOfRange {
                index,
                pages: self.pages,
            });
        }
        if page.len() != PAGE_SIZE {
            return Err(PageError::WrongSize { got: page.len() });
        }

        self.nodes[self.capacity + index] = hash_leaf(page);
        self.dirty.insert(index);
        Ok(())
    }

    /// The current commitment to the whole memory.
    ///
    /// Recomputes any paths invalidated since the last call. Calling this repeatedly without
    /// intervening writes is free.
    pub fn root(&mut self) -> Hash {
        self.recompute();
        self.nodes[1]
    }

    /// The sibling path proving page `index` belongs to the current [`root`](Self::root).
    ///
    /// The returned path runs from the leaf's sibling upward, one hash per level. Pass it to
    /// [`verify`] together with the page contents.
    ///
    /// Returns `None` if `index` is beyond the declared page count.
    pub fn proof(&mut self, index: usize) -> Option<Vec<Hash>> {
        if index >= self.pages {
            return None;
        }
        self.recompute();

        let mut path = Vec::with_capacity(self.capacity.trailing_zeros() as usize);
        let mut node = self.capacity + index;
        while node > 1 {
            // Siblings differ only in their lowest bit.
            path.push(self.nodes[node ^ 1]);
            node /= 2;
        }
        Some(path)
    }

    /// Rehash the paths from every dirty leaf to the root.
    ///
    /// Parents are collected into a set per level, so a batch of writes sharing ancestors
    /// pays for each shared node once rather than once per leaf.
    fn recompute(&mut self) {
        if self.dirty.is_empty() {
            return;
        }

        let mut level: BTreeSet<usize> = self
            .dirty
            .iter()
            .map(|&leaf| (self.capacity + leaf) / 2)
            .collect();
        self.dirty.clear();

        while !level.is_empty() {
            let mut parents = BTreeSet::new();
            for &node in &level {
                let left = self.nodes[node * 2];
                let right = self.nodes[node * 2 + 1];
                self.nodes[node] = hash_node(&left, &right);
                if node > 1 {
                    parents.insert(node / 2);
                }
            }
            level = parents;
        }
    }
}

/// Check that `page` sits at `index` in the memory committed by `root`.
///
/// This is what lets the coordinator arbitrate a dispute without holding either worker's
/// memory image: it is handed one page and one proof, confirms the page is genuinely part of
/// the state the worker committed to, and re-executes a single instruction against it.
#[must_use]
pub fn verify(root: &Hash, index: usize, page: &[u8], proof: &[Hash]) -> bool {
    if page.len() != PAGE_SIZE {
        return false;
    }

    let mut running = hash_leaf(page);
    let mut position = index;

    for sibling in proof {
        // An even position is a left child.
        running = if position % 2 == 0 {
            hash_node(&running, sibling)
        } else {
            hash_node(sibling, &running)
        };
        position /= 2;
    }

    &running == root
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    /// A page filled with `byte`, with `marker` written at offset 0 to distinguish pages
    /// that would otherwise be identical.
    fn page(byte: u8, marker: u8) -> Vec<u8> {
        let mut p = vec![byte; PAGE_SIZE];
        p[0] = marker;
        p
    }

    #[test]
    fn empty_tree_root_is_deterministic() {
        let a = PageTree::new(16).root();
        let b = PageTree::new(16).root();
        assert_eq!(a, b, "two empty trees of the same size must agree");
    }

    #[test]
    fn different_capacities_produce_different_roots() {
        // Trees of different depth cannot collide, because depth changes how many times the
        // zero leaf is folded into the root.
        assert_ne!(PageTree::new(4).root(), PageTree::new(8).root());
    }

    #[test]
    fn page_counts_sharing_a_capacity_do_collide() {
        // Documents the known limitation stated in the module docs rather than pretending it
        // is absent: padding makes 5 and 8 pages indistinguishable. Safe only because the
        // page count is authenticated by the work unit manifest. If this test ever starts
        // failing, someone bound the page count into the root — update the module docs.
        assert_eq!(PageTree::new(5).root(), PageTree::new(8).root());
    }

    #[test]
    fn writing_a_page_changes_the_root() {
        let mut tree = PageTree::new(8);
        let before = tree.root();
        tree.set_page(3, &page(0, 1)).unwrap();
        assert_ne!(tree.root(), before);
    }

    #[test]
    fn restoring_contents_restores_the_root() {
        // The commitment is a function of state alone. A worker that writes a page and then
        // writes it back must land on exactly the root it started from, or honest workers
        // taking snapshots at different moments would diverge.
        let mut tree = PageTree::new(8);
        let original = tree.root();

        tree.set_page(5, &page(0xAB, 0xCD)).unwrap();
        assert_ne!(tree.root(), original);

        tree.set_page(5, &vec![0u8; PAGE_SIZE]).unwrap();
        assert_eq!(tree.root(), original);
    }

    #[test]
    fn write_order_does_not_affect_the_root() {
        let mut forward = PageTree::new(8);
        for i in 0..8 {
            forward.set_page(i, &page(i as u8, 0xF0)).unwrap();
        }

        let mut reverse = PageTree::new(8);
        for i in (0..8).rev() {
            reverse.set_page(i, &page(i as u8, 0xF0)).unwrap();
        }

        assert_eq!(forward.root(), reverse.root());
    }

    #[test]
    fn incremental_matches_rebuilt_from_scratch() {
        // The incremental path is an optimisation and must be indistinguishable from
        // rebuilding the tree, including after many overwrites of the same page.
        let mut incremental = PageTree::new(16);
        for round in 0..4u8 {
            for i in 0..16 {
                incremental.set_page(i, &page(round, i as u8)).unwrap();
            }
            // Force intermediate recomputation, which a snapshot would also do.
            let _ = incremental.root();
        }

        let mut rebuilt = PageTree::new(16);
        for i in 0..16 {
            rebuilt.set_page(i, &page(3, i as u8)).unwrap();
        }

        assert_eq!(incremental.root(), rebuilt.root());
    }

    #[test]
    fn proofs_verify_for_every_page() {
        let mut tree = PageTree::new(8);
        let pages: Vec<Vec<u8>> = (0..8).map(|i| page(i as u8, 0x11)).collect();
        for (i, p) in pages.iter().enumerate() {
            tree.set_page(i, p).unwrap();
        }

        let root = tree.root();
        for (i, p) in pages.iter().enumerate() {
            let proof = tree.proof(i).expect("index is in range");
            assert!(verify(&root, i, p, &proof), "proof failed for page {i}");
        }
    }

    #[test]
    fn proof_rejects_tampered_contents() {
        let mut tree = PageTree::new(8);
        let genuine = page(0x22, 0x33);
        tree.set_page(4, &genuine).unwrap();

        let root = tree.root();
        let proof = tree.proof(4).unwrap();

        let mut forged = genuine.clone();
        forged[PAGE_SIZE - 1] ^= 0x01;

        assert!(verify(&root, 4, &genuine, &proof));
        assert!(
            !verify(&root, 4, &forged, &proof),
            "a single flipped bit must invalidate the proof"
        );
    }

    #[test]
    fn proof_rejects_the_wrong_index() {
        let mut tree = PageTree::new(8);
        let p = page(0x44, 0x55);
        tree.set_page(1, &p).unwrap();

        let root = tree.root();
        let proof = tree.proof(1).unwrap();

        assert!(verify(&root, 1, &p, &proof));
        assert!(!verify(&root, 2, &p, &proof));
    }

    #[test]
    fn proof_length_is_logarithmic() {
        let mut tree = PageTree::new(1024);
        let proof = tree.proof(0).unwrap();
        assert_eq!(proof.len(), 10, "1024 pages is a depth-10 tree");
    }

    #[test]
    fn single_page_memory_is_well_formed() {
        let mut tree = PageTree::new(1);
        let p = page(0x66, 0x77);
        tree.set_page(0, &p).unwrap();

        let root = tree.root();
        let proof = tree.proof(0).unwrap();
        assert!(verify(&root, 0, &p, &proof));
    }

    #[test]
    fn rejects_out_of_range_and_wrong_size() {
        let mut tree = PageTree::new(4);

        assert_eq!(
            tree.set_page(4, &vec![0u8; PAGE_SIZE]),
            Err(PageError::OutOfRange { index: 4, pages: 4 })
        );
        assert_eq!(
            tree.set_page(0, &[0u8; 128]),
            Err(PageError::WrongSize { got: 128 })
        );
        assert!(tree.proof(4).is_none());
    }

    #[test]
    fn leaf_and_node_hashing_are_domain_separated() {
        // Without domain separation a page whose bytes happen to be two concatenated hashes
        // could be presented as an internal node. Confirm the two hash functions disagree on
        // structurally identical input.
        let left = [0xAAu8; 32];
        let right = [0xBBu8; 32];

        let mut as_page = Vec::with_capacity(64);
        as_page.extend_from_slice(&left);
        as_page.extend_from_slice(&right);

        assert_ne!(hash_leaf(&as_page), hash_node(&left, &right));
    }
}
