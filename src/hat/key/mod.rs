// Copyright 2014 Google Inc. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! External API for creating and manipulating snapshots.

use std::boxed::FnBox;

use blob;
use hash;
use hash::tree::{HashTreeBackend, SimpleHashTreeWriter, SimpleHashTreeReader, ReaderResult};

use process::{Process, MsgHandler};

mod index;
pub use self::index::{Index, IndexProcess, Entry};


pub type StoreProcess<IT> = Process<Msg<IT>, Reply>;

pub type DirElem = (Entry,
                    Option<blob::ChunkRef>,
                    Option<Box<FnBox() -> Option<ReaderResult<HashStoreBackend>> + Send>>);

// Public structs
pub enum Msg<IT> {
    /// Insert a key into the index. If this key has associated data a "chunk-iterator creator"
    /// can be passed along with it. If the data turns out to be unreadable, this iterator proc
    /// can return `None`. Returns `Id` with the new entry ID.
    Insert(Entry, Option<Box<FnBox() -> Option<IT> + Send>>),

    /// List a "directory" (aka. a `level`) in the index.
    /// Returns `ListResult` with all the entries under the given parent.
    ListDir(Option<u64>),

    /// Flush this key store and its dependencies.
    /// Returns `FlushOk`.
    Flush,
}

pub enum Reply {
    Id(u64),
    ListResult(Vec<DirElem>),
    FlushOk,
}

#[derive(Clone)]
pub struct Store {
    index: index::IndexProcess,
    hash_index: hash::IndexProcess,
    blob_store: blob::StoreProcess,
}

// Implementations
impl Store {
    pub fn new(index: index::IndexProcess,
               hash_index: hash::IndexProcess,
               blob_store: blob::StoreProcess)
               -> Store {
        Store {
            index: index,
            hash_index: hash_index,
            blob_store: blob_store,
        }
    }

    #[cfg(test)]
    pub fn new_for_testing<B: 'static + blob::StoreBackend + Send + Clone>(backend: B) -> Store {
        let ki_p = Process::new(Box::new(move || index::Index::new_for_testing()));
        let hi_p = Process::new(Box::new(move || hash::Index::new_for_testing()));
        let bs_p = Process::new(Box::new(move || blob::Store::new_for_testing(backend, 1024)));
        Store {
            index: ki_p,
            hash_index: hi_p,
            blob_store: bs_p,
        }
    }

    pub fn flush(&mut self) {
        self.blob_store.send_reply(blob::Msg::Flush);
        self.hash_index.send_reply(hash::Msg::Flush);
        self.index.send_reply(index::Msg::Flush);
    }

    pub fn hash_tree_writer(&mut self) -> SimpleHashTreeWriter<HashStoreBackend> {
        let backend = HashStoreBackend::new(self.hash_index.clone(), self.blob_store.clone());
        return SimpleHashTreeWriter::new(8, backend);
    }
}

#[derive(Clone)]
pub struct HashStoreBackend {
    hash_index: hash::IndexProcess,
    blob_store: blob::StoreProcess,
}

impl HashStoreBackend {
    pub fn new(hash_index: hash::IndexProcess, blob_store: blob::StoreProcess) -> HashStoreBackend {
        HashStoreBackend {
            hash_index: hash_index,
            blob_store: blob_store,
        }
    }

    fn fetch_chunk_from_hash(&mut self, hash: hash::Hash) -> Option<Vec<u8>> {
        assert!(!hash.bytes.is_empty());
        match self.hash_index.send_reply(hash::Msg::FetchPersistentRef(hash)) {
            hash::Reply::PersistentRef(chunk_ref) => {
                self.fetch_chunk_from_persistent_ref(chunk_ref)
            }
            _ => None,  // TODO: Do we need to distinguish `missing` from `unknown ref`?
        }
    }

    fn fetch_chunk_from_persistent_ref(&mut self, chunk_ref: blob::ChunkRef) -> Option<Vec<u8>> {
        match self.blob_store.send_reply(blob::Msg::Retrieve(chunk_ref)) {
            blob::Reply::RetrieveOk(chunk) => Some(chunk),
            _ => None,
        }
    }
}

