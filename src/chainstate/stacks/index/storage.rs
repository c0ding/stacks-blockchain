/*
 copyright: (c) 2013-2019 by Blockstack PBC, a public benefit corporation.

 This file is part of Blockstack.

 Blockstack is free software. You may redistribute or modify
 it under the terms of the GNU General Public License as published by
 the Free Software Foundation, either version 3 of the License or
 (at your option) any later version.

 Blockstack is distributed in the hope that it will be useful,
 but WITHOUT ANY WARRANTY, including without the implied warranty of
 MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 GNU General Public License for more details.

 You should have received a copy of the GNU General Public License
 along with Blockstack. If not, see <http://www.gnu.org/licenses/>.
*/

use std::fmt;
use std::error;
use std::io;
use std::io::{
    Read,
    Write,
    Seek,
    SeekFrom,
    Cursor,
    BufWriter,
};

use std::char::from_digit;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::collections::{VecDeque, HashMap, HashSet};

use std::fs;
use std::path::{
    Path,
    PathBuf
};

use std::os;
use std::iter::FromIterator;

use regex::Regex;

use chainstate::burn::BlockHeaderHash;
use chainstate::burn::BLOCK_HEADER_HASH_ENCODED_SIZE;

use chainstate::stacks::index::{
    TrieHash,
    TRIEHASH_ENCODED_SIZE,
    BlockMap,
    trie_sql,
    MarfTrieId,
};

use chainstate::stacks::index::bits::{
    get_node_byte_len,
    write_nodetype_bytes,
    read_hash_bytes,
    read_block_identifier,
    read_node_hash_bytes,
    read_nodetype,
    get_node_hash,
};

use chainstate::stacks::index::node::{
    is_backptr,
    clear_backptr,
    set_backptr,
    TrieNodeType,
    TrieNode4,
    TrieNode16,
    TrieNode48,
    TrieNode256,
    TrieLeaf,
    TrieNodeID,
    TriePtr,
    TriePath,
    TrieNode
};

use rusqlite::{
    Connection, OptionalExtension,
    types::{ FromSql,
             ToSql },
    NO_PARAMS,
    Error as SqliteError,
    OpenFlags
};

use std::convert::{
    TryFrom,
    TryInto
};

use chainstate::stacks::index::Error as Error;

use util::log;
use util::db::tx_begin_immediate;
use util::db::tx_busy_handler;
use util::db::Error as db_error;

pub fn ftell<F: Seek>(f: &mut F) -> Result<u64, Error> {
    f.seek(SeekFrom::Current(0))
        .map_err(Error::IOError)
}

pub fn fseek<F: Seek>(f: &mut F, off: u64) -> Result<u64, Error> {
    f.seek(SeekFrom::Start(off))
        .map_err(Error::IOError)
}

pub fn fseek_end<F: Seek>(f: &mut F) -> Result<u64, Error> {
    f.seek(SeekFrom::End(0))
        .map_err(Error::IOError)
}

trait NodeHashReader {
    fn read_node_hash_bytes<W: Write>(&mut self, ptr: &TriePtr, w: &mut W) -> Result<(), Error>;
}

impl <T: MarfTrieId> BlockMap for TrieFileStorage<T> {
    type TrieId = T;

    fn get_block_hash(&self, id: u32) -> Result<T, Error> {
        trie_sql::get_block_hash(&self.db, id)
    }

    fn get_block_hash_caching(&mut self, id: u32) -> Result<&T, Error> {
        if !self.block_hash_cache.contains_key(&id) {
            self.block_hash_cache.insert(id, self.get_block_hash(id)?);
        }
        Ok(&self.block_hash_cache[&id])
    }
}

impl <T: MarfTrieId> BlockMap for TrieSqlHashMapCursor<'_, T> {
    type TrieId = T;

    fn get_block_hash(&self, id: u32) -> Result<T, Error> {
        trie_sql::get_block_hash(&self.db, id)
    }

    fn get_block_hash_caching(&mut self, id: u32) -> Result<&T, Error> {
        if !self.cache.contains_key(&id) {
            self.cache.insert(id, self.get_block_hash(id)?);
        }
        Ok(&self.cache[&id])
    }
}

enum FlushOptions<'a, T: MarfTrieId> {
    CurrentHeader,
    NewHeader(&'a T),
    MinedTable(&'a T),
    UnconfirmedTable,
}

impl <T: MarfTrieId> fmt::Display for FlushOptions <'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FlushOptions::CurrentHeader => write!(f, "self"),
            FlushOptions::MinedTable(bhh) => write!(f, "{}.mined", bhh),
            FlushOptions::NewHeader(bhh) => write!(f, "{}", bhh),
            FlushOptions::UnconfirmedTable => write!(f, "self.unconfirmed")
        }
    }
}

/// In-RAM trie storage.
/// Used by TrieFileStorage to buffer the next trie being built.
#[derive(Clone)]
pub struct TrieRAM <T: MarfTrieId> {
    data: Vec<(TrieNodeType, TrieHash)>,
    block_header: T,
    readonly: bool,

    read_count: u64,
    read_backptr_count: u64,
    read_node_count: u64,
    read_leaf_count: u64,

    write_count: u64,
    write_node_count: u64,
    write_leaf_count: u64,

    total_bytes: usize,

    parent: T
}

// Trie in RAM without the serialization overhead
impl <T: MarfTrieId> TrieRAM <T> {
    pub fn new(block_header: &T, capacity_hint: usize, parent: &T) -> TrieRAM<T> {
        TrieRAM {
            data: Vec::with_capacity(capacity_hint),
            block_header: block_header.clone(),
            readonly: false,

            read_count: 0,
            read_backptr_count: 0,
            read_node_count: 0,
            read_leaf_count: 0,

            write_count: 0,
            write_node_count: 0,
            write_leaf_count: 0,

            total_bytes: 0,

            parent: parent.clone(),
        }
    }

