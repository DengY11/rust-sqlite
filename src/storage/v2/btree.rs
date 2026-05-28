use serde::{Deserialize, Serialize};

use crate::common::error::{DbError, Result};

use super::page::{PageId, PageKind, decode_payload, encode_payload_page};
use super::pager::Pager;

const LEAF_MAX_ENTRIES: usize = 16;
const INTERNAL_MAX_KEYS: usize = 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BTree {
    root_page_id: PageId,
}

#[derive(Debug, Clone)]
struct SplitResult {
    separator_key: u64,
    right_page_id: PageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeafEntry {
    key: u64,
    value: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LeafNode {
    entries: Vec<LeafEntry>,
    next: Option<PageId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InternalNode {
    separators: Vec<u64>,
    children: Vec<PageId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum Node {
    Leaf(LeafNode),
    Internal(InternalNode),
}

impl LeafNode {
    fn empty() -> Self {
        Self {
            entries: Vec::new(),
            next: None,
        }
    }

    fn lookup(&self, key: u64) -> Option<&Vec<u8>> {
        self.entries
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| &entry.value)
    }
}

impl InternalNode {
    fn child_index(&self, key: u64) -> usize {
        self.separators.partition_point(|separator| key >= *separator)
    }

    fn from_split(left_child: PageId, separator_key: u64, right_child: PageId) -> Self {
        Self {
            separators: vec![separator_key],
            children: vec![left_child, right_child],
        }
    }
}

impl BTree {
    pub fn create(pager: &mut Pager, txn_id: u64) -> Result<Self> {
        let root_page_id = pager.allocate_leaf_page(txn_id)?;
        write_node(pager, txn_id, root_page_id, &Node::Leaf(LeafNode::empty()))?;
        Ok(Self { root_page_id })
    }

    #[must_use]
    pub fn from_root(root_page_id: PageId) -> Self {
        Self { root_page_id }
    }

    #[must_use]
    pub fn root_page_id(&self) -> PageId {
        self.root_page_id
    }

    pub fn get(&self, pager: &Pager, key: u64) -> Result<Option<Vec<u8>>> {
        let leaf = self.find_leaf(pager, self.root_page_id, key)?;
        Ok(leaf.lookup(key).cloned())
    }

    pub fn insert(&mut self, pager: &mut Pager, txn_id: u64, key: u64, value: &[u8]) -> Result<()> {
        if let Some(split) =
            self.insert_into_page(pager, txn_id, self.root_page_id, key, value.to_vec())?
        {
            let new_root = pager.allocate_internal_page(txn_id)?;
            let root = InternalNode::from_split(
                self.root_page_id,
                split.separator_key,
                split.right_page_id,
            );
            write_node(pager, txn_id, new_root, &Node::Internal(root))?;
            self.root_page_id = new_root;
        }
        Ok(())
    }

    pub fn scan_all(&self, pager: &Pager) -> Result<Vec<(u64, Vec<u8>)>> {
        let mut current = Some(self.leftmost_leaf_page(pager, self.root_page_id)?);
        let mut rows = Vec::new();

        while let Some(page_id) = current {
            let leaf = match read_node(pager, page_id)? {
                Node::Leaf(leaf) => leaf,
                Node::Internal(_) => {
                    return Err(DbError::storage(format!(
                        "expected leaf page {} while scanning B+Tree leaf chain",
                        page_id.0
                    )));
                }
            };
            rows.extend(
                leaf.entries
                    .into_iter()
                    .map(|entry| (entry.key, entry.value)),
            );
            current = leaf.next;
        }

        Ok(rows)
    }

    pub fn delete(&mut self, pager: &mut Pager, txn_id: u64, key: u64) -> Result<()> {
        self.delete_from_page(pager, txn_id, self.root_page_id, key)
    }

    fn find_leaf(&self, pager: &Pager, page_id: PageId, key: u64) -> Result<LeafNode> {
        match read_node(pager, page_id)? {
            Node::Leaf(leaf) => Ok(leaf),
            Node::Internal(internal) => {
                let child = internal.children[internal.child_index(key)];
                self.find_leaf(pager, child, key)
            }
        }
    }

    fn leftmost_leaf_page(&self, pager: &Pager, page_id: PageId) -> Result<PageId> {
        match read_node(pager, page_id)? {
            Node::Leaf(_) => Ok(page_id),
            Node::Internal(internal) => self.leftmost_leaf_page(pager, internal.children[0]),
        }
    }

    fn insert_into_page(
        &mut self,
        pager: &mut Pager,
        txn_id: u64,
        page_id: PageId,
        key: u64,
        value: Vec<u8>,
    ) -> Result<Option<SplitResult>> {
        match read_node(pager, page_id)? {
            Node::Leaf(mut leaf) => {
                match leaf.entries.binary_search_by_key(&key, |entry| entry.key) {
                    Ok(index) => leaf.entries[index].value = value,
                    Err(index) => leaf.entries.insert(index, LeafEntry { key, value }),
                }

                if leaf.entries.len() <= LEAF_MAX_ENTRIES {
                    write_node(pager, txn_id, page_id, &Node::Leaf(leaf))?;
                    return Ok(None);
                }

                let split_at = leaf.entries.len() / 2;
                let right_entries = leaf.entries.split_off(split_at);
                let separator_key = right_entries[0].key;
                let new_page_id = pager.allocate_leaf_page(txn_id)?;
                let old_next = leaf.next.take();
                leaf.next = Some(new_page_id);

                write_node(pager, txn_id, page_id, &Node::Leaf(leaf))?;
                write_node(
                    pager,
                    txn_id,
                    new_page_id,
                    &Node::Leaf(LeafNode {
                        entries: right_entries,
                        next: old_next,
                    }),
                )?;

                Ok(Some(SplitResult {
                    separator_key,
                    right_page_id: new_page_id,
                }))
            }
            Node::Internal(mut internal) => {
                let child_index = internal.child_index(key);
                let child_page_id = internal.children[child_index];
                let child_split =
                    self.insert_into_page(pager, txn_id, child_page_id, key, value)?;

                let Some(split) = child_split else {
                    return Ok(None);
                };

                internal.separators.insert(child_index, split.separator_key);
                internal.children.insert(child_index + 1, split.right_page_id);

                if internal.separators.len() <= INTERNAL_MAX_KEYS {
                    write_node(pager, txn_id, page_id, &Node::Internal(internal))?;
                    return Ok(None);
                }

                let mid = internal.separators.len() / 2;
                let promoted_key = internal.separators[mid];
                let right_separators = internal.separators.split_off(mid + 1);
                internal.separators.pop();
                let right_children = internal.children.split_off(mid + 1);

                let new_page_id = pager.allocate_internal_page(txn_id)?;
                write_node(pager, txn_id, page_id, &Node::Internal(internal))?;
                write_node(
                    pager,
                    txn_id,
                    new_page_id,
                    &Node::Internal(InternalNode {
                        separators: right_separators,
                        children: right_children,
                    }),
                )?;

                Ok(Some(SplitResult {
                    separator_key: promoted_key,
                    right_page_id: new_page_id,
                }))
            }
        }
    }

    fn delete_from_page(
        &mut self,
        pager: &mut Pager,
        txn_id: u64,
        page_id: PageId,
        key: u64,
    ) -> Result<()> {
        match read_node(pager, page_id)? {
            Node::Leaf(mut leaf) => {
                if let Ok(index) = leaf.entries.binary_search_by_key(&key, |entry| entry.key) {
                    leaf.entries.remove(index);
                    write_node(pager, txn_id, page_id, &Node::Leaf(leaf))?;
                }
                Ok(())
            }
            Node::Internal(internal) => {
                let child_page_id = internal.children[internal.child_index(key)];
                self.delete_from_page(pager, txn_id, child_page_id, key)
            }
        }
    }
}

fn read_node(pager: &Pager, page_id: PageId) -> Result<Node> {
    let page = pager.read_page(page_id)?;
    match super::page::page_kind(&page)? {
        PageKind::Leaf => Ok(serde_json::from_slice(&decode_payload(&page, PageKind::Leaf)?)?),
        PageKind::Internal => Ok(serde_json::from_slice(&decode_payload(&page, PageKind::Internal)?)?),
        other => Err(DbError::storage(format!(
            "page {} has non-tree page kind {:?}",
            page_id.0, other
        ))),
    }
}

fn write_node(pager: &mut Pager, txn_id: u64, page_id: PageId, node: &Node) -> Result<()> {
    let (kind, payload) = match node {
        Node::Leaf(_) => (PageKind::Leaf, serde_json::to_vec(node)?),
        Node::Internal(_) => (PageKind::Internal, serde_json::to_vec(node)?),
    };
    pager.write_page(txn_id, page_id, encode_payload_page(kind, &payload)?)
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn inserts_and_reads_values_from_a_single_leaf() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();
        let mut tree = BTree::create(&mut pager, txn).unwrap();
        tree.insert(&mut pager, txn, 1, b"alice").unwrap();
        assert_eq!(tree.get(&pager, 1).unwrap(), Some(b"alice".to_vec()));
    }

    #[test]
    fn splits_leaf_pages_and_preserves_sorted_scan_order() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();
        let mut tree = BTree::create(&mut pager, txn).unwrap();

        for key in 1..=64 {
            tree.insert(&mut pager, txn, key, format!("row-{key}").as_bytes())
                .unwrap();
        }

        let keys: Vec<u64> = tree
            .scan_all(&pager)
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(keys, (1..=64).collect::<Vec<_>>());
    }

    #[test]
    fn delete_removes_visible_key_without_rebalancing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("db.rsql");
        let mut pager = Pager::open(&path).unwrap();
        let txn = pager.begin().unwrap();
        let mut tree = BTree::create(&mut pager, txn).unwrap();
        tree.insert(&mut pager, txn, 7, b"carol").unwrap();
        tree.delete(&mut pager, txn, 7).unwrap();
        assert_eq!(tree.get(&pager, 7).unwrap(), None);
    }
}
