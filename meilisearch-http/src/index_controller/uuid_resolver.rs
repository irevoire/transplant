use std::{fs::create_dir_all, path::Path};

use heed::{
    types::{ByteSlice, Str},
    Database, Env, EnvOpenOptions,
};
use log::{info, warn};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

pub type Result<T> = std::result::Result<T, UuidError>;

#[derive(Debug)]
enum UuidResolveMsg {
    Get {
        uid: String,
        ret: oneshot::Sender<Result<Uuid>>,
    },
    Create {
        uid: String,
        ret: oneshot::Sender<Result<Uuid>>,
    },
    Delete {
        uid: String,
        ret: oneshot::Sender<Result<Uuid>>,
    },
    List {
        ret: oneshot::Sender<Result<Vec<(String, Uuid)>>>,
    },
    Insert {
        uuid: Uuid,
        name: String,
        ret: oneshot::Sender<Result<()>>,
    }
}

struct UuidResolverActor<S> {
    inbox: mpsc::Receiver<UuidResolveMsg>,
    store: S,
}

impl<S: UuidStore> UuidResolverActor<S> {
    fn new(inbox: mpsc::Receiver<UuidResolveMsg>, store: S) -> Self {
        Self { inbox, store }
    }

    async fn run(mut self) {
        use UuidResolveMsg::*;

        info!("uuid resolver started");

        loop {
            match self.inbox.recv().await {
                Some(Create { uid: name, ret }) => {
                    let _ = ret.send(self.handle_create(name).await);
                }
                Some(Get { uid: name, ret }) => {
                    let _ = ret.send(self.handle_get(name).await);
                }
                Some(Delete { uid: name, ret }) => {
                    let _ = ret.send(self.handle_delete(name).await);
                }
                Some(List { ret }) => {
                    let _ = ret.send(self.handle_list().await);
                }
                Some(Insert { ret, uuid, name }) => {
                    let _ = ret.send(self.handle_insert(name, uuid).await);
                }
                // all senders have been dropped, need to quit.
                None => break,
            }
        }

        warn!("exiting uuid resolver loop");
    }

    async fn handle_create(&self, uid: String) -> Result<Uuid> {
        if !is_index_uid_valid(&uid) {
            return Err(UuidError::BadlyFormatted(uid));
        }
        self.store.create_uuid(uid, true).await
    }

    async fn handle_get(&self, uid: String) -> Result<Uuid> {
        self.store
            .get_uuid(uid.clone())
            .await?
            .ok_or(UuidError::UnexistingIndex(uid))
    }

    async fn handle_delete(&self, uid: String) -> Result<Uuid> {
        self.store
            .delete(uid.clone())
            .await?
            .ok_or(UuidError::UnexistingIndex(uid))
    }

    async fn handle_list(&self) -> Result<Vec<(String, Uuid)>> {
        let result = self.store.list().await?;
        Ok(result)
    }

    async fn handle_insert(&self, uid: String, uuid: Uuid) -> Result<()> {
        if !is_index_uid_valid(&uid) {
            return Err(UuidError::BadlyFormatted(uid));
        }
        self.store.insert(uid, uuid).await?;
        Ok(())
    }
}

fn is_index_uid_valid(uid: &str) -> bool {
    uid.chars()
        .all(|x| x.is_ascii_alphanumeric() || x == '-' || x == '_')
}

#[derive(Clone)]
pub struct UuidResolverHandle {
    sender: mpsc::Sender<UuidResolveMsg>,
}

impl UuidResolverHandle {
    pub fn new(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let (sender, reveiver) = mpsc::channel(100);
        let store = HeedUuidStore::new(path)?;
        let actor = UuidResolverActor::new(reveiver, store);
        tokio::spawn(actor.run());
        Ok(Self { sender })
    }

    pub async fn get(&self, name: String) -> Result<Uuid> {
        let (ret, receiver) = oneshot::channel();
        let msg = UuidResolveMsg::Get { uid: name, ret };
        let _ = self.sender.send(msg).await;
        Ok(receiver
            .await
            .expect("Uuid resolver actor has been killed")?)
    }

    pub async fn create(&self, name: String) -> anyhow::Result<Uuid> {
        let (ret, receiver) = oneshot::channel();
        let msg = UuidResolveMsg::Create { uid: name, ret };
        let _ = self.sender.send(msg).await;
        Ok(receiver
            .await
            .expect("Uuid resolver actor has been killed")?)
    }

    pub async fn delete(&self, name: String) -> anyhow::Result<Uuid> {
        let (ret, receiver) = oneshot::channel();
        let msg = UuidResolveMsg::Delete { uid: name, ret };
        let _ = self.sender.send(msg).await;
        Ok(receiver
            .await
            .expect("Uuid resolver actor has been killed")?)
    }