    fn from_data(block_header: T, data: Vec<(TrieNodeType, TrieHash)>, parent: T) -> TrieRAM<T> {
        TrieRAM {
            data: data,
            block_header: block_header,
            readonly: false,

            read_count: 0,
            read_backptr_count: 0,
            read_node_count: 0,
            read_leaf_count: 0,

            write_count: 0,
            write_node_count: 0,
            write_leaf_count: 0,

            total_bytes: 0,

            parent: parent
        }
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn stats(&mut self) -> (u64, u64) {
        let r = self.read_count;
        let w = self.write_count;
        self.read_count = 0;
        self.write_count = 0;
        (r, w)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn node_stats(&mut self) -> (u64, u64, u64) {
        let nr = self.read_node_count;
        let br = self.read_backptr_count;
        let nw = self.write_node_count;

        self.read_node_count = 0;
        self.read_backptr_count = 0;
        self.write_node_count = 0;
         
        (nr, br, nw)
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub fn leaf_stats(&mut self) -> (u64, u64) {
        let lr = self.read_leaf_count;
        let lw = self.write_leaf_count;

        self.read_leaf_count = 0;
        self.write_leaf_count = 0;

        (lr, lw)
    }

    pub fn write_trie<F: Write + Seek>(f: &mut F, node_data: &[(TrieNodeType, TrieHash)], offsets: &[u32],
                                       parent_hash: &T) -> Result<(), Error> {
        assert_eq!(node_data.len(), offsets.len());

        // write parent block ptr
        fseek(f, 0)?;
        f.write_all(parent_hash.as_bytes())
            .map_err(|e| Error::IOError(e))?;
        // write zero-identifier (TODO: this is a convenience hack for now, we should remove the
        //    identifier from the trie data blob)
        fseek(f, BLOCK_HEADER_HASH_ENCODED_SIZE as u64)?;
        f.write_all(&0u32.to_le_bytes())
            .map_err(|e| Error::IOError(e))?;

        for i in 0..node_data.len() {
            // dump the node to storage
            write_nodetype_bytes(f, &node_data[i].0, node_data[i].1)?;
            
            // next node
            fseek(f, offsets[i] as u64)?;
        }

        Ok(())
    }

    /// Walk through the bufferred TrieNodes and dump them to f.
    fn dump_traverse<F: Write + Seek>(&mut self, f: &mut F, root: &TrieNodeType, hash: &TrieHash) -> Result<u64, Error> {
        let mut frontier : VecDeque<(TrieNodeType, TrieHash)> = VecDeque::new();

        let mut node_data = vec![];
        let mut offsets = vec![];

        frontier.push_back((root.clone(), hash.clone()));

        // first 32 bytes is reserved for the parent block hash
        //    next 4 bytes is the local block identifier
        let mut ptr = BLOCK_HEADER_HASH_ENCODED_SIZE as u64 +
            4;
        
        // step 1: write out each node in breadth-first order to get their ptr offsets
        while frontier.len() > 0 {
            let (node, node_hash) = match frontier.pop_front() {
                Some((n, h)) => (n, h),
                None => {
                    break;
                }
            };

            // calculate size
            let num_written = get_node_byte_len(&node);
            ptr += num_written as u64;
            
            // queue each child
            if !node.is_leaf() {
                let ptrs = node.ptrs();
                let num_children = ptrs.len();
                for i in 0..num_children {
                    if ptrs[i].id != TrieNodeID::Empty as u8 && !is_backptr(ptrs[i].id) {
                        let (child, child_hash) = self.read_nodetype(&ptrs[i])?;
                        frontier.push_back((child, child_hash));
                    }
                }
            }
            
            node_data.push((node, node_hash));
            offsets.push(ptr as u32);
        }

        assert_eq!(offsets.len(), node_data.len());

        // step 2: update ptrs in all nodes
        let mut i = 0;
        for j in 0..node_data.len() {
            let next_node = &mut node_data[j].0;
            if !next_node.is_leaf() {
                let mut ptrs = next_node.ptrs_mut();
                let num_children = ptrs.len();
                for k in 0..num_children {
                    if ptrs[k].id != TrieNodeID::Empty as u8 && !is_backptr(ptrs[k].id) {
                        ptrs[k].ptr = offsets[i];
                        i += 1;
                    }
                }
            }
        }

        // step 3: write out each node (now that they have the write ptrs)
        TrieRAM::write_trie(f, node_data.as_slice(), offsets.as_slice(), &self.parent)?;

        Ok(ptr)
    }

    /// Dump ourself to f
    pub fn dump<F: Write + Seek>(&mut self, f: &mut F, bhh: &T) -> Result<u64, Error> {
        if self.block_header == *bhh {
            let (root, hash) = self.read_nodetype(&TriePtr::new(TrieNodeID::Node256 as u8, 0, 0))?;
            self.dump_traverse(f, &root, &hash)
        }
        else {
            error!("Failed to dump {:?}: not the current block", bhh);
            Err(Error::NotFoundError)
        }
    }

    /// load the trie from F.
    /// The trie will have the same structure as the on-disk trie, but it may have nodes in a
    /// different order.
    pub fn load<F: Read + Seek>(f: &mut F, bhh: &T) -> Result<TrieRAM<T>, Error> {
        let mut data = vec![];
        let mut frontier = VecDeque::new();

        // read parent
        fseek(f, 0)?;
        let parent_hash_bytes = read_hash_bytes(f)?;
        let parent_hash = T::from_bytes(parent_hash_bytes);

        let root_disk_ptr = BLOCK_HEADER_HASH_ENCODED_SIZE as u64 +
            4;

        let root_ptr = TriePtr::new(TrieNodeID::Node256 as u8, 0, root_disk_ptr as u32);
        let (mut root_node, root_hash) = read_nodetype(f, &root_ptr)
            .map_err(|e| {
                error!("Failed to read root node info for {:?}: {:?}", bhh, &e);
                e
            })?;

        let mut next_index = 1;

        if let TrieNodeType::Node256(ref mut data) = root_node {
            // queue children in the same order we stored them
            for ptr in data.ptrs.iter_mut() {
                if ptr.id() != TrieNodeID::Empty as u8 && !is_backptr(ptr.id()) {
                    frontier.push_back((*ptr).clone());

                    // fix up ptrs
                    ptr.ptr = next_index;
                    next_index += 1;
                }
            }
        }
        else {
            return Err(Error::CorruptionError("First TrieRAM node is not a Node256".to_string()));
        }

        data.push((root_node, root_hash));

        while frontier.len() > 0 {
            let next_ptr = frontier.pop_front().expect("BUG: no ptr in non-empty frontier");
            let (mut next_node, next_hash) = read_nodetype(f, &next_ptr)
                .map_err(|e| {
                    error!("Failed to read node at {:?}: {:?}", &next_ptr, &e);
                    e
                })?;

            if !next_node.is_leaf() {
                // queue children in the same order we stored them
                let ptrs : &mut [TriePtr] = match next_node {
                    TrieNodeType::Node4(ref mut data) => &mut data.ptrs,
                    TrieNodeType::Node16(ref mut data) => &mut data.ptrs,
                    TrieNodeType::Node48(ref mut data) => &mut data.ptrs,
                    TrieNodeType::Node256(ref mut data) => &mut data.ptrs,
                    _ => {
                        unreachable!();
                    }
                };

                for ptr in ptrs {
                    if ptr.id() != TrieNodeID::Empty as u8 && !is_backptr(ptr.id()) {
                        frontier.push_back((*ptr).clone());

                        // fix up ptrs
                        ptr.ptr = next_index;
                        next_index += 1;
                    }
                }
            }

            data.push((next_node, next_hash));
        }

        Ok(TrieRAM::from_data((*bhh).clone(), data, parent_hash))
    }

    fn size_hint(&self) -> usize {
        self.write_count as usize
        // the size hint is used for a capacity guess on the data vec, which is _nodes_
        //  NOT bytes. this led to enormous over-allocations
    }

    pub fn format(&mut self) -> Result<(), Error> {
        if self.readonly {
            trace!("Read-only!");
            return Err(Error::ReadOnlyError);
        }

        self.data.clear();
        Ok(())
    }

    pub fn read_node_hash(&self, ptr: &TriePtr) -> Result<TrieHash, Error> {
        let (_, node_trie_hash) = self.data.get(ptr.ptr() as usize)
            .ok_or_else(|| {
                error!("TrieRAM: Failed to read node bytes: {} >= {}", ptr.ptr(), self.data.len());
                Error::NotFoundError
            })?;

        Ok(node_trie_hash.clone())
    }

    pub fn read_nodetype(&mut self, ptr: &TriePtr) -> Result<(TrieNodeType, TrieHash), Error> {
        trace!("TrieRAM: read_nodetype({:?}): at {:?}", &self.block_header, ptr);

        self.read_count += 1;
        if is_backptr(ptr.id()) {
            self.read_backptr_count += 1;
        }
        else if ptr.id() == TrieNodeID::Leaf as u8 {
            self.read_leaf_count += 1;
        }
        else {
            self.read_node_count += 1;
        }

        if (ptr.ptr() as u64) >= (self.data.len() as u64) {
            error!("TrieRAM read_nodetype({:?}): Failed to read node {:?}: {} >= {}", &self.block_header, ptr, ptr.ptr(), self.data.len());
            Err(Error::NotFoundError)
        }
        else {
            Ok(self.data[ptr.ptr() as usize].clone())
        }
    }

    pub fn write_nodetype(&mut self, node_array_ptr: u32, node: &TrieNodeType, hash: TrieHash) -> Result<(), Error> {
        if self.readonly {
            trace!("Read-only!");
            return Err(Error::ReadOnlyError);
        }

        trace!("TrieRAM: write_nodetype({:?}): at {}: {:?} {:?}", &self.block_header, node_array_ptr, &hash, node);
        
        self.write_count += 1;
        match node {
            TrieNodeType::Leaf(_) => {
                self.write_leaf_count += 1;
            },
            _ => {
                self.write_node_count += 1;
            }
        }

        if node_array_ptr < (self.data.len() as u32) {
            self.data[node_array_ptr as usize] = (node.clone(), hash);
            Ok(())
        }
        else if node_array_ptr == (self.data.len() as u32) {
            self.data.push((node.clone(), hash));
            self.total_bytes += get_node_byte_len(node);
            Ok(())
        }
        else {
            error!("Failed to write node bytes: off the end of the buffer");
            Err(Error::NotFoundError)
        }
    }

    /// Get the next ptr value for a node to store.
    pub fn last_ptr(&mut self) -> Result<u32, Error> {
        Ok(self.data.len() as u32)
    }
}

impl <T: MarfTrieId> NodeHashReader for TrieRAM <T> {
    fn read_node_hash_bytes<W: Write>(&mut self, ptr: &TriePtr, w: &mut W) -> Result<(), Error> {
        let (_, node_trie_hash) = self.data.get(ptr.ptr() as usize)
            .ok_or_else(|| {
                error!("TrieRAM: Failed to read node bytes: {} >= {}", ptr.ptr(), self.data.len());
                Error::NotFoundError
            })?;
        w.write_all(node_trie_hash.as_bytes())?;
        Ok(())
    }
}

pub struct TrieSqlCursor <'a> {
    db: &'a Connection,
    block_id: u32
}

pub struct TrieSqlHashMapCursor <'a, T: MarfTrieId> {
    db: &'a Connection,
    cache: &'a mut HashMap<u32, T>
}

impl NodeHashReader for TrieSqlCursor<'_> {
    fn read_node_hash_bytes<W: Write>(&mut self, ptr: &TriePtr, w: &mut W) -> Result<(), Error> {
        trie_sql::read_node_hash_bytes(self.db, w, self.block_id, ptr)
    }
}

