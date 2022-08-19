use std::{collections::BTreeMap, rc::Rc};

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    namefilter::Namefilter, INumber, PrivateFile, PrivateForest, PrivateNode, PrivateNodeHeader,
    PrivateRef, Rng,
};

use crate::{
    error, utils, BlockStore, FsError, HashOutput, Id, Metadata, PathNodes, PathNodesResult,
    UnixFsNodeKind, HASH_BYTE_SIZE,
};

//--------------------------------------------------------------------------------------------------
// Type Definitions
//--------------------------------------------------------------------------------------------------

pub type PrivatePathNodes = PathNodes<PrivateDirectory>;
pub type PrivatePathNodesResult = PathNodesResult<PrivateDirectory>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivateDirectoryContent {
    pub(crate) metadata: Metadata,
    pub(crate) entries: BTreeMap<String, PrivateRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrivateDirectory {
    pub(crate) header: PrivateNodeHeader,
    pub(crate) content: PrivateDirectoryContent,
}

/// The result of an operation applied to a directory.
#[derive(Debug, Clone, PartialEq)]
pub struct PrivateOpResult<T> {
    /// The root directory.
    pub root_dir: Rc<PrivateDirectory>,
    /// The hamt forest.
    pub hamt: Rc<PrivateForest>,
    /// Implementation dependent but it usually the last leaf node operated on.
    pub result: T,
}

//--------------------------------------------------------------------------------------------------
// Implementations
//--------------------------------------------------------------------------------------------------

impl PrivateDirectory {
    /// Creates a new directory with provided details.
    pub fn new(
        parent_bare_name: Namefilter,
        inumber: INumber,
        ratchet_seed: HashOutput,
        time: DateTime<Utc>,
    ) -> Self {
        Self {
            header: PrivateNodeHeader::new(parent_bare_name, inumber, ratchet_seed),
            content: PrivateDirectoryContent {
                metadata: Metadata::new(time, UnixFsNodeKind::Dir),
                entries: BTreeMap::new(),
            },
        }
    }

    /// Generates two random set of bytes.
    pub fn generate_double_random<R: Rng>(rng: &R) -> (HashOutput, HashOutput) {
        const _DOUBLE_SIZE: usize = HASH_BYTE_SIZE * 2;
        let [first, second] = unsafe {
            std::mem::transmute::<[u8; _DOUBLE_SIZE], [[u8; HASH_BYTE_SIZE]; 2]>(
                rng.random_bytes::<_DOUBLE_SIZE>(),
            )
        };
        (first, second)
    }

    ///  Advances the ratchet.
    pub(crate) fn advance_ratchet(&mut self) {
        self.header.advance_ratchet();
    }

    /// Creates a new `PathNodes` that is not based on an existing file tree.
    pub(crate) fn create_path_nodes<R: Rng>(
        path_segments: &[String],
        time: DateTime<Utc>,
        parent_bare_name: Namefilter,
        rng: &R,
    ) -> PrivatePathNodes {
        let mut working_parent_bare_name = parent_bare_name;
        let (mut inumber, mut ratchet_seed) = Self::generate_double_random(rng);

        let path: Vec<(Rc<PrivateDirectory>, String)> = path_segments
            .iter()
            .map(|segment| {
                // Create new private directory.
                let directory = Rc::new(PrivateDirectory::new(
                    std::mem::take(&mut working_parent_bare_name),
                    inumber,
                    ratchet_seed,
                    time,
                ));

                // Update seeds and the working parent bare name.
                (inumber, ratchet_seed) = Self::generate_double_random(rng);
                working_parent_bare_name = directory.header.bare_name.clone();

                (directory, segment.clone())
            })
            .collect();

        PrivatePathNodes {
            path,
            tail: Rc::new(PrivateDirectory::new(
                std::mem::take(&mut working_parent_bare_name),
                inumber,
                ratchet_seed,
                time,
            )),
        }
    }

    /// Uses specified path segments and their existence in the file tree to generate `PathNodes`.
    ///
    /// Supports cases where the entire path does not exist.
    pub(crate) async fn get_path_nodes<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        hamt: &PrivateForest,
        store: &B,
    ) -> Result<PrivatePathNodesResult> {
        use PathNodesResult::*;
        let mut working_node = self;
        let mut path_nodes = Vec::with_capacity(path_segments.len());

        for path_segment in path_segments {
            match working_node.lookup_node(path_segment, hamt, store).await? {
                Some(PrivateNode::Dir(ref directory)) => {
                    path_nodes.push((Rc::clone(&working_node), path_segment.clone()));
                    working_node = Rc::clone(directory);
                }
                Some(_) => {
                    let path_nodes = PrivatePathNodes {
                        path: path_nodes,
                        tail: Rc::clone(&working_node),
                    };

                    return Ok(NotADirectory(path_nodes, path_segment.clone()));
                }
                None => {
                    let path_nodes = PrivatePathNodes {
                        path: path_nodes,
                        tail: Rc::clone(&working_node),
                    };

                    return Ok(MissingLink(path_nodes, path_segment.clone()));
                }
            }
        }

        Ok(Complete(PrivatePathNodes {
            path: path_nodes,
            tail: Rc::clone(&working_node),
        }))
    }

    /// Uses specified path segments to generate `PathNodes`. Creates missing directories as needed.
    pub(crate) async fn get_or_create_path_nodes<B: BlockStore, R: Rng>(
        self: Rc<Self>,
        path_segments: &[String],
        time: DateTime<Utc>,
        hamt: &PrivateForest,
        store: &mut B,
        rng: &R,
    ) -> Result<PrivatePathNodes> {
        use PathNodesResult::*;
        match self.get_path_nodes(path_segments, hamt, store).await? {
            Complete(path_nodes) => Ok(path_nodes),
            NotADirectory(_, _) => error(FsError::InvalidPath),
            MissingLink(path_so_far, missing_link) => {
                // Get remaining missing path segments.
                let missing_path = path_segments.split_at(path_so_far.path.len() + 1).1;

                // Get tail bare name from `path_so_far`.
                let parent_bare_name = path_so_far.tail.header.bare_name.clone();

                // Create missing directories.
                let missing_path_nodes =
                    Self::create_path_nodes(missing_path, time, parent_bare_name, rng);

                Ok(PrivatePathNodes {
                    path: [
                        path_so_far.path,
                        vec![(path_so_far.tail, missing_link)],
                        missing_path_nodes.path,
                    ]
                    .concat(),
                    tail: missing_path_nodes.tail,
                })
            }
        }
    }

    /// Fix up `PathNodes` so that parents refer to the newly updated children.
    async fn fix_up_path_nodes<B: BlockStore, R: Rng>(
        path_nodes: PrivatePathNodes,
        hamt: Rc<PrivateForest>,
        store: &mut B,
        rng: &R,
    ) -> Result<(Rc<Self>, Rc<PrivateForest>)> {
        let mut working_hamt = Rc::clone(&hamt);
        let mut working_child_dir = {
            let mut tmp = (*path_nodes.tail).clone();
            tmp.advance_ratchet();
            Rc::new(tmp)
        };

        for (parent_dir, segment) in path_nodes.path.iter().rev() {
            let mut parent_dir = (**parent_dir).clone();
            parent_dir.advance_ratchet();
            let child_private_ref = working_child_dir.header.get_private_ref()?;

            parent_dir
                .content
                .entries
                .insert(segment.clone(), child_private_ref.clone());

            let parent_dir = Rc::new(parent_dir);

            working_hamt = working_hamt
                .set(
                    working_child_dir.header.get_saturated_name(),
                    &child_private_ref,
                    &PrivateNode::Dir(Rc::clone(&working_child_dir)),
                    store,
                    rng,
                )
                .await?;

            working_child_dir = parent_dir;
        }

        working_hamt = working_hamt
            .set(
                working_child_dir.header.get_saturated_name(),
                &working_child_dir.header.get_private_ref()?,
                &PrivateNode::Dir(Rc::clone(&working_child_dir)),
                store,
                rng,
            )
            .await?;

        Ok((working_child_dir, working_hamt))
    }

    /// Follows a path and fetches the node at the end of the path.
    pub async fn get_node<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        hamt: Rc<PrivateForest>,
        store: &B,
    ) -> Result<PrivateOpResult<Option<PrivateNode>>> {
        use PathNodesResult::*;
        let root_dir = Rc::clone(&self);

        Ok(match path_segments.split_last() {
            Some((path_segment, parent_path)) => {
                match self.get_path_nodes(parent_path, &hamt, store).await? {
                    Complete(parent_path_nodes) => {
                        let result = parent_path_nodes
                            .tail
                            .lookup_node(path_segment, &hamt, store)
                            .await?;

                        PrivateOpResult {
                            root_dir,
                            hamt,
                            result,
                        }
                    }
                    MissingLink(_, _) => bail!(FsError::NotFound),
                    NotADirectory(_, _) => bail!(FsError::NotFound),
                }
            }
            None => PrivateOpResult {
                root_dir,
                hamt,
                result: Some(PrivateNode::Dir(self)),
            },
        })
    }

    /// Reads specified file content from the directory.
    pub async fn read<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        hamt: Rc<PrivateForest>,
        store: &B,
    ) -> Result<PrivateOpResult<Vec<u8>>> {
        let root_dir = Rc::clone(&self);
        let (path, filename) = utils::split_last(path_segments)?;

        match self.get_path_nodes(path, &hamt, store).await? {
            PathNodesResult::Complete(node_path) => {
                match node_path.tail.lookup_node(filename, &hamt, store).await? {
                    Some(PrivateNode::File(file)) => Ok(PrivateOpResult {
                        root_dir,
                        hamt,
                        result: file.content.content.clone(),
                    }),
                    Some(PrivateNode::Dir(_)) => error(FsError::NotAFile),
                    None => error(FsError::NotFound),
                }
            }
            _ => error(FsError::NotFound),
        }
    }

    /// Writes a file to the directory.
    pub async fn write<B: BlockStore, R: Rng>(
        self: Rc<Self>,
        path_segments: &[String],
        time: DateTime<Utc>,
        content: Vec<u8>,
        hamt: Rc<PrivateForest>,
        store: &mut B,
        rng: &R,
    ) -> Result<PrivateOpResult<()>> {
        let (directory_path, filename) = utils::split_last(path_segments)?;

        // This will create directories if they don't exist yet
        let mut directory_path_nodes = self
            .get_or_create_path_nodes(directory_path, time, &hamt, store, rng)
            .await?;

        let mut directory = (*directory_path_nodes.tail).clone();

        // Modify the file if it already exists, otherwise create a new file with expected content
        let file = match directory.lookup_node(filename, &hamt, store).await? {
            Some(PrivateNode::File(file_before)) => {
                let mut file = (*file_before).clone();
                file.content.content = content;
                file.content.metadata = Metadata::new(time, UnixFsNodeKind::File);
                file
            }
            Some(PrivateNode::Dir(_)) => bail!(FsError::DirectoryAlreadyExists),
            None => {
                let (inumber, ratchet_seed) = Self::generate_double_random(rng);
                PrivateFile::new(
                    directory.header.bare_name.clone(),
                    inumber,
                    ratchet_seed,
                    time,
                    content,
                )
            }
        };

        let child_private_ref = file.header.get_private_ref()?;
        let hamt = hamt
            .set(
                file.header.get_saturated_name(),
                &child_private_ref,
                &PrivateNode::File(Rc::new(file)),
                store,
                rng,
            )
            .await?;

        // Insert the file into its parent directory
        directory
            .content
            .entries
            .insert(filename.to_string(), child_private_ref);

        directory_path_nodes.tail = Rc::new(directory);

        let (root_dir, hamt) =
            Self::fix_up_path_nodes(directory_path_nodes, hamt, store, rng).await?;

        // Fix up the file path
        Ok(PrivateOpResult {
            root_dir,
            hamt,
            result: (),
        })
    }

    /// Looks up a node by its path name in the current directory.
    pub async fn lookup_node<'a, B: BlockStore>(
        &self,
        path_segment: &str,
        hamt: &PrivateForest,
        store: &B,
    ) -> Result<Option<PrivateNode>> {
        Ok(match self.content.entries.get(path_segment) {
            Some(private_ref) => hamt.get(private_ref, store).await?,
            None => None,
        })
    }

    /// Creates a new directory at the specified path.
    pub async fn mkdir<B: BlockStore, R: Rng>(
        self: Rc<Self>,
        path_segments: &[String],
        time: DateTime<Utc>,
        hamt: Rc<PrivateForest>,
        store: &mut B,
        rng: &R,
    ) -> Result<PrivateOpResult<()>> {
        let path_nodes = self
            .get_or_create_path_nodes(path_segments, time, &hamt, store, rng)
            .await?;

        let (root_dir, hamt) = Self::fix_up_path_nodes(path_nodes, hamt, store, rng).await?;

        Ok(PrivateOpResult {
            root_dir,
            hamt,
            result: (),
        })
    }

    /// Returns names and metadata of directory's immediate children.
    pub async fn ls<B: BlockStore>(
        self: Rc<Self>,
        path_segments: &[String],
        hamt: Rc<PrivateForest>,
        store: &B,
    ) -> Result<PrivateOpResult<Vec<(String, Metadata)>>> {
        let root_dir = Rc::clone(&self);
        match self.get_path_nodes(path_segments, &hamt, store).await? {
            PathNodesResult::Complete(path_nodes) => {
                let mut result = vec![];
                for (name, private_ref) in path_nodes.tail.content.entries.iter() {
                    match hamt.get(private_ref, store).await? {
                        Some(PrivateNode::File(file)) => {
                            result.push((name.clone(), file.content.metadata.clone()));
                        }
                        Some(PrivateNode::Dir(dir)) => {
                            result.push((name.clone(), dir.content.metadata.clone()));
                        }
                        _ => bail!(FsError::NotFound),
                    }
                }
                Ok(PrivateOpResult {
                    root_dir,
                    hamt,
                    result,
                })
            }
            _ => bail!(FsError::NotFound),
        }
    }

    /// Removes a file or directory from the directory.
    pub async fn rm<B: BlockStore, R: Rng>(
        self: Rc<Self>,
        path_segments: &[String],
        hamt: Rc<PrivateForest>,
        store: &mut B,
        rng: &R,
    ) -> Result<PrivateOpResult<PrivateNode>> {
        let (directory_path, node_name) = utils::split_last(path_segments)?;

        let mut directory_path_nodes =
            match self.get_path_nodes(directory_path, &hamt, store).await? {
                PrivatePathNodesResult::Complete(node_path) => node_path,
                _ => bail!(FsError::NotFound),
            };

        let mut directory = (*directory_path_nodes.tail).clone();

        // Remove the entry from its parent directory
        let removed_node = match directory.content.entries.remove(node_name) {
            Some(ref private_ref) => hamt.get(private_ref, store).await?.unwrap(),
            None => bail!(FsError::NotFound),
        };

        directory_path_nodes.tail = Rc::new(directory);

        let (root_dir, hamt) =
            Self::fix_up_path_nodes(directory_path_nodes, hamt, store, rng).await?;

        Ok(PrivateOpResult {
            root_dir,
            hamt,
            result: removed_node,
        })
    }
}

