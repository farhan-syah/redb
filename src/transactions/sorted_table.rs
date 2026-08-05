use super::{TableType, WriteTransaction};
use crate::tree_store::{BtreeBulkBuilder, BtreeMut, MAX_PAIR_LENGTH, MAX_VALUE_LENGTH};
use crate::types::{Key, Value};
use crate::{Error, TableDefinition, TableHandle};
use std::borrow::Borrow;

/// Packing policy for [`WriteTransaction::build_sorted_table_with_options`].
#[derive(Clone, Copy, Debug, Default)]
pub struct SortedTableOptions {
    pub(super) target_page_size: usize,
}

impl SortedTableOptions {
    /// Prefer pages of at least this encoded size. Redb rounds allocations to
    /// its normal page orders, and records larger than the target remain valid.
    #[must_use]
    pub fn with_target_page_size(mut self, bytes: usize) -> Self {
        self.target_page_size = bytes;
        self
    }
}

/// Typed one-shot writer used by [`WriteTransaction::build_sorted_table`].
pub struct SortedTableBuilder<K: Key + 'static, V: Value + 'static> {
    inner: BtreeBulkBuilder<'static, K, V>,
    last_key: Option<Vec<u8>>,
    failed: bool,
}

impl<K: Key + 'static, V: Value + 'static> SortedTableBuilder<K, V> {
    /// Append one key/value pair. Keys must be strictly increasing according
    /// to [`Key::compare`]. Values are serialized before any tree mutation.
    pub fn insert<'k, 'v>(
        &mut self,
        key: impl Borrow<K::SelfType<'k>>,
        value: impl Borrow<V::SelfType<'v>>,
    ) -> crate::Result<(), Error> {
        let key_bytes = K::as_bytes(key.borrow());
        let value_bytes = V::as_bytes(value.borrow());
        let key_bytes = key_bytes.as_ref();
        let value_bytes = value_bytes.as_ref();

        let Some(pair_len) = key_bytes.len().checked_add(value_bytes.len()) else {
            self.failed = true;
            return Err(Error::ValueTooLarge(usize::MAX));
        };
        if key_bytes.len() > MAX_VALUE_LENGTH {
            self.failed = true;
            return Err(Error::ValueTooLarge(key_bytes.len()));
        }
        if value_bytes.len() > MAX_VALUE_LENGTH || pair_len > MAX_PAIR_LENGTH {
            self.failed = true;
            return Err(Error::ValueTooLarge(pair_len));
        }
        if self
            .last_key
            .as_deref()
            .is_some_and(|last| K::compare(last, key_bytes) != std::cmp::Ordering::Less)
        {
            self.failed = true;
            return Err(Error::SortedTableKeyOrder);
        }

        let owned_key = key_bytes.to_vec();
        let owned_value = value_bytes.to_vec();
        if let Err(error) = self.inner.push(owned_key.clone(), owned_value) {
            self.failed = true;
            return Err(error.into());
        }
        self.last_key = Some(owned_key);
        Ok(())
    }
}

impl WriteTransaction {
    /// Build a normal table bottom-up from a strictly sorted stream.
    ///
    /// The transaction is consumed so any producer, ordering, allocation, or
    /// table error aborts all pages built by this transaction. The target table
    /// must be empty. On success the transaction is returned and may build
    /// another table or commit normally.
    #[track_caller]
    pub fn build_sorted_table<K, V, F>(
        self,
        definition: TableDefinition<K, V>,
        producer: F,
    ) -> crate::Result<Self, Error>
    where
        K: Key + 'static,
        V: Value + 'static,
        F: FnOnce(&mut SortedTableBuilder<K, V>) -> crate::Result<(), Error>,
    {
        self.build_sorted_table_with_options(definition, SortedTableOptions::default(), producer)
    }

    /// Build a normal empty table with an explicit bottom-up packing policy.
    #[track_caller]
    pub fn build_sorted_table_with_options<K, V, F>(
        self,
        definition: TableDefinition<K, V>,
        options: SortedTableOptions,
        producer: F,
    ) -> crate::Result<Self, Error>
    where
        K: Key + 'static,
        V: Value + 'static,
        F: FnOnce(&mut SortedTableBuilder<K, V>) -> crate::Result<(), Error>,
    {
        let name = definition.name().to_string();
        let (root, length, freed_pages, allocated_pages) = {
            let mut tables = self.tables.lock().unwrap();
            let (root, length) = tables.inner_open::<K, V>(&name, TableType::Normal)?;
            if root.is_some() || length != 0 {
                return Err(Error::SortedTableNotEmpty(name));
            }
            tables.set_dirty(&self);
            (
                root,
                length,
                tables.freed_pages.clone(),
                tables.allocated_pages.clone(),
            )
        };
        debug_assert!(root.is_none());
        debug_assert_eq!(length, 0);

        let tree = BtreeMut::new(
            None,
            self.transaction_guard.clone(),
            self.mem.clone(),
            freed_pages,
            allocated_pages,
        );
        let mut builder = SortedTableBuilder {
            inner: BtreeBulkBuilder::new(tree, options.target_page_size),
            last_key: None,
            failed: false,
        };
        producer(&mut builder)?;
        if builder.failed {
            return Err(Error::SortedTableBuilderFailed);
        }
        let (tree, length) = builder.inner.finish().map_err(Error::from)?;
        self.close_table(&name, &tree, length);
        drop(tree);
        Ok(self)
    }
}