// disk-backed Trie.
// Keeps the last-extended Trie in-RAM and flushes it to disk on either a call to flush() or a call
// to extend_to_block() with a different block header hash.
pub struct TrieFileStorage <T: MarfTrieId> {
    pub db_path: String,

    last_extended: Option<(T, TrieRAM<T>)>,

    db: Connection,
    cur_block: T,
    /// Tracking the row_id for the cur_block. If cur_block == last_extended,
    ///   this value should always be None
    cur_block_id: Option<u32>,

    read_count: u64,
    read_backptr_count: u64,
    read_node_count: u64,
    read_leaf_count: u64,

    write_count: u64,
    write_node_count: u64,
    write_leaf_count: u64,

    pub trie_ancestor_hash_bytes_cache: Option<(T, Vec<TrieHash>)>,

    block_hash_cache: HashMap<u32, T>,

    pub readonly: bool,
    pub unconfirmed: bool,

    // used in testing in order to short-circuit block-height lookups
    //   when the trie struct is tested outside of marf.rs usage
    #[cfg(test)]
    pub test_genesis_block: Option<T>,
}

impl <T: MarfTrieId> TrieFileStorage <T> {
    fn open_opts(db_path: &str, readonly: bool, unconfirmed: bool) -> Result<TrieFileStorage<T>, Error> {
        let mut create_flag = false;
        let open_flags = 
            if db_path != ":memory:" {
                match fs::metadata(db_path) {
                    Err(e) => {
                        if e.kind() == io::ErrorKind::NotFound {
                            // need to create 
                            if !readonly {
                                create_flag = true;
                                OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
                            }
                            else {
                                return Err(Error::NotFoundError);
                            }
                        }
                        else {
                            return Err(Error::IOError(e));
                        }
                    },
                    Ok(_md) => {
                        // can just open 
                        if !readonly {
                            OpenFlags::SQLITE_OPEN_READ_WRITE
                        }
                        else {
                            OpenFlags::SQLITE_OPEN_READ_ONLY
                        }
                    }
                }
            }
            else {
                create_flag = true;
                if !readonly {
                    OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE
                }
                else {
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_CREATE
                }
            };

        let mut db = Connection::open_with_flags(db_path, open_flags)?;
        db.busy_handler(Some(tx_busy_handler))?;

        let db_path = db_path.to_string();

        if create_flag {
            trie_sql::create_tables_if_needed(&mut db)?;
        }

        debug!("Opened TrieFileStorage {};", db_path);

        let ret = TrieFileStorage {
            db_path,
            db,

            last_extended: None,
            cur_block: TrieFileStorage::block_sentinel(),
            cur_block_id: None,
            
            read_count: 0,
            read_backptr_count: 0,
            read_node_count: 0,
            read_leaf_count: 0,

            write_count: 0,
            write_node_count: 0,
            write_leaf_count: 0,

            trie_ancestor_hash_bytes_cache: None,
            block_hash_cache: HashMap::new(),
  
            readonly: readonly,
            unconfirmed: unconfirmed,

            // used in testing in order to short-circuit block-height lookups
            //   when the trie struct is tested outside of marf.rs usage
            #[cfg(test)]
            test_genesis_block: None,
        };

        Ok(ret)
    }