impl HashTreeBackend for HashStoreBackend {
    fn fetch_chunk(&mut self,
                   hash: hash::Hash,
                   persistent_ref: Option<blob::ChunkRef>)
                   -> Option<Vec<u8>> {
        assert!(!hash.bytes.is_empty());
        if let Some(r) = persistent_ref {
            return self.fetch_chunk_from_persistent_ref(r);
        }
        return self.fetch_chunk_from_hash(hash);
    }

    fn fetch_persistent_ref(&mut self, hash: hash::Hash) -> Option<blob::ChunkRef> {
        assert!(!hash.bytes.is_empty());
        loop {
            match self.hash_index.send_reply(hash::Msg::FetchPersistentRef(hash.clone())) {
                hash::Reply::PersistentRef(r) => return Some(r), // done
                hash::Reply::HashNotKnown => return None, // done
                hash::Reply::Retry => (),  // continue loop
                _ => panic!("Unexpected reply from hash index."),
            }
        }
    }

    fn fetch_payload(&mut self, hash: hash::Hash) -> Option<Vec<u8>> {
        match self.hash_index.send_reply(hash::Msg::FetchPayload(hash)) {
            hash::Reply::Payload(p) => return p, // done
            hash::Reply::HashNotKnown => return None, // done
            _ => panic!("Unexpected reply from hash index."),
        }
    }

    fn insert_chunk(&mut self,
                    hash: hash::Hash,
                    level: i64,
                    payload: Option<Vec<u8>>,
                    chunk: Vec<u8>)
                    -> blob::ChunkRef {
        assert!(!hash.bytes.is_empty());

        let mut hash_entry = hash::Entry {
            hash: hash.clone(),
            level: level,
            payload: payload,
            persistent_ref: None,
        };

        match self.hash_index.send_reply(hash::Msg::Reserve(hash_entry.clone())) {
            hash::Reply::HashKnown => {
                // Someone came before us: piggyback on their result.
                return self.fetch_persistent_ref(hash)
                           .expect("Could not find persistent_ref for known chunk.");
            }
            hash::Reply::ReserveOk => {
                // We came first: this data-chunk is ours to process.
                let local_hash_index = self.hash_index.clone();

                let callback = Box::new(move |chunk_ref: blob::ChunkRef| {
                    local_hash_index.send_reply(hash::Msg::Commit(hash, chunk_ref));
                });
                match self.blob_store.send_reply(blob::Msg::Store(chunk, callback)) {
                    blob::Reply::StoreOk(chunk_ref) => {
                        hash_entry.persistent_ref = Some(chunk_ref.clone());
                        self.hash_index.send_reply(hash::Msg::UpdateReserved(hash_entry));
                        return chunk_ref;
                    }
                    _ => panic!("Unexpected reply from BlobStore."),
                };
            }
            _ => panic!("Unexpected HashIndex reply."),
        };
    }
}

fn file_size_warning(name: &[u8], wanted: u64, got: u64) {
    if wanted < got {
        println!("Warning: File grew while reading it: {:?} (wanted {}, got {})",
                 name,
                 wanted,
                 got)
    } else if wanted > got {
        println!("Warning: Could not read whole file (or it shrank): {:?} (wanted {}, got {})",
                 name,
                 wanted,
                 got)
    }
}

