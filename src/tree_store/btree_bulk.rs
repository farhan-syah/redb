use crate::Result;
use crate::tree_store::btree::BtreeMut;
use crate::tree_store::btree_base::{BranchBuilder, BtreeHeader, Checksum, DEFERRED, LeafBuilder};
use crate::tree_store::page_store::{MAX_PAIR_LENGTH, Page, PageNumber};
use std::mem::size_of;

const MAX_ENCODED_PAGE_BYTES: usize = if usize::BITS <= 32 {
    isize::MAX as usize
} else {
    u32::MAX as usize
};

fn leaf_required_bytes(
    pairs: usize,
    payload_bytes: usize,
    fixed_key_size: Option<usize>,
    fixed_value_size: Option<usize>,
) -> Option<usize> {
    let mut bytes = 4usize;
    if fixed_key_size.is_none() {
        bytes = bytes.checked_add(pairs.checked_mul(size_of::<u32>())?)?;
    }
    if fixed_value_size.is_none() {
        bytes = bytes.checked_add(pairs.checked_mul(size_of::<u32>())?)?;
    }
    bytes.checked_add(payload_bytes)
}

fn branch_required_bytes(
    keys: usize,
    key_bytes: usize,
    fixed_key_size: Option<usize>,
) -> Option<usize> {
    let child_bytes = PageNumber::serialized_size().checked_add(size_of::<Checksum>())?;
    let mut bytes = 8usize.checked_add(child_bytes.checked_mul(keys.checked_add(1)?)?)?;
    if fixed_key_size.is_none() {
        bytes = bytes.checked_add(keys.checked_mul(size_of::<u32>())?)?;
    }
    bytes.checked_add(key_bytes)
}
use crate::types::{Key, Value};

struct Child {
    page: PageNumber,
    max_key: Vec<u8>,
}

#[derive(Default)]
struct BranchLevel {
    children: Vec<Child>,
    emitted: bool,
}

/// One-shot bottom-up builder for an empty normal table.
///
/// Only the current leaf and one partial branch group per level are retained.
pub(crate) struct BtreeBulkBuilder<'a, K: Key + 'static, V: Value + 'static> {
    tree: BtreeMut<'a, K, V>,
    leaf: Vec<(Vec<u8>, Vec<u8>)>,
    leaf_key_bytes: usize,
    leaf_value_bytes: usize,
    levels: Vec<BranchLevel>,
    target_page_size: usize,
    length: u64,
}

impl<'a, K: Key + 'static, V: Value + 'static> BtreeBulkBuilder<'a, K, V> {
    pub(crate) fn new(tree: BtreeMut<'a, K, V>, target_page_size: usize) -> Self {
        debug_assert!(tree.get_root().is_none());
        let target_page_size = target_page_size
            .max(tree.mem.get_page_size())
            .min(MAX_PAIR_LENGTH)
            .min(MAX_ENCODED_PAGE_BYTES);
        Self {
            tree,
            leaf: Vec::new(),
            leaf_key_bytes: 0,
            leaf_value_bytes: 0,
            levels: Vec::new(),
            target_page_size,
            length: 0,
        }
    }

    pub(crate) fn push(&mut self, key: Vec<u8>, value: Vec<u8>) -> Result {
        let projected_pairs = self.leaf.len() + 1;
        let projected_payload = self
            .leaf_key_bytes
            .checked_add(key.len())
            .and_then(|bytes| bytes.checked_add(self.leaf_value_bytes))
            .and_then(|bytes| bytes.checked_add(value.len()));
        let projected = projected_payload.and_then(|payload| {
            leaf_required_bytes(projected_pairs, payload, K::fixed_width(), V::fixed_width())
        });
        let single_payload = key
            .len()
            .checked_add(value.len())
            .ok_or(crate::StorageError::ValueTooLarge(usize::MAX))?;
        let single_required =
            leaf_required_bytes(1, single_payload, K::fixed_width(), V::fixed_width())
                .ok_or(crate::StorageError::ValueTooLarge(usize::MAX))?;
        if single_required > MAX_ENCODED_PAGE_BYTES {
            return Err(crate::StorageError::ValueTooLarge(single_required));
        }
        if !self.leaf.is_empty()
            && (projected.is_none_or(|bytes| bytes > self.target_page_size)
                || projected_pairs > u16::MAX as usize)
        {
            self.close_leaf()?;
        }

        self.leaf_key_bytes = self.leaf_key_bytes.checked_add(key.len()).ok_or_else(|| {
            crate::StorageError::Corrupted("sorted leaf key length overflow".into())
        })?;
        self.leaf_value_bytes =
            self.leaf_value_bytes
                .checked_add(value.len())
                .ok_or_else(|| {
                    crate::StorageError::Corrupted("sorted leaf value length overflow".into())
                })?;
        self.leaf.push((key, value));
        self.length = self
            .length
            .checked_add(1)
            .ok_or_else(|| crate::StorageError::Corrupted("sorted table length overflow".into()))?;
        Ok(())
    }