    pub fn open(db_path: &str) -> Result<TrieFileStorage<T>, Error> {
        TrieFileStorage::open_opts(db_path, false, false)
    }

    pub fn open_readonly(db_path: &str) -> Result<TrieFileStorage<T>, Error> {
        TrieFileStorage::open_opts(db_path, true, false)
    }

    pub fn open_unconfirmed(db_path: &str) -> Result<TrieFileStorage<T>, Error> {
        TrieFileStorage::open_opts(db_path, false, true)
    }

    pub fn reopen_readonly(&self) -> Result<TrieFileStorage<T>, Error> {
        if let Some((ref block_bhh, _)) = self.last_extended {
            error!("MARF storage already opened to in-progress block {}", block_bhh);
            return Err(Error::InProgressError);
        }

        let db = Connection::open_with_flags(&self.db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        db.busy_handler(Some(tx_busy_handler))?;

        trace!("Make read-only view of TrieFileStorage: {}", &self.db_path);
        
        let ret = TrieFileStorage {
            db_path: self.db_path.clone(),
            db: db,

            last_extended: None,
            cur_block: self.cur_block.clone(),
            cur_block_id: self.cur_block_id.clone(),
            
            read_count: 0,
            read_backptr_count: 0,
            read_node_count: 0,
            read_leaf_count: 0,

            write_count: 0,
            write_node_count: 0,
            write_leaf_count: 0,

            trie_ancestor_hash_bytes_cache: None,
            block_hash_cache: self.block_hash_cache.clone(),
  
            readonly: true,
            unconfirmed: true,
            
            // used in testing in order to short-circuit block-height lookups
            //   when the trie struct is tested outside of marf.rs usage
            #[cfg(test)]
            test_genesis_block: self.test_genesis_block.clone()
        };

        Ok(ret)
    }

    pub fn set_cached_ancestor_hashes_bytes(&mut self, bhh: &T, bytes: Vec<TrieHash>) {
        self.trie_ancestor_hash_bytes_cache = Some((bhh.clone(), bytes));
    }

    pub fn clear_cached_ancestor_hashes_bytes(&mut self) {
        self.trie_ancestor_hash_bytes_cache = None;
    }

    pub fn check_cached_ancestor_hashes_bytes(&mut self, bhh: &T) -> Option<Vec<TrieHash>> {
        if let Some((ref cached_bhh, ref cached_bytes)) = self.trie_ancestor_hash_bytes_cache {
            if cached_bhh == bhh {
                return Some(cached_bytes.clone())
            }
        }
        None
    }

    #[cfg(test)]
    pub fn new_memory() -> Result<TrieFileStorage<T>, Error> {
        TrieFileStorage::open(":memory:")
    }

    /// Get the block hash of the "parent of the root".  This does not correspond to a real block,
    /// but instead is a sentinel value that is all 1's
    pub fn block_sentinel() -> T {
        T::sentinel()
    }

    #[cfg(test)]
    pub fn stats(&mut self) -> (u64, u64) {
        let r = self.read_count;
        let w = self.write_count;
        self.read_count = 0;
        self.write_count = 0;
        (r, w)
    }
    
    #[cfg(test)]
    pub fn node_stats(&mut self) -> (u64, u64, u64) {
        let nr = self.read_node_count;
        let br = self.read_backptr_count;
        let nw = self.write_node_count;

        self.read_node_count = 0;
        self.read_backptr_count = 0;
        self.write_node_count = 0;

        (nr, br, nw)
    }

    #[cfg(test)]
    pub fn leaf_stats(&mut self) -> (u64, u64) {
        let lr = self.read_leaf_count;
        let lw = self.write_leaf_count;

        self.read_leaf_count = 0;
        self.write_leaf_count = 0;

        (lr, lw)
    }

    /// Recover from partially-written state -- i.e. blow it away.
    /// Doesn't get called automatically.
    pub fn recover(db_path: &String) -> Result<(), Error> {
        let conn = Connection::open(db_path)?;
        conn.busy_handler(Some(tx_busy_handler))?;

        trie_sql::clear_lock_data(&conn)
    }

    /// Read the Trie root node's hash from the block table.
    #[cfg(test)]
    pub fn read_block_root_hash(&self, bhh: &T) -> Result<TrieHash, Error> {
        let root_hash_ptr =
            TriePtr::new(TrieNodeID::Node256 as u8, 0, TrieFileStorage::<T>::root_ptr_disk());
        trie_sql::get_node_hash_bytes_by_bhh(&self.db, bhh, &root_hash_ptr)
    }

    /// Generate a mapping between Trie root hashes and the blocks that contain them
    #[cfg(test)]
    pub fn read_root_to_block_table(&mut self) -> Result<HashMap<TrieHash, T>, Error> {
        let mut ret = HashMap::from_iter(trie_sql::read_all_block_hashes_and_roots(&self.db)?
                                         .into_iter());

        let last_extended = match self.last_extended.take() {
            Some((bhh, trie_ram)) => {
                let ptr = TriePtr::new(set_backptr(TrieNodeID::Node256 as u8), 0, 0);

                let root_hash = trie_ram.read_node_hash(&ptr)?;

                ret.insert(root_hash.clone(), bhh.clone());
                Some((bhh, trie_ram))
            },
            _ => {
                None
            }
        };

        self.last_extended = last_extended;

        Ok(ret)
    }

    /// internal procedure for locking a trie hash for work
    fn switch_trie(&mut self, bhh: &T, trie_buf: TrieRAM<T>) {
        trace!("Extended from {} to {}", &self.cur_block, bhh);

        // update internal structures
        self.cur_block = bhh.clone();
        self.cur_block_id = None;
        self.clear_cached_ancestor_hashes_bytes();

        self.last_extended = Some((bhh.clone(), trie_buf));
    }

    /// Extend the forest of Tries to include a new confirmed block.
    /// Fails if the block already exists, or if the storage is read-only, or open
    /// only for unconfirmed state.
    pub fn extend_to_block(&mut self, bhh: &T) -> Result<(), Error> {
        self.clear_cached_ancestor_hashes_bytes();
        if self.readonly {
            return Err(Error::ReadOnlyError);
        }
        if self.unconfirmed {
            return Err(Error::UnconfirmedError);
        }
        
        if trie_sql::get_block_identifier(&self.db, bhh).is_ok() {
            warn!("Block already exists: {}", &bhh);
            return Err(Error::ExistsError);
        }

        self.flush()?;

        let size_hint = match self.last_extended {
            Some((_, ref trie_storage)) => 2*trie_storage.size_hint(),
            None => (1024) // don't try to guess _byte_ allocation here.
        };

        let trie_buf = TrieRAM::new(bhh, size_hint, &self.cur_block);
        
        // place a lock on this block, so we can't extend to it again
        if !trie_sql::lock_bhh_for_extension(&mut self.db, bhh, false)? {
            warn!("Block already extended: {}", &bhh);
            return Err(Error::ExistsError);
        }

        self.switch_trie(bhh, trie_buf);
        Ok(())
    }

    /// Extend the forest of Tries to include a new unconfirmed block.
    /// If the unconfirmed block (bhh) already exists, then load up its trie as the last_extended
    /// trie.
    pub fn extend_to_unconfirmed_block(&mut self, bhh: &T) -> Result<bool, Error> {
        self.clear_cached_ancestor_hashes_bytes();
        if !self.unconfirmed {
            return Err(Error::UnconfirmedError);
        }

        self.flush()?;

        // try to load up the trie
        let mut tx = tx_begin_immediate(&mut self.db)?;
        let (trie_buf, created) = 
            if let Some(block_id) = trie_sql::get_unconfirmed_block_identifier(&mut tx, bhh)? {
                debug!("Reload unconfirmed trie {} ({})", bhh, block_id);

                // restore trie
                let mut fd = trie_sql::open_trie_blob(&tx, block_id)?;
                (TrieRAM::load(&mut fd, bhh)?, false)
            }
            else {
                debug!("Instantiate unconfirmed trie {}", bhh);
                
                // new trie
                let size_hint = match self.last_extended {
                    Some((_, ref trie_storage)) => 2*trie_storage.size_hint(),
                    None => (1024) // don't try to guess _byte_ allocation here.
                };

                (TrieRAM::new(bhh, size_hint, &self.cur_block), true)
            };

        // place a lock on this block, so we can't extend to it again
        if !trie_sql::tx_lock_bhh_for_extension(&mut tx, bhh, true)? {
            warn!("Block already extended: {}", &bhh);
            return Err(Error::ExistsError);
        }

        tx.commit()?;

        self.switch_trie(bhh, trie_buf);
        Ok(created)
    }

    pub fn has_confirmed_block(&self, bhh: &T) -> Result<bool, Error> {
        match trie_sql::get_confirmed_block_identifier(&self.db, bhh) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(e) => Err(e)
        }
    }

