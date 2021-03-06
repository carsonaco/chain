/// Witness for Merklized Abstract Syntax Trees (MAST) + Schnorr
pub mod tree;

use std::fmt;

use rlp::{Decodable, DecoderError, Encodable, Rlp, RlpStream};
use secp256k1::{
    self, key::PublicKey, schnorrsig::schnorr_verify, schnorrsig::SchnorrSignature, Message,
    RecoverableSignature, RecoveryId, Secp256k1,
};
use serde::{Deserialize, Serialize};

use crate::init::address::RedeemAddress;
use crate::tx::data::address::ExtendedAddr;
use crate::tx::data::{txid_hash, Tx};
use crate::tx::witness::tree::{MerklePath, ProofOp, RawPubkey, RawSignature};

pub type EcdsaSignature = RecoverableSignature;

/// A transaction witness is a vector of input witnesses
#[derive(Debug, Default, PartialEq, Eq, Clone, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TxWitness(Vec<TxInWitness>);

impl Encodable for TxWitness {
    fn rlp_append(&self, s: &mut RlpStream) {
        s.append_list(&self.0);
    }
}

impl Decodable for TxWitness {
    fn decode(rlp: &Rlp) -> Result<Self, DecoderError> {
        let witnesses: Vec<TxInWitness> = rlp.as_list()?;
        Ok(witnesses.into())
    }
}

impl TxWitness {
    /// creates an empty witness (for testing/tools)
    pub fn new() -> Self {
        TxWitness::default()
    }
}
impl From<Vec<TxInWitness>> for TxWitness {
    fn from(v: Vec<TxInWitness>) -> Self {
        TxWitness(v)
    }
}
impl ::std::iter::FromIterator<TxInWitness> for TxWitness {
    fn from_iter<I>(iter: I) -> Self
    where
        I: IntoIterator<Item = TxInWitness>,
    {
        TxWitness(Vec::from_iter(iter))
    }
}
impl ::std::ops::Deref for TxWitness {
    type Target = Vec<TxInWitness>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl ::std::ops::DerefMut for TxWitness {
    fn deref_mut(&mut self) -> &mut Vec<TxInWitness> {
        &mut self.0
    }
}

// normally should be some structure: e.g. indicate a type of signature
#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub enum TxInWitness {
    BasicRedeem(EcdsaSignature),
    TreeSig(PublicKey, SchnorrSignature, Vec<ProofOp>),
}