impl<IT: Iterator<Item = Vec<u8>>> MsgHandler<Msg<IT>, Reply> for Store {
    fn handle(&mut self, msg: Msg<IT>, reply: Box<Fn(Reply)>) {
        match msg {
            Msg::Flush => {
                self.flush();
                return reply(Reply::FlushOk);
            }

            Msg::ListDir(parent) => {
                match self.index.send_reply(index::Msg::ListDir(parent)) {
                    index::Reply::ListResult(entries) => {
                        let mut my_entries: Vec<DirElem> = Vec::with_capacity(entries.len());
                        for (entry, persistent_ref) in entries.into_iter() {
                            let open_fn = entry.data_hash.as_ref().map(|bytes| {
                                let local_hash = hash::Hash { bytes: bytes.clone() };
                                let local_ref = persistent_ref.clone();
                                let local_hash_index = self.hash_index.clone();
                                let local_blob_store = self.blob_store.clone();
                                Box::new(move || {
                                    SimpleHashTreeReader::open(
                                        HashStoreBackend::new(local_hash_index.clone(),
                                                              local_blob_store.clone()),
                                        local_hash, local_ref) })
                                    as Box<FnBox() -> Option<ReaderResult<HashStoreBackend>> + Send>
                            });

                            my_entries.push((entry, persistent_ref, open_fn));
                        }
                        return reply(Reply::ListResult(my_entries));
                    }
                    _ => panic!("Unexpected result from key index."),
                }
            }

            Msg::Insert(org_entry, chunk_it_opt) => {
                let entry = match self.index.send_reply(index::Msg::LookupExact(org_entry)) {
                    index::Reply::Entry(entry) => {
                        if entry.data_hash.is_some() {
                            match self.hash_index
                                      .send_reply(hash::Msg::HashExists(hash::Hash {
                                          bytes: entry.data_hash.clone().unwrap(),
                                      })) {
                                hash::Reply::HashKnown => {
                                    return reply(Reply::Id(entry.id.unwrap()));
                                }
                                _ => entry,
                            }
                        } else {
                            entry
                        }
                    }
                    index::Reply::NotFound(entry) => {
                        match self.index.send_reply(index::Msg::Insert(entry)) {
                            index::Reply::Entry(entry) => entry,
                            _ => panic!("Could not insert entry into key index."),
                        }
                    }
                    _ => panic!("Unexpected reply from key index."),
                };

                // Send out the ID early to allow the client to continue its key discovery routine.
                // The bounded input-channel will prevent the client from overflowing us.
                assert!(entry.id.is_some());
                reply(Reply::Id(entry.id.unwrap().clone()));


                // Setup hash tree structure
                let mut tree = self.hash_tree_writer();

                // Check if we have an data source:
                let it_opt = chunk_it_opt.and_then(|open| open());
                if it_opt.is_none() {
                    // No data is associated with this entry.
                    self.index.send_reply(index::Msg::UpdateDataHash(entry, None, None));
                    // Bail out before storing data that does not exist:
                    return;
                }

                // Read and insert all file chunks:
                // (see HashStoreBackend::insert_chunk above)
                let mut bytes_read = 0u64;
                for chunk in it_opt.unwrap() {
                    bytes_read += chunk.len() as u64;
                    tree.append(chunk);
                }

                // Warn the user if we did not read the expected size:
                entry.data_length.map(|s| {
                    file_size_warning(&entry.name, s, bytes_read);
                });

                // Get top tree hash:
                let (hash, persistent_ref) = tree.hash();

                // Update hash in key index.
                // It is OK that this has is not yet valid, as we check hashes at snapshot time.
                match self.index.send_reply(index::Msg::UpdateDataHash(entry,
                                                                       Some(hash),
                                                                       Some(persistent_ref))) {
                    index::Reply::UpdateOk => (),
                    _ => panic!("Unexpected reply from key index."),
                };
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    use blob::tests::{MemoryBackend, DevNullBackend};
    use process::Process;

    use rand::Rng;
    use rand::thread_rng;

    use test::Bencher;
    use quickcheck;

    fn random_ascii_bytes() -> Vec<u8> {
        let ascii: String = thread_rng().gen_ascii_chars().take(32).collect();
        ascii.into_bytes()
    }

    #[derive(Clone, Debug)]
    struct EntryStub {
        key_entry: Entry,
        data: Option<Vec<Vec<u8>>>,
    }

    impl Iterator for EntryStub {
        type Item = Vec<u8>;

        fn next(&mut self) -> Option<Vec<u8>> {
            match self.data.as_mut() {
                Some(x) => {
                    if !x.is_empty() {
                        Some(x.remove(0))
                    } else {
                        None
                    }
                }
                None => None,
            }
        }
    }

    #[derive(Clone, Debug)]
    struct FileSystem {
        file: EntryStub,
        filelist: Vec<FileSystem>,
    }

    fn rng_filesystem(size: usize) -> FileSystem {
        fn create_files(size: usize) -> Vec<FileSystem> {
            let children = size as f32 / 10.0;

            // dist_factor * i for i in range(children) + children == size
            let dist_factor: f32 = (size as f32 - children) / ((children * (children - 1.0)) / 2.0);

            let mut child_size = 0.0 as f32;

            let mut files = Vec::new();
            for _ in 0..children as usize {

                let data_opt = if thread_rng().gen() {
                    None
                } else {
                    let mut v = vec![];
                    for _ in 0..8 {
                        v.push(random_ascii_bytes())
                    }
                    Some(v)
                };

                let new_root = EntryStub {
                    data: data_opt,
                    key_entry: Entry {
                        id: None,
                        parent_id: None, // updated by insert_and_update_fs()

                        name: random_ascii_bytes(),
                        data_hash: None,
                        data_length: None,

                        created: thread_rng().gen(),
                        modified: thread_rng().gen(),
                        accessed: thread_rng().gen(),

                        permissions: None,
                        user_id: None,
                        group_id: None,
                    },
                };

                files.push(FileSystem {
                    file: new_root,
                    filelist: create_files(child_size as usize),
                });
                child_size += dist_factor;
            }
            files
        }

        let root = EntryStub {
            data: None,
            key_entry: Entry {
                parent_id: None,
                id: None, // updated by insert_and_update_fs()
                name: b"root".to_vec(),
                data_hash: None,
                data_length: None,
                created: thread_rng().gen(),
                modified: thread_rng().gen(),
                accessed: thread_rng().gen(),
                permissions: None,
                user_id: None,
                group_id: None,
            },
        };

        FileSystem {
            file: root,
            filelist: create_files(size),
        }
    }

    fn insert_and_update_fs(fs: &mut FileSystem, ks_p: StoreProcess<EntryStub>) {
        let local_file = fs.file.clone();
        let id = match ks_p.send_reply(Msg::Insert(fs.file.key_entry.clone(),
                                                   if fs.file.data.is_some() {
                                                       Some(Box::new(move || Some(local_file)))
                                                   } else {
                                                       None
                                                   })) {
            Reply::Id(id) => id,
            _ => panic!("unexpected reply from key store"),
        };

        fs.file.key_entry.id = Some(id);

        for f in fs.filelist.iter_mut() {
            f.file.key_entry.parent_id = Some(id);
            insert_and_update_fs(f, ks_p.clone());
        }
    }

    fn verify_filesystem(fs: &FileSystem, ks_p: StoreProcess<EntryStub>) -> usize {
        let listing = match ks_p.send_reply(Msg::ListDir(fs.file.key_entry.id)) {
            Reply::ListResult(ls) => ls,
            _ => panic!("Unexpected result from key store."),
        };

        assert_eq!(fs.filelist.len(), listing.len());

        for (entry, persistent_ref, tree_data) in listing {
            let mut found = false;

            for dir in fs.filelist.iter() {
                if dir.file.key_entry.name == entry.name {
                    found = true;

                    assert_eq!(dir.file.key_entry.id, entry.id);
                    assert_eq!(dir.file.key_entry.created, entry.created);
                    assert_eq!(dir.file.key_entry.accessed, entry.accessed);
                    assert_eq!(dir.file.key_entry.modified, entry.modified);

                    match dir.file.data {
                        Some(ref original) => {
                            let it = match tree_data.expect("has data")() {
                                None => panic!("No data."),
                                Some(it) => it,
                            };
                            let mut chunk_count = 0;
                            for (i, chunk) in it.enumerate() {
                                assert_eq!(original.get(i), Some(&chunk));
                                chunk_count += 1;
                            }
                            assert_eq!(original.len(), chunk_count);
                        }
                        None => {
                            assert_eq!(entry.data_hash, None);
                            assert_eq!(persistent_ref, None);
                        }
                    }

                    break;  // Proceed to check next file
                }
            }
            assert_eq!(true, found);
        }

        let mut count = fs.filelist.len();
        for dir in fs.filelist.iter() {
            count += verify_filesystem(dir, ks_p.clone());
        }

        count
    }

    #[test]
    fn identity() {
        fn prop(size: u8) -> bool {
            let backend = MemoryBackend::new();
            let ks_p = Process::new(Box::new(move || Store::new_for_testing(backend)));

            let mut fs = rng_filesystem(size as usize);
            insert_and_update_fs(&mut fs, ks_p.clone());
            let fs = fs;

            match ks_p.send_reply(Msg::Flush) {
                Reply::FlushOk => (),
                _ => panic!("Unexpected result from key store."),
            }

            verify_filesystem(&fs, ks_p.clone());
            true
        }
        quickcheck::quickcheck(prop as fn(u8) -> bool);
    }

    #[bench]
    fn insert_1_key_x_128000_zeros(bench: &mut Bencher) {
        let backend = DevNullBackend;
        let ks_p: StoreProcess<EntryStub> = Process::new(Box::new(move || {
            Store::new_for_testing(backend)
        }));

        let bytes = vec![0u8; 128*1024];

        let mut i = 0i32;
        bench.iter(|| {
            i += 1;

            let entry = EntryStub {
                data: Some(vec![bytes.clone()]),
                key_entry: Entry {
                    parent_id: None,
                    id: None,
                    name: format!("{}", i).as_bytes().to_vec(),
                    created: None,
                    modified: None,
                    accessed: None,
                    group_id: None,
                    user_id: None,
                    permissions: None,
                    data_hash: None,
                    data_length: None,
                },
            };

            ks_p.send_reply(Msg::Insert(entry.key_entry.clone(),
                                        Some(Box::new(move || Some(entry)))));
        });

        bench.bytes = 128 * 1024;

    }

    #[bench]
    fn insert_1_key_x_128000_unique(bench: &mut Bencher) {
        let backend = DevNullBackend;
        let ks_p: StoreProcess<EntryStub> = Process::new(Box::new(move || {
            Store::new_for_testing(backend)
        }));

        let bytes = vec![0u8; 128*1024];

        let mut i = 0i32;
        bench.iter(|| {
            i += 1;

            let mut my_bytes = bytes.clone();
            my_bytes[0] = i as u8;
            my_bytes[1] = (i / 256) as u8;
            my_bytes[2] = (i / 65536) as u8;

            let entry = EntryStub {
                data: Some(vec![my_bytes]),
                key_entry: Entry {
                    parent_id: None,
                    id: None,
                    name: format!("{}", i).as_bytes().to_vec(),
                    created: None,
                    modified: None,
                    accessed: None,
                    group_id: None,
                    user_id: None,
                    permissions: None,
                    data_hash: None,
                    data_length: None,
                },
            };

            ks_p.send_reply(Msg::Insert(entry.key_entry.clone(),
                                        Some(Box::new(move || Some(entry)))));
        });

        bench.bytes = 128 * 1024;

    }


    #[bench]
    fn insert_1_key_x_16_x_128000_zeros(bench: &mut Bencher) {
        let backend = DevNullBackend;
        let ks_p: StoreProcess<EntryStub> = Process::new(Box::new(move || {
            Store::new_for_testing(backend)
        }));

        bench.iter(|| {
            let bytes = vec![0u8; 128*1024];

            let entry = EntryStub {
                data: Some(vec![bytes; 16]),
                key_entry: Entry {
                    parent_id: None,
                    id: None,
                    name: vec![1u8, 2, 3].to_vec(),
                    created: None,
                    modified: None,
                    accessed: None,
                    group_id: None,
                    user_id: None,
                    permissions: None,
                    data_hash: None,
                    data_length: None,
                },
            };
            ks_p.send_reply(Msg::Insert(entry.key_entry.clone(),
                                        Some(Box::new(move || Some(entry)))));

            match ks_p.send_reply(Msg::Flush) {
                Reply::FlushOk => (),
                _ => panic!("Unexpected result from key store."),
            }
        });

        bench.bytes = 16 * (128 * 1024);
    }

    #[bench]
    fn insert_1_key_x_16_x_128000_unique(bench: &mut Bencher) {
        let backend = DevNullBackend;
        let ks_p: StoreProcess<EntryStub> = Process::new(Box::new(move || {
            Store::new_for_testing(backend)
        }));

        let bytes = vec![0u8; 128*1024];
        let mut i = 0i32;

        bench.iter(|| {
            i += 1;

            let mut my_bytes = bytes.clone();
            my_bytes[0] = i as u8;
            my_bytes[1] = (i / 256) as u8;
            my_bytes[2] = (i / 65536) as u8;

            let mut chunks = vec![];
            for i in 0..16 {
                let mut local_bytes = my_bytes.clone();
                local_bytes[3] = i as u8;
                chunks.push(local_bytes);
            }

            let entry = EntryStub {
                data: Some(chunks),
                key_entry: Entry {
                    parent_id: None,
                    id: None,
                    name: vec![1u8, 2, 3],
                    created: None,
                    modified: None,
                    accessed: None,
                    group_id: None,
                    user_id: None,
                    permissions: None,
                    data_hash: None,
                    data_length: None,
                },
            };

            ks_p.send_reply(Msg::Insert(entry.key_entry.clone(),
                                        Some(Box::new(move || Some(entry)))));

            match ks_p.send_reply(Msg::Flush) {
                Reply::FlushOk => (),
                _ => panic!("Unexpected result from key store."),
            }
        });

        bench.bytes = 16 * (128 * 1024);
    }

    #[bench]
    fn insert_1_key_unchanged_empty(bench: &mut Bencher) {
        let backend = DevNullBackend;
        let ks_p: StoreProcess<EntryStub> = Process::new(Box::new(move || {
            Store::new_for_testing(backend)
        }));

        bench.iter(|| {
            let entry = EntryStub {
                data: None,
                key_entry: Entry {
                    parent_id: None,
                    id: None,
                    name: vec![1u8, 2, 3].to_vec(),
                    created: Some(0),
                    modified: Some(0),
                    accessed: Some(0),
                    group_id: None,
                    user_id: None,
                    permissions: None,
                    data_hash: None,
                    data_length: None,
                },
            };
            ks_p.send_reply(Msg::Insert(entry.key_entry.clone(), None));
        });
    }

    #[bench]
    fn insert_1_key_updated_empty(bench: &mut Bencher) {
        let backend = DevNullBackend;
        let ks_p: StoreProcess<EntryStub> = Process::new(Box::new(move || {
            Store::new_for_testing(backend)
        }));

        let mut i = 0;
        bench.iter(|| {
            i += 1;
            let entry = EntryStub {
                data: None,
                key_entry: Entry {
                    parent_id: None,
                    id: None,
                    name: vec![1u8, 2, 3].to_vec(),
                    created: Some(i),
                    modified: Some(i),
                    accessed: Some(i),
                    group_id: None,
                    user_id: None,
                    permissions: None,
                    data_hash: None,
                    data_length: None,
                },
            };
            ks_p.send_reply(Msg::Insert(entry.key_entry.clone(), None));
        });
    }

    #[bench]
    fn insert_1_key_unique_empty(bench: &mut Bencher) {
        let backend = DevNullBackend;
        let ks_p: StoreProcess<EntryStub> = Process::new(Box::new(move || {
            Store::new_for_testing(backend)
        }));

        let mut i = 0;
        bench.iter(|| {
            i += 1;
            let entry = EntryStub {
                data: None,
                key_entry: Entry {
                    parent_id: None,
                    id: None,
                    name: format!("{}", i).as_bytes().to_vec(),
                    created: Some(i),
                    modified: Some(i),
                    accessed: Some(i),
                    group_id: None,
                    user_id: None,
                    permissions: None,
                    data_hash: None,
                    data_length: None,
                },
            };
            ks_p.send_reply(Msg::Insert(entry.key_entry.clone(), None));
        });
    }
}