    pub fn has_unconfirmed_block(&self, bhh: &T) -> Result<bool, Error> {
        match trie_sql::get_unconfirmed_block_identifier(&self.db, bhh) {
            Ok(Some(_)) => Ok(true),
            Ok(None) => Ok(false),
            Err(e) => Err(e)
        }
    }

    pub fn has_block(&self, bhh: &T) -> Result<bool, Error> {
        Ok(self.has_confirmed_block(bhh)? || self.has_unconfirmed_block(bhh)?)
    }

    // used for providing a option<block identifier> when re-opening a block --
    //   because the previously open block may have been the last_extended block,
    //   id may have been None.
    pub fn open_block_maybe_id(&mut self, bhh: &T, id: Option<u32>) -> Result<(), Error> {
        match id {
            Some(id) => self.open_block_known_id(bhh, id),
            None => self.open_block(bhh)
        }
    }

    // used for providing a block identifier when opening a block -- usually used
    //   when following a backptr, which stores the block identifier directly.
    pub fn open_block_known_id(&mut self, bhh: &T, id: u32) -> Result<(), Error> {
        if *bhh == self.cur_block && self.cur_block_id.is_some() {
            // no-op
            return Ok(())
        }

        if let Some((ref last_extended, _)) = self.last_extended {
            if last_extended == bhh {
                panic!("BUG: passed id of a currently building block");
            }
        }

        // opening a different Trie than the one we're extending
        self.cur_block_id = Some(id);
        self.cur_block = bhh.clone();

        Ok(())
    }

    pub fn open_block(&mut self, bhh: &T) -> Result<(), Error> {
        if *bhh == self.cur_block && self.cur_block_id.is_some() {
            // no-op
            return Ok(())
        }

        let sentinel = TrieFileStorage::block_sentinel();
        if *bhh == sentinel {
            // just reset to newly opened state
            self.cur_block = sentinel;
            // did we write to the sentinel?
            self.cur_block_id = trie_sql::get_block_identifier(&self.db, bhh)
                .ok();
            return Ok(());
        }

        if let Some((ref last_extended, _)) = self.last_extended {
            if last_extended == bhh {
                // nothing to do -- we're already ready.
                // just clear out.
                self.cur_block_id = None;
                self.cur_block = bhh.clone();
                return Ok(());
            }
        }

        // opening a different Trie than the one we're extending
        self.cur_block_id = Some(trie_sql::get_block_identifier(&self.db, bhh)?);
        self.cur_block = bhh.clone();

        Ok(())
    }

    /// Return the block_identifier / row_id for a given bhh. If that bhh
    ///  is currently being extended, return None, since the row_id won't
    ///  be known until the extended trie is flushed.
    pub fn get_block_identifier(&self, bhh: &T) -> Option<u32> {
        if let Some((ref last_extended, _)) = self.last_extended {
            if bhh == last_extended {
                return None
            }
        }

        trie_sql::get_block_identifier(&self.db, bhh).ok()
    }

    pub fn get_cur_block_identifier(&mut self) -> Result<u32, Error> {
        if let Some((ref last_extended, _)) = self.last_extended {
            if &self.cur_block == last_extended {
                return Err(Error::RequestedIdentifierForExtensionTrie)
            }
        }

        self.cur_block_id.ok_or_else(|| {
            Error::NotOpenedError
        })
    }
    
    pub fn get_cur_block(&self) -> T {
        self.cur_block.clone()
    }

    pub fn get_cur_block_and_id(&self) -> (T, Option<u32>) {
        (self.cur_block.clone(), self.cur_block_id.clone())
    }

    pub fn get_block_from_local_id(&mut self, local_id: u32) -> Result<&T, Error> {
        self.get_block_hash_caching(local_id)
    }

    pub fn root_ptr(&self) -> u32 {
        if let Some((ref last_extended, _)) = self.last_extended {
            if &self.cur_block == last_extended {
                return 0
            }
        }

        TrieFileStorage::<T>::root_ptr_disk()
    }

    pub fn root_trieptr(&self) -> TriePtr {
        TriePtr::new(TrieNodeID::Node256 as u8, 0, self.root_ptr())
    }

    pub fn root_ptr_disk() -> u32 {
        // first 32 bytes are the block parent hash 
        //   next 4 are the identifier
        (BLOCK_HEADER_HASH_ENCODED_SIZE as u32) + 4
    }

    pub fn format(&mut self) -> Result<(), Error> {
        if self.readonly {
            return Err(Error::ReadOnlyError);
        }

        debug!("Format TrieFileStorage {}", &self.db_path);

        // blow away db
        trie_sql::clear_tables(&mut self.db)?;

        match self.last_extended {
            Some((_, ref mut trie_storage)) => trie_storage.format()?,
            None => {}
        };

        self.cur_block = TrieFileStorage::block_sentinel();
        self.cur_block_id = None;
        self.last_extended = None;
        self.clear_cached_ancestor_hashes_bytes();

        Ok(())
    }

