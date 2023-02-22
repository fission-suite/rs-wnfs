use super::{
    encrypted::Encrypted, hamt::Hasher, link::PrivateLink, namefilter::Namefilter, AesKey,
    PrivateDirectory, PrivateDirectoryContent, PrivateFile, PrivateFileContent, PrivateForest,
    PrivateRef, RevisionRef, NONCE_SIZE,
};
use crate::{
    dagcbor, utils, AesError, BlockStore, FsError, HashOutput, Id, NodeType, HASH_BYTE_SIZE,
};
use aes_gcm::{aead::Aead, Aes256Gcm, KeyInit, Nonce};
use aes_kw::KekAes256;
use anyhow::{bail, Result};
use async_recursion::async_recursion;
use chrono::{DateTime, Utc};
use futures::StreamExt;
use libipld::{Cid, Ipld, IpldCodec};
use rand_core::RngCore;
use serde::{Deserialize, Serialize};
use sha3::Sha3_256;
use skip_ratchet::{seek::JumpSize, Ratchet, RatchetSeeker};
use std::{cmp::Ordering, collections::BTreeSet, fmt::Debug, rc::Rc};

//--------------------------------------------------------------------------------------------------
// Type Definitions
//--------------------------------------------------------------------------------------------------

pub type INumber = HashOutput;

