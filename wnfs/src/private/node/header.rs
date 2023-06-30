use super::{PrivateNodeHeaderSerializable, TemporalKey, REVISION_SEGMENT_DSS};
use crate::{error::FsError, private::RevisionRef};
use anyhow::{bail, Result};
use libipld_core::cid::Cid;
use rand_core::CryptoRngCore;
use sha3::{Digest, Sha3_256};
use skip_ratchet::Ratchet;
use std::fmt::Debug;
use wnfs_common::{utils, BlockStore, HashOutput, CODEC_RAW, HASH_BYTE_SIZE};
use wnfs_hamt::Hasher;
use wnfs_nameaccumulator::{AccumulatorSetup, Name, NameSegment};

//--------------------------------------------------------------------------------------------------
// Type Definitions
//--------------------------------------------------------------------------------------------------

/// This is the header of a private node. It contains secret information about the node which includes
/// the inumber, the ratchet, and the identifying private name.
///
/// # Examples
///
/// ```
/// use wnfs::private::PrivateFile;
/// use wnfs_nameaccumulator::{AccumulatorSetup, Name};
/// use chrono::Utc;
/// use rand::thread_rng;
///
/// let rng = &mut thread_rng();
/// let setup = &AccumulatorSetup::from_rsa_2048(rng);
/// let file = PrivateFile::new(
///     &Name::empty(setup),
///     Utc::now(),
///     rng,
/// );
///
/// println!("Header: {:#?}", file.header);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateNodeHeader {
    /// A unique identifier of the node.
    pub(crate) inumber: NameSegment,
    /// Used both for versioning and deriving keys for that enforces privacy.
    pub(crate) ratchet: Ratchet,
    /// Stores the name of this node for easier lookup.
    pub(crate) name: Name,
}

//--------------------------------------------------------------------------------------------------
// Implementations
//--------------------------------------------------------------------------------------------------

impl PrivateNodeHeader {
    /// Creates a new PrivateNodeHeader.
    pub(crate) fn new(parent_name: &Name, rng: &mut impl CryptoRngCore) -> Self {
        let inumber = NameSegment::new(rng);
        let ratchet_seed = utils::get_random_bytes::<HASH_BYTE_SIZE>(rng);
        Self::with_seed(parent_name, ratchet_seed, inumber)
    }

    /// Creates a new PrivateNodeHeader with provided seed.
    pub(crate) fn with_seed(
        parent_name: &Name,
        ratchet_seed: HashOutput,
        inumber: NameSegment,
    ) -> Self {
        // A keyed hash to use for determining the seed increments without
        // leaking info about the seed itself (from the ratchet state).
        let seed_hash = Sha3_256::new()
            .chain_update("WNFS ratchet increments")
            .chain_update(ratchet_seed)
            .finalize();
        Self {
            name: parent_name.with_segments_added(Some(inumber.clone())),
            ratchet: Ratchet::from_seed(&ratchet_seed, seed_hash[0], seed_hash[1]),
            inumber,
        }
    }

    /// Advances the ratchet.
    pub(crate) fn advance_ratchet(&mut self) {
        self.ratchet.inc();
    }

    /// Updates the name to the child of given parent name.
    pub(crate) fn update_name(&mut self, parent_name: &Name) {
        self.name = parent_name.clone();
        self.name.add_segments(Some(self.inumber.clone()));
    }

    /// Resets the ratchet.
    pub(crate) fn reset_ratchet(&mut self, rng: &mut impl CryptoRngCore) {
        self.ratchet = Ratchet::from_rng(rng)
    }

    /// Derives the revision ref of the current header.
    pub(crate) fn derive_revision_ref(&self, setup: &AccumulatorSetup) -> RevisionRef {
        let temporal_key = self.derive_temporal_key();
        let revision_name_hash = Sha3_256::hash(self.get_revision_name().as_accumulator(setup));

        RevisionRef {
            revision_name_hash,
            temporal_key,
        }
    }

