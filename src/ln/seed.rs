//! The one secret this mint holds: a 32-byte seed in `<DATA_DIR>/seed`,
//! created on first start. LDK's keys and the BDK wallet's descriptors are
//! both derived from it, under separate labels, so backing up this file (with
//! the node's channel state) backs up every key.

use std::{
    fs,
    io::{ErrorKind, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};
use bitcoin::hashes::{Hash, HashEngine, Hmac, HmacEngine, sha256};

pub struct Seed([u8; 32]);

impl std::fmt::Debug for Seed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Seed(..)")
    }
}

impl Seed {
    /// Read the seed at `path`, or create it there (mode 0600) if absent.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        match fs::read(path) {
            Ok(bytes) => {
                let Ok(seed) = <[u8; 32]>::try_from(bytes.as_slice()) else {
                    bail!("{} is not a 32-byte seed", path.display());
                };
                Ok(Seed(seed))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let seed: [u8; 32] = rand::random();
                let mut options = fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
                let mut file = options
                    .open(path)
                    .with_context(|| format!("could not create {}", path.display()))?;
                file.write_all(&seed)?;
                file.sync_all()?;
                log::info!("created a new seed at {} - back it up", path.display());
                Ok(Seed(seed))
            }
            Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
        }
    }

    /// A key derived for one purpose: `HMAC-SHA256(seed, label)`.
    fn derive(&self, label: &str) -> [u8; 32] {
        let mut engine = HmacEngine::<sha256::Hash>::new(&self.0);
        engine.input(label.as_bytes());
        Hmac::<sha256::Hash>::from_engine(engine).to_byte_array()
    }

    /// The seed LDK's `KeysManager` gets: the node key and every channel key.
    pub fn ldk(&self) -> [u8; 32] {
        self.derive("lnurl-mint/ldk")
    }

    /// The seed the on-chain wallet's BIP-32 master key is made from.
    pub fn wallet(&self) -> [u8; 32] {
        self.derive("lnurl-mint/wallet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeds_persist_and_purposes_differ() {
        let dir = std::env::temp_dir().join(format!("lnurl-mint-seed-{}", rand::random::<u64>()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("seed");
        let a = Seed::load_or_create(&path).unwrap();
        let b = Seed::load_or_create(&path).unwrap();
        assert_eq!(a.0, b.0);
        assert_ne!(a.ldk(), a.wallet());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::write(&path, b"short").unwrap();
        assert!(Seed::load_or_create(&path).is_err());
        fs::remove_dir_all(dir).unwrap();
    }
}
