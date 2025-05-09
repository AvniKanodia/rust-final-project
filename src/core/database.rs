use rocksdb::{DB, Options};
use blake3;
use std::time::{SystemTime, UNIX_EPOCH};
use crate::core::models::{Commit, Change};
use crate::error::{GitDBError, Result};
use std::sync::Arc;
use std::collections::HashMap;
use crate::core::crdt::CrdtEngine;
use rocksdb::WriteBatch;

pub struct CommitStorage {
    pub db: Arc<DB>,
}

impl CommitStorage {
    pub fn open(path: &str) -> Result<Self> {
        let mut opts = Options::default();
        opts.create_if_missing(true);
        let db = DB::open(&opts, path)?;
        Ok(Self {
            db: Arc::new(db)
        })
    }
    
    pub fn get_commit_by_hash(&self, hash: &[u8; 32]) -> Result<Commit> {
        let raw = self.db.get(hash)?
            .ok_or_else(|| GitDBError::InvalidInput("Commit not found".into()))?;
        bincode::deserialize(&raw).map_err(Into::into)
    }

    pub fn get_head(&self) -> Result<Option<[u8; 32]>> {
        match self.db.get(b"HEAD")? {
            Some(raw) if raw.len() == 32 => {
                let mut bytes = [0u8; 32];
                bytes.copy_from_slice(&raw);
                Ok(Some(bytes))
            }
            Some(_) => Err(GitDBError::InvalidInput("HEAD contains invalid data".into())),
            None => Ok(None),
        }
    }

    pub fn create_commit(&self, message: &str, changes: Vec<Change>) -> Result<[u8; 32]> {
        let parent = self.get_head()?;
        let mut tree = HashMap::new(); 

        // Not sure if this is optimal — might refactor how we store tree structure later
        for c in &changes {
            let table_hash = self.calculate_table_hash(c.table())?;
            tree.insert(c.table().to_string(), table_hash); 
        }

        let commit = Commit {
            parents: parent.into_iter().collect(),
            message: message.to_string(),
            timestamp: SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs(),
            changes,
            tree,
        };

        let serialized = bincode::serialize(&commit)?;
        let hash = blake3::hash(&serialized);
        let hash_bytes: [u8; 32] = *hash.as_bytes();

        let test_deserialize: Commit = bincode::deserialize(&serialized)?;
        if test_deserialize.message != commit.message {
            return Err(GitDBError::CorruptData("Serialization roundtrip failed".into()));
        }

        let checksum = blake3::hash(&serialized);
        let mut protected_value = serialized.clone();
        protected_value.extend_from_slice(checksum.as_bytes());

        self.db.put(&hash_bytes, &protected_value)?;
        
        self.update_head(&hash_bytes)?;
        
        Ok(hash_bytes)
    }

    pub fn revert_to_commit(&self, commit_hash: &[u8; 32]) -> Result<()> {
        let target_commit = self.get_commit_by_hash(commit_hash)?;
        let mut target_engine = CrdtEngine::new();
        let commit_chain = self.load_commit_chain(Some(*commit_hash))?;

        for commit in commit_chain.into_iter().rev() {
            for change in &commit.changes {
                target_engine.apply_change(change)?;
            }
        }

        let mut batch = WriteBatch::default();
        for table in target_commit.tree.keys() {
            let prefix = format!("{}:", table);
            let iter = self.db.prefix_iterator(prefix.as_bytes());
            for item in iter {
                let (key, _) = item?;
                batch.delete(key);
            }
        }

        for (table, rows) in target_engine.into_data() {
            for (id, value) in rows {
                let key = format!("{}:{}", table, id);
                let serialized = bincode::serialize(&value)?;
                batch.put(key.as_bytes(), serialized);
            }
        }

        let revert_changes = target_commit.changes.iter()
            .map(|c| match c {
                Change::Insert { table, id, .. } => Change::Delete {
                    table: table.clone(),
                    id: id.clone(),
                },
                _ => c.clone(),
            })
            .collect();

        self.db.write(batch)?;
        self.create_commit(&format!("Revert to {}", hex::encode(commit_hash)), revert_changes)?;
        Ok(())
    }