/// Represents a node in the WNFS private file system. This can either be a file or a directory.
///
/// # Examples
///
/// ```
/// use wnfs::{PrivateDirectory, PrivateNode, Namefilter};
/// use chrono::Utc;
/// use std::rc::Rc;
/// use rand::thread_rng;
///
/// let rng = &mut thread_rng();
/// let dir = Rc::new(PrivateDirectory::new(
///     Namefilter::default(),
///     Utc::now(),
///     rng,
/// ));
///
/// let node = PrivateNode::Dir(dir);
///
/// println!("Node: {:?}", node);
/// ```
#[derive(Debug, Clone, PartialEq)]
pub enum PrivateNode {
    File(Rc<PrivateFile>),
    Dir(Rc<PrivateDirectory>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum PrivateNodeContent {
    File(PrivateFileContent),
    Dir(PrivateDirectoryContent),
}

/// The key used to encrypt the content of a node.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct SnapshotKey(pub AesKey);

/// The key used to encrypt the header section of a node.
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct TemporalKey(pub AesKey);

/// This is the header of a private node. It contains secret information about the node which includes
/// the inumber, the ratchet, and the namefilter.
///
/// # Examples
///
/// ```
/// use wnfs::{PrivateFile, Namefilter, Id};
/// use chrono::Utc;
/// use rand::thread_rng;
///
/// let rng = &mut thread_rng();
/// let file = PrivateFile::new(
///     Namefilter::default(),
///     Utc::now(),
///     rng,
/// );
///
/// println!("Header: {:?}", file.header);
/// ```
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateNodeHeader {
    /// A unique identifier of the node.
    pub(crate) inumber: INumber,
    /// Used both for versioning and deriving keys for that enforces privacy.
    pub(crate) ratchet: Ratchet,
    /// Used for ancestry checks and as a key for the private forest.
    pub(crate) bare_name: Namefilter,
}

//--------------------------------------------------------------------------------------------------
// Implementations
//--------------------------------------------------------------------------------------------------

impl PrivateNode {
    /// Creates node with upserted modified time.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateDirectory, PrivateNode, Namefilter};
    /// use chrono::{Utc, Duration, TimeZone};
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let dir = Rc::new(PrivateDirectory::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::Dir(dir);
    ///
    /// let time = Utc::now() + Duration::days(1);
    /// let node = node.upsert_mtime(time);
    ///
    /// let imprecise_time = Utc.timestamp_opt(time.timestamp(), 0).single();
    /// assert_eq!(
    ///     imprecise_time,
    ///     node.as_dir()
    ///         .unwrap()
    ///         .get_metadata()
    ///         .get_modified()
    /// );
    /// ```
    pub fn upsert_mtime(&self, time: DateTime<Utc>) -> Self {
        match self {
            Self::File(file) => {
                let mut file = (**file).clone();
                file.content.metadata.upsert_mtime(time);
                Self::File(Rc::new(file))
            }
            Self::Dir(dir) => {
                let mut dir = (**dir).clone();
                dir.content.metadata.upsert_mtime(time);
                Self::Dir(Rc::new(dir))
            }
        }
    }

    /// Updates bare name ancestry of private sub tree.
    #[async_recursion(?Send)]
    pub(crate) async fn update_ancestry(
        &mut self,
        parent_bare_name: Namefilter,
        forest: &mut Rc<PrivateForest>,
        store: &mut impl BlockStore,
        rng: &mut impl RngCore,
    ) -> Result<()> {
        match self {
            Self::File(file_rc) => {
                let file = Rc::make_mut(file_rc);

                file.prepare_key_rotation(parent_bare_name, forest, store, rng)
                    .await?;
            }
            PrivateNode::Dir(dir_rc) => {
                let dir = Rc::make_mut(dir_rc);

                for private_link in &mut dir.content.entries.values_mut() {
                    let mut node = private_link.resolve_node(forest, store).await?.clone();
                    node.update_ancestry(dir.header.bare_name.clone(), forest, store, rng)
                        .await?;
                    *private_link = PrivateLink::new(node);
                }

                dir.prepare_key_rotation(parent_bare_name, rng);
            }
        }
        Ok(())
    }

    /// Gets the header of the node.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateDirectory, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let dir = Rc::new(PrivateDirectory::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::Dir(Rc::clone(&dir));
    ///
    /// assert_eq!(&dir.header, node.get_header());
    /// ```
    #[inline]
    pub fn get_header(&self) -> &PrivateNodeHeader {
        match self {
            Self::File(file) => &file.header,
            Self::Dir(dir) => &dir.header,
        }
    }

    /// Gets the previous links of the node.
    ///
    /// The previous links are encrypted with the previous revision's
    /// temporal key, so you need to know an 'older' revision of the
    /// skip ratchet to decrypt these.
    ///
    /// The previous links is exactly one Cid in most cases and refers
    /// to the ciphertext Cid from the previous revision that this
    /// node is an update of.
    ///
    /// If this node is a merge-node, it has two or more previous Cids.
    /// A single previous Cid must be from the previous revision, but all
    /// other Cids may appear in even older revisions.
    ///
    /// The previous links is `None`, it doesn't have previous Cids.
    /// The node is malformed if the previous links are `Some`, but
    /// the `BTreeSet` inside is empty.
    pub fn get_previous(&self) -> &BTreeSet<(usize, Encrypted<Cid>)> {
        match self {
            Self::File(file) => &file.content.previous,
            Self::Dir(dir) => &dir.content.previous,
        }
    }

    /// Casts a node to a directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateDirectory, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let dir = Rc::new(PrivateDirectory::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::Dir(Rc::clone(&dir));
    ///
    /// assert_eq!(node.as_dir().unwrap(), dir);
    /// ```
    pub fn as_dir(&self) -> Result<Rc<PrivateDirectory>> {
        Ok(match self {
            Self::Dir(dir) => Rc::clone(dir),
            _ => bail!(FsError::NotADirectory),
        })
    }

    /// Casts a node to a file.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateFile, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let file = Rc::new(PrivateFile::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::File(Rc::clone(&file));
    ///
    /// assert_eq!(node.as_file().unwrap(), file);
    /// ```
    pub fn as_file(&self) -> Result<Rc<PrivateFile>> {
        Ok(match self {
            Self::File(file) => Rc::clone(file),
            _ => bail!(FsError::NotAFile),
        })
    }

    /// Returns true if underlying node is a directory.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateDirectory, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let dir = Rc::new(PrivateDirectory::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::Dir(dir);
    ///
    /// assert!(node.is_dir());
    /// ```
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Dir(_))
    }

    /// Returns true if the underlying node is a file.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::{PrivateFile, PrivateNode, Namefilter};
    /// use chrono::Utc;
    /// use std::rc::Rc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let file = Rc::new(PrivateFile::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let node = PrivateNode::File(file);
    ///
    /// assert!(node.is_file());
    /// ```
    pub fn is_file(&self) -> bool {
        matches!(self, Self::File(_))
    }

    /// Gets the latest version of the node using exponential search.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    /// use chrono::Utc;
    /// use rand::thread_rng;
    /// use wnfs::{
    ///     private::{PrivateForest, PrivateRef, PrivateNode},
    ///     BlockStore, MemoryBlockStore, Namefilter, PrivateDirectory, PrivateOpResult,
    /// };
    ///
    /// #[async_std::main]
    /// async fn main() {
    ///     let store = &mut MemoryBlockStore::default();
    ///     let rng = &mut thread_rng();
    ///     let forest = &mut Rc::new(PrivateForest::new());
    ///
    ///     let PrivateOpResult { root_dir: init_dir, .. } = PrivateDirectory::new_and_store(
    ///         Default::default(),
    ///         Utc::now(),
    ///         forest,
    ///         store,
    ///         rng
    ///     ).await.unwrap();
    ///
    ///     let PrivateOpResult { root_dir, .. } = Rc::clone(&init_dir)
    ///         .mkdir(&["pictures".into(), "cats".into()], true, Utc::now(), forest, store, rng)
    ///         .await
    ///         .unwrap();
    ///
    ///     root_dir.store(forest, store, rng).await.unwrap();
    ///
    ///     let latest_node = PrivateNode::Dir(init_dir).search_latest(forest, store).await.unwrap();
    ///
    ///     let found_node = latest_node
    ///         .as_dir()
    ///         .unwrap()
    ///         .lookup_node("pictures", true, forest, store)
    ///         .await
    ///         .unwrap();
    ///
    ///     assert!(found_node.is_some());
    /// }
    /// ```
    pub async fn search_latest(
        &self,
        forest: &PrivateForest,
        store: &impl BlockStore,
    ) -> Result<PrivateNode> {
        self.search_latest_nodes(forest, store)
            .await?
            .into_iter()
            .next()
            // We expect the latest revision to have found valid nodes.
            // otherwise it's a revision that's filled with other stuff
            // than PrivateNodes, which should be an error.
            .ok_or(FsError::NotFound.into())
    }

    /// Seek ahead to the latest revision in this node's history.
    ///
    /// The result are all nodes from the latest revision, each one
    /// representing an instance of a concurrent write.
    pub async fn search_latest_nodes(
        &self,
        forest: &PrivateForest,
        store: &impl BlockStore,
    ) -> Result<Vec<PrivateNode>> {
        let header = self.get_header();

        let current_name = &header.get_saturated_name_hash();
        if !forest.has(current_name, store).await? {
            return Ok(vec![self.clone()]);
        }

        // Start an exponential search, starting with a small jump.
        // In many cases, we'll be at the latest revision already, so we only
        // do a single lookup to the next version, most likely realize it's not
        // there and thus stop seeking.
        let mut search = RatchetSeeker::new(header.ratchet.clone(), JumpSize::Small);
        let mut current_header = header.clone();

        loop {
            let current = search.current();
            current_header.ratchet = current.clone();

            let has_curr = forest
                .has(&current_header.get_saturated_name_hash(), store)
                .await?;

            let ord = if has_curr {
                Ordering::Less
            } else {
                Ordering::Greater
            };

            if !search.step(ord) {
                break;
            }
        }

        current_header.ratchet = search.current().clone();

        Ok(forest
            .get_multivalue(&current_header.derive_revision_ref(), store)
            .collect::<Vec<Result<PrivateNode>>>()
            .await
            .into_iter()
            .filter_map(|result| result.ok()) // Should we filter out errors?
            .collect())
    }

    pub(crate) async fn load(
        private_ref: &PrivateRef,
        store: &impl BlockStore,
    ) -> Result<PrivateNode> {
        let encrypted_bytes = store.get_block(&private_ref.content_cid).await?;
        let snapshot_key = private_ref.temporal_key.derive_snapshot_key();
        let bytes = snapshot_key.decrypt(&encrypted_bytes)?;
        let ipld = dagcbor::decode(&bytes)?;

        match ipld {
            Ipld::Map(map) => {
                let r#type: NodeType = map
                    .get("type")
                    .ok_or(FsError::MissingNodeType)?
                    .try_into()?;

                Ok(match r#type {
                    NodeType::PrivateFile => {
                        let (content, header_cid) = PrivateFileContent::deserialize(
                            Ipld::Map(map),
                            private_ref.content_cid,
                        )?;
                        let header =
                            PrivateNodeHeader::load(&header_cid, &private_ref.temporal_key, store)
                                .await?;
                        PrivateNode::File(Rc::new(PrivateFile { header, content }))
                    }
                    NodeType::PrivateDirectory => {
                        let (content, header_cid) = PrivateDirectoryContent::deserialize(
                            Ipld::Map(map),
                            &private_ref.temporal_key,
                            private_ref.content_cid,
                        )?;
                        let header =
                            PrivateNodeHeader::load(&header_cid, &private_ref.temporal_key, store)
                                .await?;
                        PrivateNode::Dir(Rc::new(PrivateDirectory { header, content }))
                    }
                    other => bail!(FsError::UnexpectedNodeType(other)),
                })
            }
            other => bail!("Expected `Ipld::Map` got {:#?}", other),
        }
    }