    pub async fn list(&self) -> anyhow::Result<Vec<(String, Uuid)>> {
        let (ret, receiver) = oneshot::channel();
        let msg = UuidResolveMsg::List { ret };
        let _ = self.sender.send(msg).await;
        Ok(receiver
            .await
            .expect("Uuid resolver actor has been killed")?)
    }

    pub async fn insert(&self, name: String, uuid: Uuid) -> anyhow::Result<()> {
        let (ret, receiver) = oneshot::channel();
        let msg = UuidResolveMsg::Insert { ret, name, uuid };
        let _ = self.sender.send(msg).await;
        Ok(receiver
            .await
            .expect("Uuid resolver actor has been killed")?)
    }
}

#[derive(Debug, Error)]
pub enum UuidError {
    #[error("Name already exist.")]
    NameAlreadyExist,
    #[error("Index \"{0}\" doesn't exist.")]
    UnexistingIndex(String),
    #[error("Error performing task: {0}")]
    TokioTask(#[from] tokio::task::JoinError),
    #[error("Database error: {0}")]
    Heed(#[from] heed::Error),
    #[error("Uuid error: {0}")]
    Uuid(#[from] uuid::Error),
    #[error("Badly formatted index uid: {0}")]
    BadlyFormatted(String),
}

#[async_trait::async_trait]
trait UuidStore {
    // Create a new entry for `name`. Return an error if `err` and the entry already exists, return
    // the uuid otherwise.
    async fn create_uuid(&self, uid: String, err: bool) -> Result<Uuid>;
    async fn get_uuid(&self, uid: String) -> Result<Option<Uuid>>;
    async fn delete(&self, uid: String) -> Result<Option<Uuid>>;
    async fn list(&self) -> Result<Vec<(String, Uuid)>>;
    async fn insert(&self, name: String, uuid: Uuid) -> Result<()>;
}

struct HeedUuidStore {
    env: Env,
    db: Database<Str, ByteSlice>,
}

impl HeedUuidStore {
    fn new(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().join("index_uuids");
        create_dir_all(&path)?;
        let mut options = EnvOpenOptions::new();
        options.map_size(1_073_741_824); // 1GB
        let env = options.open(path)?;
        let db = env.create_database(None)?;
        Ok(Self { env, db })
    }
}

#[async_trait::async_trait]
impl UuidStore for HeedUuidStore {
    async fn create_uuid(&self, name: String, err: bool) -> Result<Uuid> {
        let env = self.env.clone();
        let db = self.db;
        tokio::task::spawn_blocking(move || {
            let mut txn = env.write_txn()?;
            match db.get(&txn, &name)? {
                Some(uuid) => {
                    if err {
                        Err(UuidError::NameAlreadyExist)
                    } else {
                        let uuid = Uuid::from_slice(uuid)?;
                        Ok(uuid)
                    }
                }
                None => {
                    let uuid = Uuid::new_v4();
                    db.put(&mut txn, &name, uuid.as_bytes())?;
                    txn.commit()?;
                    Ok(uuid)
                }
            }
        })
        .await?
    }

    async fn get_uuid(&self, name: String) -> Result<Option<Uuid>> {
        let env = self.env.clone();
        let db = self.db;
        tokio::task::spawn_blocking(move || {
            let txn = env.read_txn()?;
            match db.get(&txn, &name)? {
                Some(uuid) => {
                    let uuid = Uuid::from_slice(uuid)?;
                    Ok(Some(uuid))
                }
                None => Ok(None),
            }
        })
        .await?
    }

    async fn delete(&self, uid: String) -> Result<Option<Uuid>> {
        let env = self.env.clone();
        let db = self.db;
        tokio::task::spawn_blocking(move || {
            let mut txn = env.write_txn()?;
            match db.get(&txn, &uid)? {
                Some(uuid) => {
                    let uuid = Uuid::from_slice(uuid)?;
                    db.delete(&mut txn, &uid)?;
                    txn.commit()?;
                    Ok(Some(uuid))
                }
                None => Ok(None),
            }
        })
        .await?
    }

    async fn list(&self) -> Result<Vec<(String, Uuid)>> {
        let env = self.env.clone();
        let db = self.db;
        tokio::task::spawn_blocking(move || {
            let txn = env.read_txn()?;
            let mut entries = Vec::new();
            for entry in db.iter(&txn)? {
                let (name, uuid) = entry?;
                let uuid = Uuid::from_slice(uuid)?;
                entries.push((name.to_owned(), uuid))
            }
            Ok(entries)
        })
        .await?
    }

    async fn insert(&self, name: String, uuid: Uuid) -> Result<()> {
        let env = self.env.clone();
        let db = self.db;
        tokio::task::spawn_blocking(move || {
            let mut txn = env.write_txn()?;
            db.put(&mut txn, &name, uuid.as_bytes())?;
            txn.commit()?;
            Ok(())
        })
        .await?
    }
}
