use std::collections::BTreeMap;

use atuin_common::record::{DecryptedData, Host, HostId};
use eyre::{bail, ensure, eyre, Result};
use serde::Deserialize;

use crate::record::encryption::PASETO_V4;
use crate::record::store::Store;

const KV_VERSION: &str = "v0";
const KV_TAG: &str = "kv";
const KV_VAL_MAX_LEN: usize = 100 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KvRecord {
    pub namespace: String,
    pub key: String,
    pub value: String,
}

impl KvRecord {
    pub fn serialize(&self) -> Result<DecryptedData> {
        use rmp::encode;

        let mut output = vec![];

        // INFO: ensure this is updated when adding new fields
        encode::write_array_len(&mut output, 3)?;

        encode::write_str(&mut output, &self.namespace)?;
        encode::write_str(&mut output, &self.key)?;
        encode::write_str(&mut output, &self.value)?;

        Ok(DecryptedData(output))
    }

    pub fn deserialize(data: &DecryptedData, version: &str) -> Result<Self> {
        use rmp::decode;

        fn error_report<E: std::fmt::Debug>(err: E) -> eyre::Report {
            eyre!("{err:?}")
        }

        match version {
            KV_VERSION => {
                let mut bytes = decode::Bytes::new(&data.0);

                let nfields = decode::read_array_len(&mut bytes).map_err(error_report)?;
                ensure!(nfields == 3, "too many entries in v0 kv record");

                let bytes = bytes.remaining_slice();

                let (namespace, bytes) =
                    decode::read_str_from_slice(bytes).map_err(error_report)?;
                let (key, bytes) = decode::read_str_from_slice(bytes).map_err(error_report)?;
                let (value, bytes) = decode::read_str_from_slice(bytes).map_err(error_report)?;

                if !bytes.is_empty() {
                    bail!("trailing bytes in encoded kvrecord. malformed")
                }

                Ok(KvRecord {
                    namespace: namespace.to_owned(),
                    key: key.to_owned(),
                    value: value.to_owned(),
                })
            }
            _ => {
                bail!("unknown version {version:?}")
            }
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct KvStore;

impl Default for KvStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KvStore {
    // will want to init the actual kv store when that is done
    pub fn new() -> KvStore {
        KvStore {}
    }

    pub async fn set(
        &self,
        store: &mut (impl Store + Send + Sync),
        encryption_key: &[u8; 32],
        host_id: HostId,
        namespace: &str,
        key: &str,
        value: &str,
    ) -> Result<()> {
        if value.len() > KV_VAL_MAX_LEN {
            return Err(eyre!(
                "kv value too large: max len {} bytes",
                KV_VAL_MAX_LEN
            ));
        }

        let record = KvRecord {
            namespace: namespace.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        };

        let bytes = record.serialize()?;

        let idx = store
            .last(host_id, KV_TAG)
            .await?
            .map_or(0, |entry| entry.idx + 1);

        let record = atuin_common::record::Record::builder()
            .host(Host::new(host_id))
            .version(KV_VERSION.to_string())
            .tag(KV_TAG.to_string())
            .idx(idx)
            .data(bytes)
            .build();

        store
            .push(&record.encrypt::<PASETO_V4>(encryption_key))
            .await?;

        Ok(())
    }

    // TODO: setup an actual kv store, rebuild func, and do not pass the main store in here as
    // well.
    pub async fn get(
        &self,
        _store: &impl Store,
        _encryption_key: &[u8; 32],
        _namespace: &str,
        _key: &str,
    ) -> Result<Option<KvRecord>> {
        // TODO: implement

        Ok(None)
    }

    // Build a kv map out of the linked list kv store
    // Map is Namespace -> Key -> Value
    // TODO(ellie): "cache" this into a real kv structure, which we can
    // use as a write-through cache to avoid constant rebuilds.
    pub async fn build_kv(
        &self,
        store: &impl Store,
        encryption_key: &[u8; 32],
    ) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
        let map = BTreeMap::new();

        // get the status of all stores
        let mut tagged = store.all_tagged(KV_TAG).await?;

        // iterate through all tags and play each KV record at a time
        for (host, record) in tagged {
            let decrypted = match record.version.as_str() {
                KV_VERSION => record.decrypt::<PASETO_V4>(encryption_key)?,
                version => bail!("unknown version {version:?}"),
            };

            let kv = KvRecord::deserialize(&decrypted.data, &decrypted.version)?;

            let next = store.next(host, KV_TAG, decrypted.idx, 1).await?;

            write a function that iterates the kv records in order and builds a map
            maybe next_record(tag) that returns the next of that tag in time, ignoring host?
            then write some tests for this new sync
            also see what happens if we run the new server but try and sync with v17 client. I imagine `atuin sync` breaks, but auto sync is fine. maybe version the api now.
        }

        Ok(map)
    }
}

#[cfg(test)]
mod tests {
    use crypto_secretbox::{KeyInit, XSalsa20Poly1305};
    use rand::rngs::OsRng;

    use crate::record::sqlite_store::SqliteStore;

    use super::{KvRecord, KvStore, KV_VERSION};

    #[test]
    fn encode_decode() {
        let kv = KvRecord {
            namespace: "foo".to_owned(),
            key: "bar".to_owned(),
            value: "baz".to_owned(),
        };
        let snapshot = [
            0x93, 0xa3, b'f', b'o', b'o', 0xa3, b'b', b'a', b'r', 0xa3, b'b', b'a', b'z',
        ];

        let encoded = kv.serialize().unwrap();
        let decoded = KvRecord::deserialize(&encoded, KV_VERSION).unwrap();

        assert_eq!(encoded.0, &snapshot);
        assert_eq!(decoded, kv);
    }

    #[tokio::test]
    async fn build_kv() {
        let mut store = SqliteStore::new(":memory:").await.unwrap();
        let kv = KvStore::new();
        let key: [u8; 32] = XSalsa20Poly1305::generate_key(&mut OsRng).into();
        let host_id = atuin_common::record::HostId(atuin_common::utils::uuid_v7());

        kv.set(&mut store, &key, host_id, "test-kv", "foo", "bar")
            .await
            .unwrap();

        kv.set(&mut store, &key, host_id, "test-kv", "1", "2")
            .await
            .unwrap();

        let map = kv.build_kv(&store, &key).await.unwrap();

        assert_eq!(
            map.get("test-kv")
                .expect("map namespace not set")
                .get("foo")
                .expect("map key not set"),
            "bar"
        );

        assert_eq!(
            map.get("test-kv")
                .expect("map namespace not set")
                .get("1")
                .expect("map key not set"),
            "2"
        );
    }
}