    fn calculate_table_hash(&self, table: &str) -> Result<[u8; 32]> {
        let mut hasher = blake3::Hasher::new();
        let mut rows = Vec::new();
        
        let iter = self.db.prefix_iterator(table.as_bytes());
        for result in iter {
            let (key, value) = result?;
            rows.push((key.to_vec(), value.to_vec()));
        }
        
        rows.sort_by(|a: &(Vec<u8>, Vec<u8>), b: &(Vec<u8>, Vec<u8>)| a.0.cmp(&b.0));
        
        for (key, value) in rows {
            hasher.update(&key);
            hasher.update(&value);
        }
        
        Ok(*hasher.finalize().as_bytes())
    }

    pub fn get_commit_diffs(&self, from: &[u8; 32], to: &[u8; 32]) -> Result<Vec<Change>> {
        let from_commit = self.get_commit_by_hash(from)?;
        let to_commit = self.get_commit_by_hash(to)?;
        
        let mut diffs = Vec::new();
        
        for (table, to_hash) in &to_commit.tree {
            if let Some(from_hash) = from_commit.tree.get(table) {
                if from_hash != to_hash {
                    let table_diffs = self.get_table_diffs(table, from, to)?;
                    diffs.extend(table_diffs);
                }
            } else {
                diffs.push(Change::Insert {
                    table: table.clone(),
                    id: "!schema".to_string(),
                    value: vec![],
                });
            }
        }
        Ok(diffs)
    }

    fn update_head(&self, hash: &[u8; 32]) -> Result<()> {
        self.db.put(b"HEAD", hash)?;
        Ok(())
    }

    pub fn get_commit_history(&self) -> Result<Vec<Commit>> {
        self.load_commit_chain(self.get_head()?)
    }

    pub fn get_table_diffs(&self, table: &str, from: &[u8; 32], to: &[u8; 32]) -> Result<Vec<Change>> {
        let from_commit = self.get_commit_by_hash(from)?;
        let to_commit = self.get_commit_by_hash(to)?;
    
        let mut from_engine = CrdtEngine::new();
        let mut to_engine = CrdtEngine::new();
    
        let mut current_hash = from_commit.parents.get(0).cloned();
        while let Some(hash) = current_hash {
            let commit = self.get_commit_by_hash(&hash)?;
            for change in &commit.changes {
                if change.table() == table {
                    from_engine.apply_change(change)?;
                }
            }
            current_hash = commit.parents.get(0).cloned();
        }
    
        let mut current_hash = to_commit.parents.get(0).cloned();
        while let Some(hash) = current_hash {
            let commit = self.get_commit_by_hash(&hash)?;
            for change in &commit.changes {
                if change.table() == table {
                    to_engine.apply_change(change)?;
                }
            }
            current_hash = commit.parents.get(0).cloned();
        }
        let mut diffs = Vec::new();
        let from_rows = from_engine.state.get(table).cloned().unwrap_or_default();
        let to_rows = to_engine.state.get(table).cloned().unwrap_or_default();
    
        for (id, to_val) in &to_rows {
            match from_rows.get(id) {
                Some(from_val) if from_val != to_val => {
                    diffs.push(Change::Update {
                        table: table.to_string(),
                        id: id.clone(),
                        value: bincode::serialize(to_val)?,
                    });
                }
                None => {
                    diffs.push(Change::Insert {
                        table: table.to_string(),
                        id: id.clone(),
                        value: bincode::serialize(to_val)?,
                    });
                }
                _ => {}
            }
        }
        for (id, _) in from_rows {
            if !to_rows.contains_key(&id) {
                diffs.push(Change::Delete {
                    table: table.to_string(),
                    id,
                });
            }
        }
    
        Ok(diffs)
    }

    pub fn debug_commit(&self, hash: &str) -> Result<()> {
        let hash_bytes = hex::decode(hash)?;
        match self.db.get(&hash_bytes)? {
            Some(data) => {
                println!("Commit data ({} bytes):", data.len());
                println!("Hex: {}", hex::encode(&data));
                match bincode::deserialize::<Commit>(&data) {
                    Ok(commit) => println!("Valid commit: {:?}", commit),
                    Err(e) => println!("Deserialization failed: {}", e),
                }
            }
            None => println!("Commit not found"),
        }
        Ok(())
    }

    fn load_commit_chain(&self, mut current_hash: Option<[u8; 32]>) -> Result<Vec<Commit>> {
        let mut history = Vec::new();
        while let Some(hash) = current_hash {
            let commit = self.get_commit_by_hash(&hash)?;
            history.push(commit.clone());
            current_hash = commit.parents.get(0).cloned();
        }
        Ok(history)
    }
}