    pub async fn store(
        &self,
        forest: &mut Rc<PrivateForest>,
        store: &mut impl BlockStore,
        rng: &mut impl RngCore,
    ) -> Result<PrivateRef> {
        match self {
            Self::File(file) => file.store(forest, store, rng).await,
            Self::Dir(dir) => dir.store(forest, store, rng).await,
        }
    }

    /// Returns the private ref, if this node has been `.store()`ed before.
    pub(crate) fn get_private_ref(&self) -> Option<PrivateRef> {
        match self {
            Self::File(file) => file.get_private_ref(),
            Self::Dir(dir) => dir.get_private_ref(),
        }
    }
}

impl Id for PrivateNode {
    fn get_id(&self) -> String {
        match self {
            Self::File(file) => file.get_id(),
            Self::Dir(dir) => dir.get_id(),
        }
    }
}

impl From<PrivateFile> for PrivateNode {
    fn from(file: PrivateFile) -> Self {
        Self::File(Rc::new(file))
    }
}

impl From<PrivateDirectory> for PrivateNode {
    fn from(dir: PrivateDirectory) -> Self {
        Self::Dir(Rc::new(dir))
    }
}

impl PrivateNodeHeader {
    /// Creates a new PrivateNodeHeader.
    pub(crate) fn new(parent_bare_name: Namefilter, rng: &mut impl RngCore) -> Self {
        let inumber = crate::utils::get_random_bytes::<HASH_BYTE_SIZE>(rng);
        let ratchet_seed = crate::utils::get_random_bytes::<HASH_BYTE_SIZE>(rng);

        Self {
            bare_name: {
                let mut namefilter = parent_bare_name;
                namefilter.add(&inumber);
                namefilter
            },
            ratchet: Ratchet::zero(ratchet_seed),
            inumber,
        }
    }