impl Id for PrivateDirectory {
    fn get_id(&self) -> String {
        format!("{:p}", &self.header)
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod private_directory_tests {
    use super::*;
    use crate::{utils::TestRng, MemoryBlockStore, HASH_BYTE_SIZE};
    use test_log::test;

    #[test(async_std::test)]
    async fn look_up_can_fetch_file_added_to_directory() {
        let rng = &TestRng();
        let root_dir = Rc::new(PrivateDirectory::new(
            Namefilter::default(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            Utc::now(),
        ));
        let store = &mut MemoryBlockStore::default();
        let hamt = Rc::new(PrivateForest::new());

        let content = b"Hello, World!".to_vec();

        let PrivateOpResult { root_dir, hamt, .. } = root_dir
            .write(
                &["text.txt".into()],
                Utc::now(),
                content.clone(),
                hamt,
                store,
                rng,
            )
            .await
            .unwrap();

        let PrivateOpResult { result, .. } = root_dir
            .read(&["text.txt".into()], hamt, store)
            .await
            .unwrap();

        assert_eq!(result, content);
    }

    #[test(async_std::test)]
    async fn look_up_cannot_fetch_file_not_added_to_directory() {
        let rng = &TestRng();
        let root_dir = Rc::new(PrivateDirectory::new(
            Namefilter::default(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            Utc::now(),
        ));
        let store = &mut MemoryBlockStore::default();
        let hamt = Rc::new(PrivateForest::new());

        let node = root_dir.lookup_node("Unknown", &hamt, store).await.unwrap();

        assert!(node.is_none());
    }

    #[test(async_std::test)]
    async fn mkdir_can_create_new_directory() {
        let rng = &TestRng();
        let root_dir = Rc::new(PrivateDirectory::new(
            Namefilter::default(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            Utc::now(),
        ));
        let store = &mut MemoryBlockStore::default();
        let hamt = Rc::new(PrivateForest::new());

        let PrivateOpResult { root_dir, hamt, .. } = root_dir
            .mkdir(
                &["tamedun".into(), "pictures".into()],
                Utc::now(),
                hamt,
                store,
                rng,
            )
            .await
            .unwrap();

        let PrivateOpResult { result, .. } = root_dir
            .get_node(&["tamedun".into(), "pictures".into()], hamt, store)
            .await
            .unwrap();

        assert!(result.is_some());
    }

    #[test(async_std::test)]
    async fn ls_can_list_children_under_directory() {
        let rng = &TestRng();
        let root_dir = Rc::new(PrivateDirectory::new(
            Namefilter::default(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            Utc::now(),
        ));
        let store = &mut MemoryBlockStore::default();
        let hamt = Rc::new(PrivateForest::new());

        let PrivateOpResult { root_dir, hamt, .. } = root_dir
            .mkdir(
                &["tamedun".into(), "pictures".into()],
                Utc::now(),
                hamt,
                store,
                rng,
            )
            .await
            .unwrap();

        let PrivateOpResult { root_dir, hamt, .. } = root_dir
            .write(
                &["tamedun".into(), "pictures".into(), "puppy.jpg".into()],
                Utc::now(),
                b"puppy".to_vec(),
                hamt,
                store,
                rng,
            )
            .await
            .unwrap();

        let PrivateOpResult { root_dir, hamt, .. } = root_dir
            .mkdir(
                &["tamedun".into(), "pictures".into(), "cats".into()],
                Utc::now(),
                hamt,
                store,
                rng,
            )
            .await
            .unwrap();

        let PrivateOpResult { result, .. } = root_dir
            .ls(&["tamedun".into(), "pictures".into()], hamt, store)
            .await
            .unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result[0].0, String::from("cats"));
        assert_eq!(result[1].0, String::from("puppy.jpg"));
        assert_eq!(result[0].1.unix_fs.kind, UnixFsNodeKind::Dir);
        assert_eq!(result[1].1.unix_fs.kind, UnixFsNodeKind::File);
    }

    #[test(async_std::test)]
    async fn rm_can_remove_children_from_directory() {
        let rng = &TestRng();
        let root_dir = Rc::new(PrivateDirectory::new(
            Namefilter::default(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            Utc::now(),
        ));
        let store = &mut MemoryBlockStore::default();
        let hamt = Rc::new(PrivateForest::new());

        let PrivateOpResult { root_dir, hamt, .. } = root_dir
            .mkdir(
                &["tamedun".into(), "pictures".into()],
                Utc::now(),
                hamt,
                store,
                rng,
            )
            .await
            .unwrap();

        let PrivateOpResult { root_dir, hamt, .. } = root_dir
            .write(
                &["tamedun".into(), "pictures".into(), "puppy.jpg".into()],
                Utc::now(),
                b"puppy".to_vec(),
                hamt,
                store,
                rng,
            )
            .await
            .unwrap();

        let PrivateOpResult { root_dir, hamt, .. } = root_dir
            .mkdir(
                &["tamedun".into(), "pictures".into(), "cats".into()],
                Utc::now(),
                hamt,
                store,
                rng,
            )
            .await
            .unwrap();

        let PrivateOpResult { root_dir, hamt, .. } = root_dir
            .rm(&["tamedun".into(), "pictures".into()], hamt, store, rng)
            .await
            .unwrap();

        let result = root_dir
            .rm(&["tamedun".into(), "pictures".into()], hamt, store, rng)
            .await;

        assert!(result.is_err());
    }

    #[async_std::test]
    async fn read_can_fetch_userland_of_file_added_to_directory() {
        let rng = &TestRng();
        let root_dir = Rc::new(PrivateDirectory::new(
            Namefilter::default(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            rng.random_bytes::<HASH_BYTE_SIZE>(),
            Utc::now(),
        ));
        let store = &mut MemoryBlockStore::default();
        let hamt = Rc::new(PrivateForest::new());

        let PrivateOpResult { root_dir, hamt, .. } = root_dir
            .write(
                &["text.txt".into()],
                Utc::now(),
                b"text".to_vec(),
                hamt,
                store,
                rng,
            )
            .await
            .unwrap();

        let PrivateOpResult { result, .. } = root_dir
            .read(&["text.txt".into()], hamt, store)
            .await
            .unwrap();

        assert_eq!(result, b"text".to_vec());
    }

    #[async_std::test]
    async fn path_nodes_can_generates_new_path_nodes() {
        let store = &mut MemoryBlockStore::default();
        let hamt = Rc::new(PrivateForest::new());
        let rng = &TestRng();

        let path_nodes = PrivateDirectory::create_path_nodes(
            &["Documents".into(), "Apps".into()],
            Utc::now(),
            Namefilter::default(),
            rng,
        );

        let (root_dir, hamt) =
            PrivateDirectory::fix_up_path_nodes(path_nodes.clone(), hamt, store, rng)
                .await
                .unwrap();

        let result = root_dir
            .get_path_nodes(&["Documents".into(), "Apps".into()], &hamt, store)
            .await
            .unwrap();

        match result {
            PathNodesResult::MissingLink(_, segment) => panic!("MissingLink {segment}"),
            PathNodesResult::NotADirectory(_, segment) => panic!("NotADirectory {segment}"),
            PathNodesResult::Complete(path_nodes_2) => {
                assert_eq!(path_nodes.path.len(), path_nodes_2.path.len());
                assert_eq!(path_nodes.path[0].1, path_nodes_2.path[0].1);
                assert_eq!(path_nodes.path[1].1, path_nodes_2.path[1].1);
            }
        }
    }
}