    /// Read a node's children's hashes into the provided <Write> implementation.
    /// This only works for intermediate nodes and leafs (the latter of which have no children).
    ///
    /// This method is designed to only access hashes that are either (1) in this Trie, or (2) in
    /// RAM already (i.e. as part of the block map)
    ///
    /// This means that the hash of a node that is in a previous Trie will _not_ be its
    /// hash (as that would require a disk access), but would instead be the root hash of the Trie
    /// that contains it.  While this makes the Merkle proof construction a bit more complicated,
    /// it _significantly_ improves the performance of this method (which is crucial since this is on
    /// the write path, which must be as short as possible).
    ///
    /// Rules:
    /// If a node is empty, pass in an empty hash.
    /// If a node is in this Trie, pass its hash.
    /// If a node is in a previous Trie, pass the root hash of its Trie.
    ///
    /// On err, S may point to a prior block.  The caller should call s.open(...) if an error
    /// occurs.
    pub fn write_children_hashes<W: Write>(&mut self, node: &TrieNodeType, w: &mut W) -> Result<(), Error> {
        if self.readonly {
            return Err(Error::ReadOnlyError);
        }

        trace!("get_children_hashes_bytes for {:?}", node);

        let mut map = TrieSqlHashMapCursor { db: &self.db,
                                             cache: &mut self.block_hash_cache };

        if let Some((ref last_extended, ref mut last_extended_trie)) = self.last_extended {
            if &self.cur_block == last_extended {
                let hash_reader = last_extended_trie;
                return TrieFileStorage::<T>::inner_write_children_hashes(hash_reader, &mut map, node, w)
            }
        }

        // otherwise, the current block is open as an FD
        let mut cursor = TrieSqlCursor { db: &self.db,
                                         block_id: self.cur_block_id.ok_or_else(|| {
                                             error!("Failed to get cur block as hash reader");
                                             Error::NotFoundError
                                         })? };

        TrieFileStorage::<T>::inner_write_children_hashes(&mut cursor, &mut map, node, w)
    }

    fn inner_write_children_hashes<W: Write, H: NodeHashReader, M: BlockMap>(
        hash_reader: &mut H, map: &mut M, node: &TrieNodeType, w: &mut W) -> Result<(), Error> {
        for ptr in node.ptrs().iter() {
            if ptr.id() == TrieNodeID::Empty as u8 {
                // hash of empty string
                w.write_all(TrieHash::from_data(&[]).as_bytes())?;
            }
            else if !is_backptr(ptr.id()) {
                // hash is in the same block as this node
                hash_reader.read_node_hash_bytes(ptr, w)?;
            }
            else {
                // AARON:
                //   I *think* this is no longer necessary in the fork-table-less construction.
                //   the back_pointer's consensus bytes uses this block_hash instead of a back_block
                //   integer. This means that it would _always_ be included the node's hash computation.
                let block_hash = map.get_block_hash_caching(ptr.back_block())?;
                w.write_all(block_hash.as_bytes())?;
            }
        }

        Ok(())
    }

    pub fn read_node_hash_bytes(&mut self, ptr: &TriePtr) -> Result<TrieHash, Error> {
        if let Some((ref last_extended, ref mut trie_ram)) = self.last_extended {
            // special case 
            if &self.cur_block == last_extended {
                return trie_ram.read_node_hash(ptr)
            }
        }

        // some other block or ptr, or cache miss
        match self.cur_block_id {
            Some(block_id) => {
                trie_sql::get_node_hash_bytes(&self.db, block_id, ptr)
            },
            None => {
                error!("Not found (no file is open)");
                Err(Error::NotFoundError)
            }
        }
    }

    // NOTE: ptr will not be treated as a backptr
    pub fn read_nodetype(&mut self, ptr: &TriePtr) -> Result<(TrieNodeType, TrieHash), Error> {
        trace!("read_nodetype({:?}): {:?}", &self.cur_block, ptr);

        self.read_count += 1;
        if is_backptr(ptr.id()) {
            self.read_backptr_count += 1;
        }
        else if ptr.id() == TrieNodeID::Leaf as u8 {
            self.read_leaf_count += 1;
        }
        else {
            self.read_node_count += 1;
        }

        let clear_ptr = ptr.from_backptr();

        if let Some((ref last_extended, ref mut trie_storage)) = self.last_extended {
            // special case 
            if &self.cur_block == last_extended {
                return trie_storage.read_nodetype(&clear_ptr)
            }
        }

        // some other block
        match self.cur_block_id {
            Some(id) => trie_sql::read_node_type(&self.db, id, &clear_ptr),
            None => {
                error!("Not found (no file is open)");
                Err(Error::NotFoundError)
            }
        }
    }
    
    pub fn write_nodetype(&mut self, disk_ptr: u32, node: &TrieNodeType, hash: TrieHash) -> Result<(), Error> {
        if self.readonly {
            return Err(Error::ReadOnlyError);
        }

        trace!("write_nodetype({:?}): at {}: {:?} {:?}", &self.cur_block, disk_ptr, &hash, node);

        self.write_count += 1;
        match node {
            TrieNodeType::Leaf(_) => {
                self.write_leaf_count += 1;
            },
            _ => {
                self.write_node_count += 1;
            }
        }

        // Only allow writes when the cur_block is the current in-RAM extending block.
        if let Some((ref last_extended, ref mut trie_storage)) = self.last_extended {
            if &self.cur_block == last_extended {
                return trie_storage.write_nodetype(disk_ptr, node, hash)
            }
        }

        panic!("Tried to write to another Trie besides the currently-bufferred one.  This should never happen -- only flush() can write to disk!");
    }

    pub fn write_node<N: TrieNode + std::fmt::Debug>(&mut self, ptr: u32, node: &N, hash: TrieHash) -> Result<(), Error> {
        if self.readonly {
            return Err(Error::ReadOnlyError);
        }

        let node_type = node.as_trie_node_type();
        self.write_nodetype(ptr, &node_type, hash)
    }
    