    /// Creates a new PrivateNodeHeader with provided seed.
    pub(crate) fn with_seed(
        parent_bare_name: Namefilter,
        ratchet_seed: HashOutput,
        inumber: HashOutput,
    ) -> Self {
        Self {
            bare_name: {
                let mut namefilter = parent_bare_name;
                namefilter.add(&inumber);
                namefilter
            },
            ratchet: Ratchet::zero(ratchet_seed),
            inumber,
        }
    }

    /// Advances the ratchet.
    pub(crate) fn advance_ratchet(&mut self) {
        self.ratchet.inc();
    }

    /// Updates the bare name of the node.
    pub(crate) fn update_bare_name(&mut self, parent_bare_name: Namefilter) {
        self.bare_name = {
            let mut namefilter = parent_bare_name;
            namefilter.add(&self.inumber);
            namefilter
        };
    }

    /// Resets the ratchet.
    pub(crate) fn reset_ratchet(&mut self, rng: &mut impl RngCore) {
        self.ratchet = Ratchet::zero(utils::get_random_bytes(rng))
    }

    /// Derives the revision ref of the current header.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    /// use wnfs::{PrivateFile, Namefilter, Id};
    /// use chrono::Utc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let file = Rc::new(PrivateFile::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let revision_ref = file.header.derive_revision_ref();
    ///
    /// println!("Private ref: {:?}", revision_ref);
    /// ```
    pub fn derive_revision_ref(&self) -> RevisionRef {
        let temporal_key = self.derive_temporal_key();
        let saturated_name_hash = self.get_saturated_name_hash();

        RevisionRef {
            saturated_name_hash,
            temporal_key,
        }
    }

    /// Returns the label used for identifying the revision in the PrivateForest.
    pub fn get_saturated_name_hash(&self) -> HashOutput {
        Sha3_256::hash(&self.get_saturated_name())
    }

    /// Derives the temporal key.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    /// use wnfs::{PrivateFile, Namefilter, Id};
    /// use chrono::Utc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let file = Rc::new(PrivateFile::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let temporal_key = file.header.derive_temporal_key();
    ///
    /// println!("Temporal Key: {:?}", temporal_key);
    /// ```
    #[inline]
    pub fn derive_temporal_key(&self) -> TemporalKey {
        AesKey::new(self.ratchet.derive_key()).into()
    }