    /// Derives the temporal key.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    /// use wnfs::private::PrivateFile;
    /// use wnfs_nameaccumulator::{AccumulatorSetup, Name};
    /// use chrono::Utc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let setup = &AccumulatorSetup::from_rsa_2048(rng);
    /// let file = Rc::new(PrivateFile::new(
    ///     &Name::empty(setup),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let temporal_key = file.header.derive_temporal_key();
    ///
    /// println!("Temporal Key: {:?}", temporal_key);
    /// ```
    #[inline]
    pub fn derive_temporal_key(&self) -> TemporalKey {
        TemporalKey::from(&self.ratchet)
    }

    pub(crate) fn derive_revision_segment(&self) -> NameSegment {
        let hasher = self.ratchet.derive_key(REVISION_SEGMENT_DSS);
        NameSegment::from_digest(hasher)
    }

    /// Gets the revision name for this node.
    ///
    /// It's this node's name with a last segment added that's
    /// unique but deterministic for each revision.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    /// use wnfs::private::{
    ///     PrivateFile, AesKey,
    ///     forest::{hamt::HamtForest, traits::PrivateForest},
    /// };
    /// use chrono::Utc;
    /// use rand::thread_rng;
    ///
    /// let rng = &mut thread_rng();
    /// let forest = &mut Rc::new(HamtForest::new_rsa_2048(rng));
    /// let file = Rc::new(PrivateFile::new(
    ///     &forest.empty_name(),
    ///     Utc::now(),
    ///     rng,
    /// ));
    /// let revision_name = file.header.get_revision_name();
    ///
    /// println!("Revision name: {:?}", revision_name);
    /// ```
    pub fn get_revision_name(&self) -> Name {
        self.name
            .with_segments_added(Some(self.derive_revision_segment()))
    }

    /// Gets the name for this node.
    /// This name is persistent across revisions and can be used as the "allowed base name"
    /// for delegating write access.
    pub fn get_name(&self) -> &Name {
        &self.name
    }

    /// Encrypts this private node header in an block, then stores that in the given
    /// BlockStore and returns its CID.
    pub async fn store(&self, store: &impl BlockStore, setup: &AccumulatorSetup) -> Result<Cid> {
        let temporal_key = self.derive_temporal_key();
        let cbor_bytes = serde_ipld_dagcbor::to_vec(&self.to_serializable(setup))?;
        let ciphertext = temporal_key.key_wrap_encrypt(&cbor_bytes)?;
        store.put_block(ciphertext, CODEC_RAW).await
    }

    pub(crate) fn to_serializable(
        &self,
        setup: &AccumulatorSetup,
    ) -> PrivateNodeHeaderSerializable {
        PrivateNodeHeaderSerializable {
            inumber: self.inumber.clone(),
            ratchet: self.ratchet.clone(),
            name: self.name.as_accumulator(setup).clone(),
        }
    }

    pub(crate) fn from_serializable(serializable: PrivateNodeHeaderSerializable) -> Self {
        Self {
            inumber: serializable.inumber,
            ratchet: serializable.ratchet,
            name: Name::new(serializable.name, []),
        }
    }

    /// Loads a private node header from a given CID linking to the ciphertext block
    /// to be decrypted with given key.
    pub(crate) async fn load(
        cid: &Cid,
        temporal_key: &TemporalKey,
        store: &impl BlockStore,
        parent_name: Option<Name>,
        setup: &AccumulatorSetup,
    ) -> Result<Self> {
        let ciphertext = store.get_block(cid).await?;
        let cbor_bytes = temporal_key.key_wrap_decrypt(&ciphertext)?;
        let decoded: PrivateNodeHeaderSerializable = serde_ipld_dagcbor::from_slice(&cbor_bytes)?;
        let serialized_name = decoded.name.clone();
        let mut header = Self::from_serializable(decoded);
        if let Some(parent_name) = parent_name {
            let name = parent_name.with_segments_added([header.inumber.clone()]);
            let mounted_acc = name.as_accumulator(setup);
            if mounted_acc != &serialized_name {
                bail!(FsError::MountPointAndDeserializedNameMismatch(
                    format!("{mounted_acc:?}"),
                    format!("{serialized_name:?}"),
                ));
            }
            header.name = name;
        }
        Ok(header)
    }
}