    fn inner_flush(&mut self, flush_options: FlushOptions<'_, T>) -> Result<(), Error> {
        // save the currently-bufferred Trie to disk, and atomically put it into place (possibly to
        // a different block than the one opened, as indicated by final_bhh).
        // Runs once -- subsequent calls are no-ops.
        // Panics on a failure to rename the Trie file into place (i.e. if the the actual commitment
        // fails).
        // TODO: this needs to be more robust.  Also fsync the parent directory itself, before and
        // after.  Turns out rename(2) isn't crash-consistent, and turns out syscalls can get
        // reordered.
        self.clear_cached_ancestor_hashes_bytes();
        if self.readonly {
            return Err(Error::ReadOnlyError);
        }
        if let Some((ref bhh, ref mut trie_ram)) = self.last_extended.take() {
            trace!("Buffering block flush started.");
            let mut buffer = Cursor::new(Vec::new());
            trie_ram.dump(&mut buffer, bhh)?;
            // consume the cursor, get the buffer
            let buffer = buffer.into_inner();
            trace!("Buffering block flush finished.");

            debug!("Flush: {} to {}", bhh, flush_options);
            
            let tx = tx_begin_immediate(&mut self.db)?;
            let block_id = match flush_options {
                FlushOptions::CurrentHeader => {
                    if self.unconfirmed {
                        return Err(Error::UnconfirmedError);
                    }
                    trie_sql::write_trie_blob(&tx, bhh, &buffer)?
                },
                FlushOptions::NewHeader(real_bhh) => {
                    // If we opened a block with a given hash, but want to store it as a block with a *different*
                    // hash, then call this method to update the internal storage state to make it so.  This is
                    // necessary for validating blocks in the blockchain, since the miner will always build a
                    // block whose hash is all 0's (since it can't know the final block hash).  As such, a peer
                    // will process a block as if it's hash is all 0's (in order to validate the state root), and
                    // then use this method to switch over the block hash to the "real" block hash.
                    if self.unconfirmed {
                        return Err(Error::UnconfirmedError);
                    }
                    if real_bhh != bhh {
                        // note: this was moved from the block_retarget function
                        //  to avoid stepping on the borrow checker.
                        debug!("Retarget block {} to {}", bhh, real_bhh);
                        // switch over state
                        self.cur_block = real_bhh.clone();
                    }
                    trie_sql::write_trie_blob(&tx, real_bhh, &buffer)?
                },
                FlushOptions::MinedTable(real_bhh) => {
                    if self.unconfirmed {
                        return Err(Error::UnconfirmedError);
                    }
                    trie_sql::write_trie_blob_to_mined(&tx, real_bhh, &buffer)?
                },
                FlushOptions::UnconfirmedTable => {
                    if !self.unconfirmed {
                        return Err(Error::UnconfirmedError);
                    }
                    trie_sql::write_trie_blob_to_unconfirmed(&tx, bhh, &buffer)?
                }
            };

            trie_sql::drop_lock(&tx, bhh)?;
            tx.commit()?;

            debug!("Flush: identifier of {} is {}", flush_options, block_id);
        }

        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), Error> {
        if self.unconfirmed {
            self.inner_flush(FlushOptions::UnconfirmedTable)
        }
        else {
            self.inner_flush(FlushOptions::CurrentHeader)
        }
    }

    pub fn flush_to(&mut self, bhh: &T) -> Result<(), Error> {
        self.inner_flush(FlushOptions::NewHeader(bhh))
    }

    pub fn flush_mined(&mut self, bhh: &T) -> Result<(), Error> {
        self.inner_flush(FlushOptions::MinedTable(bhh))
    }

    pub fn drop_extending_trie(&mut self) {
        self.clear_cached_ancestor_hashes_bytes();
        if !self.readonly {
            if let Some((ref bhh, _)) = self.last_extended.take() {
                let tx = tx_begin_immediate(&mut self.db)
                    .expect("Corruption: Failed to obtain db transaction");
                trie_sql::drop_lock(&tx, bhh)
                    .expect("Corruption: Failed to drop the extended trie lock");
                tx.commit()
                    .expect("Corruption: Failed to drop the extended trie");
            }
            self.last_extended = None;
            self.cur_block_id = None;
            self.trie_ancestor_hash_bytes_cache = None;
        }
    }

    pub fn drop_unconfirmed_trie(&mut self, bhh: &T) {
        self.clear_cached_ancestor_hashes_bytes();
        if !self.readonly && self.unconfirmed {
            let tx = tx_begin_immediate(&mut self.db)
                .expect("Corruption: Failed to obtain db transaction");
            trie_sql::drop_unconfirmed_trie(&tx, bhh)
                .expect("Corruption: Failed to drop unconfirmed trie");
            trie_sql::drop_lock(&tx, bhh)
                .expect("Corruption: Failed to drop the extended trie lock");
            tx.commit()
                .expect("Corruption: Failed to drop the extended trie");
            self.last_extended = None;
            self.cur_block_id = None;
            self.trie_ancestor_hash_bytes_cache = None;
        }
    }

    pub fn last_ptr(&mut self) -> Result<u32, Error> {
        if let Some((_, ref mut trie_ram)) = self.last_extended {
            trie_ram.last_ptr()
        }
        else {
            panic!("Cannot allocate new ptrs in a Trie that is not in RAM");
        }
    }