impl fmt::Display for TxInWitness {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

impl Encodable for TxInWitness {
    fn rlp_append(&self, s: &mut RlpStream) {
        match self {
            TxInWitness::BasicRedeem(sig) => {
                let (recovery_id, serialized_sig) = sig.serialize_compact();
                let signature: RawSignature = serialized_sig.into();
                // recovery_id is one of 0 | 1 | 2 | 3
                let rid = recovery_id.to_i32() as u8;
                s.begin_list(3).append(&0u8).append(&rid).append(&signature);
            }
            TxInWitness::TreeSig(pk, schnorrsig, ops) => {
                let serialized_pk: RawPubkey = pk.serialize().into();
                let serialized_sig: RawSignature = schnorrsig.serialize_default().into();
                // TODO: better proof op encoding
                s.begin_list(4)
                    .append(&1u8)
                    .append(&serialized_pk)
                    .append(&serialized_sig)
                    .append_list(&ops);
            }
        }
    }
}

impl Decodable for TxInWitness {
    fn decode(rlp: &Rlp) -> Result<Self, DecoderError> {
        let item_count = rlp.item_count()?;
        if !(item_count >= 3 && item_count <= 4) {
            return Err(DecoderError::Custom("Cannot decode a transaction witness"));
        }
        let type_tag: u8 = rlp.val_at(0)?;
        match (type_tag, item_count) {
            (0, 3) => {
                let rid: u8 = rlp.val_at(1)?;
                let raw_sig: RawSignature = rlp.val_at(2)?;
                let recovery_id = RecoveryId::from_i32(i32::from(rid))
                    .map_err(|_| DecoderError::Custom("failed to decode recovery id"))?;
                let sig = RecoverableSignature::from_compact(&raw_sig.as_bytes(), recovery_id)
                    .map_err(|_| DecoderError::Custom("failed to decode recoverable signature"))?;
                Ok(TxInWitness::BasicRedeem(sig))
            }
            (1, 4) => {
                let raw_pk: RawPubkey = rlp.val_at(1)?;
                let pk = PublicKey::from_slice(&raw_pk.as_bytes())
                    .map_err(|_| DecoderError::Custom("failed to public key"))?;

                let raw_sig: RawSignature = rlp.val_at(2)?;
                let schnorrsig = SchnorrSignature::from_default(&raw_sig.as_bytes())
                    .map_err(|_| DecoderError::Custom("failed to decode schnorr signature"))?;
                // TODO: max tree depth?
                let ops: Vec<ProofOp> = rlp.list_at(3)?;
                Ok(TxInWitness::TreeSig(pk, schnorrsig, ops))
            }
            _ => Err(DecoderError::Custom("Unknown transaction type")),
        }
    }
}

impl TxInWitness {
    /// verify a given extended address is associated to the witness
    /// and the signature against the given transation `Tx`
    /// TODO: capture possible errors in enum?
    ///
    pub fn verify_tx_address(
        &self,
        tx: &Tx,
        address: &ExtendedAddr,
    ) -> Result<(), secp256k1::Error> {
        let secp = Secp256k1::verification_only();
        let message = Message::from_slice(tx.id().as_bytes())?;

        match (&self, address) {
            (TxInWitness::BasicRedeem(sig), ExtendedAddr::BasicRedeem(addr)) => {
                let pk = secp.recover(&message, &sig)?;
                let expected_addr = RedeemAddress::from(&pk);
                // TODO: constant time eq?
                if *addr != expected_addr {
                    Err(secp256k1::Error::InvalidPublicKey)
                } else {
                    secp.verify(&message, &sig.to_standard(), &pk)
                }
            }
            (TxInWitness::TreeSig(pk, sig, ops), ExtendedAddr::OrTree(roothash)) => {
                let mut pk_hash = txid_hash(&pk.serialize());
                // TODO: blake2 tree hashing?
                for op in ops.iter() {
                    let mut bs = vec![1u8];
                    match op {
                        ProofOp(MerklePath::LFound, data) => {
                            bs.extend(&pk_hash[..]);
                            bs.extend(&data[..]);
                            pk_hash = txid_hash(&bs);
                        }
                        ProofOp(MerklePath::RFound, data) => {
                            bs.extend(&data[..]);
                            bs.extend(&pk_hash[..]);
                            pk_hash = txid_hash(&bs);
                        }
                    }
                }
                // TODO: constant time eq?
                if pk_hash != *roothash {
                    Err(secp256k1::Error::InvalidPublicKey)
                // TODO: migrate to upstream secp256k1 when Schnorr is available
                } else {
                    schnorr_verify(&secp, &message, &sig, &pk)
                }
            }
            (_, _) => Err(secp256k1::Error::InvalidSignature),
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::common::merkle::MerkleTree;
    use crate::common::H256;
    use crate::tx::data::txid_hash;
    use crate::tx::witness::tree::MerklePath;
    use secp256k1::{
        key::pubkey_combine,
        key::PublicKey,
        key::SecretKey,
        musig::{MuSigSession, MuSigSessionID},
        schnorrsig::{schnorr_sign, SchnorrSignature},
        Message, Secp256k1, Signing, Verification,
    };

    pub fn get_ecdsa_witness<C: Signing>(
        secp: &Secp256k1<C>,
        tx: &Tx,
        secret_key: &SecretKey,
    ) -> TxInWitness {
        let message = Message::from_slice(tx.id().as_bytes()).expect("32 bytes");
        let sig = secp.sign_recoverable(&message, &secret_key);
        return TxInWitness::BasicRedeem(sig);
    }

    fn sign_single_schnorr<C: Signing>(
        secp: &Secp256k1<C>,
        msg: &Message,
        secret_key: &SecretKey,
    ) -> SchnorrSignature {
        schnorr_sign(secp, msg, secret_key).0
    }

    fn get_single_tx_witness<C: Signing>(
        secp: Secp256k1<C>,
        tx: &Tx,
        secret_key: &SecretKey,
    ) -> (TxInWitness, H256) {
        let message = Message::from_slice(tx.id().as_bytes()).expect("32 bytes");
        let sig = sign_single_schnorr(&secp, &message, &secret_key);
        let pk = PublicKey::from_secret_key(&secp, &secret_key);

        let pk_hash = txid_hash(&pk.serialize());
        let merkle = MerkleTree::new(&vec![pk_hash]);

        return (
            TxInWitness::TreeSig(pk, sig, vec![]),
            merkle.get_root_hash(),
        );
    }

    fn get_2_of_2_sig<C: Signing + Verification>(
        secp: &Secp256k1<C>,
        tx: &Tx,
        secret_key1: SecretKey,
        secret_key2: SecretKey,
    ) -> (SchnorrSignature, PublicKey, PublicKey) {
        let message = Message::from_slice(tx.id().as_bytes()).expect("32 bytes");
        let pk1 = PublicKey::from_secret_key(&secp, &secret_key1);
        let pk2 = PublicKey::from_secret_key(&secp, &secret_key2);
        let session_id1 = MuSigSessionID::from_slice(&[0x01; 32]).expect("32 bytes");
        let session_id2 = MuSigSessionID::from_slice(&[0x02; 32]).expect("32 bytes");
        let (pk, pk_hash) = pubkey_combine(secp, &vec![pk1, pk2]).expect("combined pk");
        let mut session1 = MuSigSession::new(
            secp,
            session_id1,
            &message,
            &pk,
            &pk_hash,
            2,
            0,
            &secret_key1,
        )
        .expect("session 1");
        let mut session2 = MuSigSession::new(
            secp,
            session_id2,
            &message,
            &pk,
            &pk_hash,
            2,
            1,
            &secret_key2,
        )
        .expect("session 2");
        session1.set_nonce_commitment(session2.get_my_nonce_commitment(), 1);
        session2.set_nonce_commitment(session1.get_my_nonce_commitment(), 0);
        let nonces = vec![
            session1.get_public_nonce().unwrap(),
            session2.get_public_nonce().unwrap(),
        ];
        for i in 0..nonces.len() {
            let nonce = nonces[i];
            session1.set_nonce(i, nonce).expect("nonce in session1");
            session2.set_nonce(i, nonce).expect("nonce in session2");
        }
        session1.combine_nonces().expect("combined nonces session1");
        session2.combine_nonces().expect("combined nonces session2");
        let partial_sigs = vec![
            session1.partial_sign().expect("partial signature 1"),
            session2.partial_sign().expect("partial signature 2"),
        ];
        let sig = session1
            .partial_sig_combine(&partial_sigs)
            .expect("combined signature");
        return (sig, pk1, pk2);
    }

    fn get_2_of_2_tx_witness<C: Signing + Verification>(
        secp: Secp256k1<C>,
        tx: &Tx,
        secret_key1: SecretKey,
        secret_key2: SecretKey,
    ) -> (TxInWitness, H256) {
        let (sig, pk1, pk2) = get_2_of_2_sig(&secp, tx, secret_key1, secret_key2);

        let pk = pubkey_combine(&secp, &vec![pk1, pk2]).unwrap().0;
        let pk_hash = txid_hash(&pk.serialize()[..]);
        let merkle = MerkleTree::new(&vec![pk_hash]);

        return (
            TxInWitness::TreeSig(pk, sig, vec![]),
            merkle.get_root_hash(),
        );
    }

    fn get_2_of_3_tx_witness<C: Signing + Verification>(
        secp: Secp256k1<C>,
        tx: &Tx,
        secret_key1: SecretKey,
        secret_key2: SecretKey,
        secret_key3: SecretKey,
    ) -> (TxInWitness, H256) {
        let pk1 = PublicKey::from_secret_key(&secp, &secret_key1);
        let pk2 = PublicKey::from_secret_key(&secp, &secret_key2);
        let pk3 = PublicKey::from_secret_key(&secp, &secret_key3);
        let pkc1 = pubkey_combine(&secp, &vec![pk1, pk2]).unwrap().0;
        let pkc2 = pubkey_combine(&secp, &vec![pk1, pk3]).unwrap().0;
        let pkc3 = pubkey_combine(&secp, &vec![pk2, pk3]).unwrap().0;
        let pk_hashes: Vec<H256> = vec![pkc1, pkc2, pkc3]
            .iter()
            .map(|x| txid_hash(&x.serialize()[..]))
            .collect();
        let merkle = MerkleTree::new(&pk_hashes);

        let path: Vec<ProofOp> = vec![
            ProofOp(MerklePath::LFound, pk_hashes[1]),
            ProofOp(MerklePath::LFound, pk_hashes[2]),
        ];

        let (sig, _, _) = get_2_of_2_sig(&secp, tx, secret_key1, secret_key2);

        return (
            TxInWitness::TreeSig(pkc1, sig, path),
            merkle.get_root_hash(),
        );
    }

    #[test]
    fn mismatched_signed_tx_should_fail() {
        let tx = Tx::new();
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let public_key = PublicKey::from_secret_key(&secp, &secret_key);
        let expected_addr1 = ExtendedAddr::OrTree([0x00; 32].into());
        let witness1 = get_ecdsa_witness(&secp, &tx, &secret_key);
        assert!(witness1.verify_tx_address(&tx, &expected_addr1).is_err());
        let expected_addr2 = ExtendedAddr::BasicRedeem(RedeemAddress::from(&public_key));
        let (witness2, _) = get_single_tx_witness(secp, &tx, &secret_key);
        assert!(witness2.verify_tx_address(&tx, &expected_addr2).is_err());
    }

    #[test]
    fn same_pk_recovered() {
        let tx = Tx::new();
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let public_key = PublicKey::from_secret_key(&secp, &secret_key);
        let witness: TxWitness = vec![get_ecdsa_witness(&secp, &tx, &secret_key)].into();
        let encoded = witness.rlp_bytes();
        let rlp = Rlp::new(&encoded);
        let decoded = TxWitness::decode(&rlp).expect("decode tx witness");
        match &decoded[0] {
            TxInWitness::BasicRedeem(sig) => {
                let message = Message::from_slice(tx.id().as_bytes()).expect("32 bytes");
                let pk = secp.recover(&message, &sig).unwrap();
                assert_eq!(pk, public_key);
            }
            _ => {
                assert!(false);
            }
        }
    }

    #[test]
    fn signed_tx_should_verify() {
        let tx = Tx::new();
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let public_key = PublicKey::from_secret_key(&secp, &secret_key);
        let expected_addr = ExtendedAddr::BasicRedeem(RedeemAddress::from(&public_key));
        let witness = get_ecdsa_witness(&secp, &tx, &secret_key);
        assert!(witness.verify_tx_address(&tx, &expected_addr).is_ok());
    }

    #[test]
    fn schnorr_signed_tx_should_verify() {
        let tx = Tx::new();
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let (witness, addr) = get_single_tx_witness(secp, &tx, &secret_key);
        let expected_addr = ExtendedAddr::OrTree(addr);
        let r = witness.verify_tx_address(&tx, &expected_addr);
        assert!(r.is_ok());
    }

    #[test]
    fn agg_schnorr_signed_tx_should_verify() {
        let tx = Tx::new();
        let secp = Secp256k1::new();
        let secret_key1 = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let secret_key2 = SecretKey::from_slice(&[0xde; 32]).expect("32 bytes, within curve order");
        let (witness, addr) = get_2_of_2_tx_witness(secp, &tx, secret_key1, secret_key2);
        let expected_addr = ExtendedAddr::OrTree(addr);
        assert!(witness.verify_tx_address(&tx, &expected_addr).is_ok());
    }

    #[test]
    fn tree_agg_schnorr_signed_tx_should_verify() {
        let tx = Tx::new();
        let secp = Secp256k1::new();
        let secret_key1 = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let secret_key2 = SecretKey::from_slice(&[0xde; 32]).expect("32 bytes, within curve order");
        let secret_key3 = SecretKey::from_slice(&[0xef; 32]).expect("32 bytes, within curve order");
        let (witness, addr) =
            get_2_of_3_tx_witness(secp, &tx, secret_key1, secret_key2, secret_key3);
        let expected_addr = ExtendedAddr::OrTree(addr);
        assert!(witness.verify_tx_address(&tx, &expected_addr).is_ok());
    }

    #[test]
    fn wrong_basic_address_should_fail() {
        let tx = Tx::new();
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");

        let witness = get_ecdsa_witness(&secp, &tx, &secret_key);
        let wrong_addr = ExtendedAddr::BasicRedeem(RedeemAddress::default());
        assert!(witness.verify_tx_address(&tx, &wrong_addr).is_err());
    }

    #[test]
    fn wrongly_basic_signed_tx_should_fail() {
        let tx = Tx::new();
        let secp = Secp256k1::new();
        let secret_key = SecretKey::from_slice(&[0xcd; 32]).expect("32 bytes, within curve order");
        let message = Message::from_slice(&[0xaa; 32]).expect("32 bytes");
        let sign = secp.sign_recoverable(&message, &secret_key);
        let witness = TxInWitness::BasicRedeem(sign);
        let addr = ExtendedAddr::BasicRedeem(RedeemAddress::default());
        assert!(witness.verify_tx_address(&tx, &addr).is_err());
    }

}
