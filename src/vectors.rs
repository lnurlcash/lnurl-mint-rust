//! LUD-25 and LUD-26's own test vectors, run against this plugin's paths.

use lnurlcash_core::{
    recoverable::{decode_cx1, derive_note_pubkey, encode_cs1_with_amount},
    signature::note_signature_message_for_hash,
};
use serde_json::Value;

use crate::spend;

fn vectors(name: &str) -> Value {
    let raw = match name {
        "25" => include_str!("../tests/vectors/25-vectors.json"),
        _ => include_str!("../tests/vectors/26-vectors.json"),
    };
    serde_json::from_str(raw).unwrap()
}

fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("vector field {key}"))
}

#[test]
fn key_path_spend_opens_its_note_at_its_domain_only() {
    let v = &vectors("25")["key_path_spend"];
    let parsed = spend::parse(s(v, "ck1")).unwrap();
    assert_eq!(parsed.note_id, s(v, "Q"));
    assert_eq!(
        spend::note_id_of_ref(s(v, "cp1")).as_deref(),
        Some(s(v, "Q"))
    );
    assert_eq!(spend::verify(&parsed, 0, &[s(v, "domain").into()], 1), None);
    assert!(spend::verify(&parsed, 0, &["other.example".into()], 1).is_some());
}

#[test]
fn certificates_encode_as_the_vectors_do() {
    let v = &vectors("25")["certificates"];
    for case in v["cases"].as_array().unwrap() {
        let amount = case["amount_msat"].as_u64().unwrap();
        assert_eq!(
            note_signature_message_for_hash(s(v, "Q"), amount),
            s(case, "message")
        );
        let sig: [u8; 65] = hex::decode(s(case, "signature"))
            .unwrap()
            .try_into()
            .unwrap();
        assert_eq!(encode_cs1_with_amount(amount, &sig), s(case, "cs1"));
    }
}

#[test]
fn bearer_note_every_form_names_one_note() {
    let v = &vectors("25")["bearer_note"];
    let q = s(v, "Q");
    for reference in [s(v, "h"), s(v, "cp1")] {
        assert_eq!(spend::note_id_of_ref(reference).as_deref(), Some(q));
    }
    for k1 in [s(v, "preimage"), s(v, "cw1")] {
        let parsed = spend::parse(k1).unwrap();
        assert_eq!(parsed.note_id, q);
        // a bearer leaf checks no signature: any domain
        assert_eq!(
            spend::verify(&parsed, 0, &["anywhere.example".into()], 1),
            None
        );
    }
    let cert = &v["certificate"];
    let sig: [u8; 65] = hex::decode(s(cert, "signature"))
        .unwrap()
        .try_into()
        .unwrap();
    let amount = cert["amount_msat"].as_u64().unwrap();
    assert_eq!(encode_cs1_with_amount(amount, &sig), s(cert, "cs1"));
}

#[test]
fn cx1_derivation_matches_every_note() {
    for branch in vectors("26")["derivation"].as_array().unwrap() {
        let cx1 = decode_cx1(s(branch, "cx1")).unwrap();
        for note in branch["notes"].as_array().unwrap() {
            let purpose = note["purpose"].as_u64().unwrap() as u32;
            let i = note["i"].as_u64().unwrap() as u32;
            let q = derive_note_pubkey(&cx1.pubkey_x_only, &cx1.chain_code, purpose, i).unwrap();
            // Q is given compressed; its x is the note
            assert_eq!(hex::encode(q), s(note, "Q")[2..]);
            assert_eq!(spend::note_id_of_ref(s(note, "cp1")), Some(hex::encode(q)));
        }
    }
}

#[test]
fn registration_proofs_verify() {
    let v = vectors("26");
    let reg = &v["registration"];
    let cx1 = decode_cx1(s(&v["derivation"][1], "cx1")).unwrap();
    let branch_hex = hex::encode([cx1.pubkey_x_only, cx1.chain_code].concat());
    let (domain, username) = (s(reg, "domain"), s(reg, "username"));
    for action in ["register", "unregister"] {
        let sig = s(&reg[action], "sig");
        assert!(crate::mint::owns_branch(
            action,
            domain,
            username,
            &branch_hex,
            sig
        ));
    }
    let register_sig = s(&reg["register"], "sig");
    assert!(!crate::mint::owns_branch(
        "unregister",
        domain,
        username,
        &branch_hex,
        register_sig
    ));
}