    pub fn num_blocks(&self) -> usize {
        let result = if self.last_extended.is_some() {
            1
        } else {
            0
        };
        result + (trie_sql::count_blocks(&self.db)
                  .expect("Corruption: SQL Error on a non-fallible query.") as usize)
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use std::fs;
    use std::collections::VecDeque;

    use chainstate::stacks::index::*;
    use chainstate::stacks::index::node::*;
    use chainstate::stacks::index::marf::*;

    fn ptrs_cmp(p1: &[TriePtr], p2: &[TriePtr]) -> bool {
        if p1.len() != p2.len() {
            return false;
        }
        for (ptr1, ptr2) in p1.iter().zip(p2.iter()) {
            if ptr1.chr != ptr2.chr || ptr1.id != ptr2.id {
                return false;
            }
        }
        return true;
    }

    fn node_cmp(n1: &TrieNodeType, n2: &TrieNodeType) -> bool {
        match (n1, n2) {
            (TrieNodeType::Leaf(ref data1), TrieNodeType::Leaf(ref data2)) => (data1.path == data2.path && data1.data == data2.data),
            (TrieNodeType::Node4(ref data1), TrieNodeType::Node4(ref data2)) => (data1.path == data2.path && ptrs_cmp(&data1.ptrs, &data2.ptrs)),
            (TrieNodeType::Node16(ref data1), TrieNodeType::Node16(ref data2)) => (data1.path == data2.path && ptrs_cmp(&data1.ptrs, &data2.ptrs)),
            (TrieNodeType::Node48(ref data1), TrieNodeType::Node48(ref data2)) => (data1.path == data2.path && ptrs_cmp(&data1.ptrs, &data2.ptrs)),
            (TrieNodeType::Node256(ref data1), TrieNodeType::Node256(ref data2)) => (data1.path == data2.path && ptrs_cmp(&data1.ptrs, &data2.ptrs)),
            (_, _) => false
        }
    }
    
    fn trie_print<T: MarfTrieId>(t: &mut TrieRAM<T>) {
        for dat in t.data.iter() {
            eprintln!("{}: {:?}", &dat.1, &dat.0);
        }
    }

    fn trie_cmp<T: MarfTrieId>(t1: &mut TrieRAM<T>, t2: &mut TrieRAM<T>) -> bool {
        eprintln!("Begin comparing tries\nTrie 1:");
        trie_print(t1);
        eprintln!("Trie 2");
        trie_print(t2);
        eprintln!("End tries\n");

        let mut frontier_1 = VecDeque::new();
        let mut frontier_2 = VecDeque::new();

        assert!(t1.data.len() > 0);
        assert!(t2.data.len() > 0);

        let (n1_data, n1_hash) = t1.data[0].clone();
        let (n2_data, n2_hash) = t2.data[0].clone();

        if let TrieNodeType::Node256(_) = n1_data {} else { assert!(false) }
        if let TrieNodeType::Node256(_) = n2_data {} else { assert!(false) }

        frontier_1.push_back((n1_data, n1_hash));
        frontier_2.push_back((n2_data, n2_hash));

        while frontier_1.len() > 0 && frontier_2.len() > 0 {
            if frontier_1.len() != frontier_2.len() {
                debug!("frontier len mismatch");
                return false;
            }

            let (n1_data, n1_hash) = frontier_1.pop_front().unwrap();
            let (n2_data, n2_hash) = frontier_2.pop_front().unwrap();

            if n1_hash != n2_hash {
                debug!("root hash mismatch: {} != {}", &n1_hash, &n2_hash);
                return false;
            }

            if !node_cmp(&n1_data, &n2_data) {
                debug!("root node mismatch: {:?} != {:?}", &n1_data, &n2_data);
                return false;
            }

            // search children
            for ptr in n1_data.ptrs() {
                if ptr.id != TrieNodeID::Empty as u8 && !is_backptr(ptr.id) {
                    let (child_data, child_hash) = t1.read_nodetype(&ptr).unwrap();
                    frontier_1.push_back((child_data, child_hash))
                }
            }
            for ptr in n2_data.ptrs() {
                if ptr.id != TrieNodeID::Empty as u8 && !is_backptr(ptr.id) {
                    let (child_data, child_hash) = t2.read_nodetype(&ptr).unwrap();
                    frontier_2.push_back((child_data, child_hash))
                }
            }
        }

        return true;
    }

    fn load_store_trie_m_n_same(m: u64, n: u64, same: bool) {
        let test_name = format!("/tmp/load_store_trie_{}_{}_{}", m, n, if same { "same" } else { "unique" });
        if fs::metadata(&test_name).is_ok() {
            fs::remove_file(&test_name).unwrap();
        }

        let confirmed_marf_storage = TrieFileStorage::<StacksBlockId>::open(&test_name).unwrap();
        let mut confirmed_marf = MARF::<StacksBlockId>::from_storage(confirmed_marf_storage);

        confirmed_marf.begin(&TrieFileStorage::block_sentinel(), &StacksBlockId([0x02; 32])).unwrap();

        // pre-populate
        for i in 0..n {
            let mut path_bytes = [0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31];
            path_bytes[24..32].copy_from_slice(&i.to_be_bytes());

            let path = TriePath::from_bytes(&path_bytes).unwrap();
            let value = TrieLeaf::new(&vec![], &[i as u8; 40].to_vec());
            confirmed_marf.insert_raw(path.clone(), value).unwrap();
        }

        let confirmed_tip = StacksBlockId([0x01; 32]);
        confirmed_marf.commit_to(&confirmed_tip).unwrap();

        let marf_storage = TrieFileStorage::<StacksBlockId>::open_unconfirmed(&test_name).unwrap();
        let mut marf = MARF::from_storage(marf_storage);

        let mut last_trie = None;

        let mut all_new_paths = vec![];

        // instantiate unconfirmed m times
        for j in 0..m {
            let unconfirmed_tip = marf.begin_unconfirmed(&confirmed_tip).unwrap();
            let mut new_inserted = vec![];
            
            if let Some(mut trie) = last_trie.take() {
                if let Some((_, ref mut loaded_trie)) = marf.borrow_storage_backend().last_extended {
                    assert!(trie_cmp(loaded_trie, &mut trie));
                }
            }

            // pre-populated keys are present
            for i in 0..n {
                let mut path_bytes = [0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31];
                path_bytes[24..32].copy_from_slice(&i.to_be_bytes());

                let path = TriePath::from_bytes(&path_bytes).unwrap();

                // NOTE: may have been overwritten; just check for presence
                assert!(MARF::get_path(marf.borrow_storage_backend(), &unconfirmed_tip, &path).unwrap().is_some());
            }

            // insert new keys
            for i in 0..n {
                // NOTE: may overwrite prepopulated values
                let mut path_bytes = [0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31];
                path_bytes[24..32].copy_from_slice(&i.to_be_bytes());
                if !same {
                    path_bytes[16..24].copy_from_slice(&j.to_be_bytes());
                }

                let path = TriePath::from_bytes(&path_bytes).unwrap();
                let value = TrieLeaf::new(&vec![], &[(i + 128) as u8; 40].to_vec());
                
                new_inserted.push((path.clone(), value.clone()));

                if let Ok(Some(_)) = MARF::get_path(confirmed_marf.borrow_storage_backend(), &confirmed_tip, &path) {
                }
                else {
                    all_new_paths.push(path.clone());
                }

                marf.insert_raw(path, value).unwrap();
            }

            // verify that all new keys are there, off the unconfirmed tip
            for (path, expected_value) in new_inserted.iter() {
                let value = MARF::get_path(marf.borrow_storage_backend(), &unconfirmed_tip, &path).unwrap().unwrap();
                assert_eq!(expected_value.data, value.data);
            }

            last_trie = Some(marf.borrow_storage_backend().last_extended.clone().unwrap().1);
            marf.commit().unwrap();
        }

        let unconfirmed_tip = MARF::make_unconfirmed_chain_tip(&confirmed_tip);

        // test rollback
        for path in all_new_paths.iter() {
            eprintln!("path present? {:?}", &path);
            assert!(MARF::get_path(marf.borrow_storage_backend(), &unconfirmed_tip, &path).unwrap().is_some());
        }

        marf.drop_unconfirmed();

        for path in all_new_paths.iter() {
            eprintln!("path absent?  {:?}", &path);
            assert!(MARF::get_path(marf.borrow_storage_backend(), &confirmed_tip, &path).is_err());
        }
    }

    #[test]
    fn load_store_trie_4_4_same() {
        load_store_trie_m_n_same(4, 4, true);
    }
    
    #[test]
    fn load_store_trie_4_4_unique() {
        load_store_trie_m_n_same(4, 4, false);
    }
    
    #[test]
    fn load_store_trie_4_16_same() {
        load_store_trie_m_n_same(4, 16, true);
    }
    
    #[test]
    fn load_store_trie_4_16_unique() {
        load_store_trie_m_n_same(4, 16, false);
    }
    
    #[test]
    fn load_store_trie_4_48_same() {
        load_store_trie_m_n_same(4, 48, true);
    }
    
    #[test]
    fn load_store_trie_4_48_unique() {
        load_store_trie_m_n_same(4, 48, false);
    }
    
    #[test]
    fn load_store_trie_4_256_same() {
        load_store_trie_m_n_same(4, 256, true);
    }
    
    #[test]
    fn load_store_trie_4_256_unique() {
        load_store_trie_m_n_same(4, 256, false);
    }
}