    pub(crate) fn finish(mut self) -> Result<(BtreeMut<'a, K, V>, u64)> {
        if !self.leaf.is_empty() {
            self.close_leaf()?;
        }
        if self.length == 0 {
            return Ok((self.tree, 0));
        }

        let root = self.finish_levels()?;
        self.tree
            .set_root(Some(BtreeHeader::new(root.page, DEFERRED, self.length)));
        Ok((self.tree, self.length))
    }

    fn close_leaf(&mut self) -> Result {
        let records = std::mem::take(&mut self.leaf);
        let max_key = records.last().unwrap().0.clone();
        let mut builder = LeafBuilder::new(
            &self.tree.mem,
            &self.tree.allocated_pages,
            records.len(),
            K::fixed_width(),
            V::fixed_width(),
        );
        for (key, value) in &records {
            builder.push(key, value);
        }
        let page = builder.build()?;
        let child = Child {
            page: page.get_page_number(),
            max_key,
        };
        drop(page);
        self.leaf_key_bytes = 0;
        self.leaf_value_bytes = 0;
        self.push_child(0, child)
    }

    fn push_child(&mut self, level: usize, child: Child) -> Result {
        while self.levels.len() <= level {
            self.levels.push(BranchLevel::default());
        }

        let current = &self.levels[level].children;
        // With the incoming child, every current child's maximum key becomes
        // a separator; the incoming final child contributes no separator.
        let projected_key_bytes = current.iter().try_fold(0usize, |total, child| {
            total.checked_add(child.max_key.len())
        });
        let projected = projected_key_bytes.and_then(|key_bytes| {
            branch_required_bytes(current.len(), key_bytes, K::fixed_width())
        });
        let must_emit = projected.is_none_or(|bytes| bytes > self.target_page_size)
            || current.len() > u16::MAX as usize;
        if current.len() >= 3 && must_emit {
            // Retain the final child and pair it with the incoming child. This
            // prevents a trailing one-child non-root branch at finish.
            let carry = self.levels[level].children.pop().unwrap();
            let full = std::mem::take(&mut self.levels[level].children);
            self.levels[level].children.push(carry);
            self.levels[level].children.push(child);
            self.levels[level].emitted = true;
            let parent = self.build_branch(full)?;
            self.push_child(level + 1, parent)?;
            return Ok(());
        }
        self.levels[level].children.push(child);
        Ok(())
    }

    fn build_branch(&self, children: Vec<Child>) -> Result<Child> {
        let max_key = children.last().unwrap().max_key.clone();
        let separator_bytes = children
            .iter()
            .take(children.len().saturating_sub(1))
            .try_fold(0usize, |total, child| {
                total.checked_add(child.max_key.len())
            })
            .ok_or(crate::StorageError::ValueTooLarge(usize::MAX))?;
        let required_bytes = branch_required_bytes(
            children.len().saturating_sub(1),
            separator_bytes,
            K::fixed_width(),
        )
        .ok_or(crate::StorageError::ValueTooLarge(usize::MAX))?;
        // Variable-width offsets and the largest allocator region are both
        // bounded to a 32-bit byte domain. A valid individual key can still
        // make a multi-key branch exceed that domain, so reject it fallibly.
        if required_bytes > MAX_ENCODED_PAGE_BYTES {
            return Err(crate::StorageError::ValueTooLarge(required_bytes));
        }
        let mut builder = BranchBuilder::new(
            &self.tree.mem,
            &self.tree.allocated_pages,
            children.len(),
            K::fixed_width(),
        );
        for child in &children {
            builder.push_child(child.page, DEFERRED);
        }
        for child in children.iter().take(children.len().saturating_sub(1)) {
            builder.push_key(&child.max_key);
        }
        let page = builder.build()?;
        let result = Child {
            page: page.get_page_number(),
            max_key,
        };
        drop(page);
        Ok(result)
    }

    fn finish_levels(&mut self) -> Result<Child> {
        let mut level = 0;
        loop {
            let only_child_is_root =
                !self.levels[level].emitted && self.levels[level].children.len() == 1;
            if only_child_is_root {
                return Ok(self.levels[level].children.pop().unwrap());
            }

            let partial = std::mem::take(&mut self.levels[level].children);
            let parent = self.build_branch(partial)?;
            self.levels[level].emitted = true;
            self.push_child(level + 1, parent)?;
            level += 1;
        }
    }
}