    /// Derives the snapshot key.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    /// use wnfs::{PrivateFile, Namefilter, Id};
    /// use chrono::Utc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let file = Rc::new(PrivateFile::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let snapshot_key = file.header.derive_snapshot_key();
    ///
    /// println!("Snapshot Key: {:?}", snapshot_key);
    /// ```
    #[inline]
    pub fn derive_snapshot_key(&self) -> SnapshotKey {
        AesKey::new(Sha3_256::hash(&self.ratchet.derive_key())).into()
    }

    /// Gets the saturated namefilter for this node using the provided ratchet key.
    pub(crate) fn get_saturated_name_with_key(&self, temporal_key: &TemporalKey) -> Namefilter {
        let mut name = self.bare_name.clone();
        name.add(&temporal_key.0.as_bytes());
        name.saturate();
        name
    }

    /// Gets the saturated namefilter for this node.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    /// use wnfs::{PrivateFile, Namefilter, private::AesKey};
    /// use chrono::Utc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let file = Rc::new(PrivateFile::new(
    ///     Namefilter::default(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let saturated_name = file.header.get_saturated_name();
    ///
    /// println!("Saturated name: {:?}", saturated_name);
    /// ```
    #[inline]
    pub fn get_saturated_name(&self) -> Namefilter {
        let temporal_key = self.derive_temporal_key();
        self.get_saturated_name_with_key(&temporal_key)
    }

    /// Encrypts this private node header in an block, then stores that in the given
    /// BlockStore and returns its CID.
    pub async fn store(&self, store: &mut impl BlockStore) -> Result<Cid> {
        let temporal_key = self.derive_temporal_key();
        let cbor_bytes = dagcbor::encode(self)?;
        let ciphertext = temporal_key.key_wrap_encrypt(&cbor_bytes)?;
        store.put_block(ciphertext, IpldCodec::Raw).await
    }

    /// Loads a private node header from a given CID linking to the ciphertext block
    /// to be decrypted with given key.
    pub(crate) async fn load(
        cid: &Cid,
        temporal_key: &TemporalKey,
        store: &impl BlockStore,
    ) -> Result<PrivateNodeHeader> {
        let ciphertext = store.get_block(cid).await?;
        let cbor_bytes = temporal_key.key_wrap_decrypt(&ciphertext)?;
        dagcbor::decode(&cbor_bytes)
    }
}

impl Debug for PrivateNodeHeader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut inumber_str = String::from("0x");
        for byte in self.inumber {
            inumber_str.push_str(&format!("{byte:02X}"));
        }

        f.debug_struct("PrivateRef")
            .field("inumber", &inumber_str)
            .field("ratchet", &self.ratchet)
            .field("bare_name", &self.bare_name)
            .finish()
    }
}

impl From<AesKey> for TemporalKey {
    fn from(key: AesKey) -> Self {
        Self(key)
    }
}

impl From<[u8; 32]> for TemporalKey {
    fn from(key: [u8; 32]) -> Self {
        Self(AesKey::new(key))
    }
}

impl From<&Ratchet> for TemporalKey {
    fn from(ratchet: &Ratchet) -> Self {
        Self::from(AesKey::new(ratchet.derive_key()))
    }
}

impl From<AesKey> for SnapshotKey {
    fn from(key: AesKey) -> Self {
        Self(key)
    }
}

impl From<SnapshotKey> for AesKey {
    fn from(key: SnapshotKey) -> Self {
        key.0
    }
}

impl TemporalKey {
    /// Turn this TemporalKey, which gives read access to the current revision and any future
    /// revisions into a SnapshotKey, which only gives read access to the current revision.
    pub fn derive_snapshot_key(&self) -> SnapshotKey {
        let TemporalKey(key) = self;
        SnapshotKey(AesKey::new(Sha3_256::hash(&key.as_bytes())))
    }

    /// Encrypt a cleartext with this temporal key.
    ///
    /// Uses authenticated deterministic encryption via AES key wrap with padding (AES-KWP).
    ///
    /// The resulting ciphertext is 8 bytes longer than the next multiple of 8 bytes of the
    /// cleartext input length.
    pub fn key_wrap_encrypt(&self, cleartext: &[u8]) -> Result<Vec<u8>> {
        Ok(KekAes256::from(self.0.clone().bytes())
            .wrap_with_padding_vec(cleartext)
            .map_err(|e| AesError::UnableToEncrypt(format!("{e}")))?)
    }

    /// Decrypt a ciphertext that was encrypted with this temporal key.
    ///
    /// Uses authenticated deterministic encryption via AES key wrap with padding (AES-KWP).
    ///
    /// The input ciphertext is 8 bytes longer than the next multiple of 8 bytes of the
    /// resulting cleartext length.
    pub fn key_wrap_decrypt(&self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        Ok(KekAes256::from(self.0.clone().bytes())
            .unwrap_with_padding_vec(ciphertext)
            .map_err(|e| AesError::UnableToEncrypt(format!("{e}")))?)
    }
}

impl SnapshotKey {
    /// Encrypts the given plaintext using the key.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::private::{AesKey, SnapshotKey};
    /// use wnfs::utils;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let key = SnapshotKey(AesKey::new(utils::get_random_bytes(rng)));
    ///
    /// let plaintext = b"Hello World!";
    /// let ciphertext = key.encrypt(plaintext, rng).unwrap();
    /// let decrypted = key.decrypt(&ciphertext).unwrap();
    ///
    /// assert_eq!(plaintext, &decrypted[..]);
    /// ```
    pub fn encrypt(&self, data: &[u8], rng: &mut impl RngCore) -> Result<Vec<u8>> {
        let nonce_bytes = utils::get_random_bytes::<NONCE_SIZE>(rng);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let cipher_text = Aes256Gcm::new_from_slice(self.0.as_bytes())?
            .encrypt(nonce, data)
            .map_err(|e| AesError::UnableToEncrypt(format!("{e}")))?;

        Ok([nonce_bytes.to_vec(), cipher_text].concat())
    }

    /// Decrypts the given ciphertext using the key.
    ///
    /// # Examples
    ///
    /// ```
    /// use wnfs::private::{AesKey, SnapshotKey};
    /// use wnfs::utils;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let key = SnapshotKey(AesKey::new(utils::get_random_bytes(rng)));
    ///
    /// let plaintext = b"Hello World!";
    /// let ciphertext = key.encrypt(plaintext, rng).unwrap();
    /// let decrypted = key.decrypt(&ciphertext).unwrap();
    ///
    /// assert_eq!(plaintext, &decrypted[..]);
    /// ```
    pub fn decrypt(&self, cipher_text: &[u8]) -> Result<Vec<u8>> {
        let (nonce_bytes, data) = cipher_text.split_at(NONCE_SIZE);

        Ok(Aes256Gcm::new_from_slice(self.0.as_bytes())?
            .decrypt(Nonce::from_slice(nonce_bytes), data)
            .map_err(|e| AesError::UnableToDecrypt(format!("{e}")))?)
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use proptest::test_runner::{RngAlgorithm, TestRng};

    use crate::MemoryBlockStore;

    use super::*;

    #[async_std::test]
    async fn serialized_private_node_can_be_deserialized() {
        let rng = &mut TestRng::deterministic_rng(RngAlgorithm::ChaCha);
        let content = b"Lorem ipsum dolor sit amet";
        let forest = &mut Rc::new(PrivateForest::new());
        let store = &mut MemoryBlockStore::new();

        let file = PrivateFile::with_content(
            Namefilter::default(),
            Utc::now(),
            content.to_vec(),
            forest,
            store,
            rng,
        )
        .await
        .unwrap();

        let file = PrivateNode::File(Rc::new(file));
        let private_ref = file.store(forest, store, rng).await.unwrap();

        let deserialized_node = PrivateNode::load(&private_ref, store).await.unwrap();

        assert_eq!(file, deserialized_node);
    }
}

//--------------------------------------------------------------------------------------------------
// Proptests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod proptests {
    use crate::private::KEY_BYTE_SIZE;

    use super::*;
    use proptest::{
        prelude::any,
        prop_assert_eq,
        test_runner::{RngAlgorithm, TestRng},
    };
    use test_strategy::proptest;

    #[proptest(cases = 100)]
    fn snapshot_key_can_encrypt_and_decrypt_data(
        #[strategy(any::<Vec<u8>>())] data: Vec<u8>,
        #[strategy(any::<[u8; KEY_BYTE_SIZE]>())] rng_seed: [u8; KEY_BYTE_SIZE],
        key_bytes: [u8; KEY_BYTE_SIZE],
    ) {
        let key = SnapshotKey(AesKey::new(key_bytes));
        let rng = &mut TestRng::from_seed(RngAlgorithm::ChaCha, &rng_seed);

        let encrypted = key.encrypt(&data, rng).unwrap();
        let decrypted = key.decrypt(&encrypted).unwrap();

        prop_assert_eq!(decrypted, data);
    }
}
