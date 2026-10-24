use super::*;

fn uri(seed: &[u8], query: &str) -> String {
    format!(
        "otpauth://totp/Example:alice?secret={}&issuer=Example{query}",
        data_encoding::BASE32_NOPAD.encode(seed)
    )
}

#[test]
fn rfc6238_all_eighteen_vectors() {
    // Public RFC 6238 Appendix B keys/vectors, never a real credential.
    let seeds: [&[u8]; 3] = [
        b"12345678901234567890",
        b"12345678901234567890123456789012",
        b"1234567890123456789012345678901234567890123456789012345678901234",
    ];
    let vectors = [
        (59, ["94287082", "46119246", "90693936"]),
        (1111111109, ["07081804", "68084774", "25091201"]),
        (1111111111, ["14050471", "67062674", "99943326"]),
        (1234567890, ["89005924", "91819424", "93441116"]),
        (2000000000, ["69279037", "90698825", "38618901"]),
        (20000000000, ["65353130", "77737706", "47863826"]),
    ];
    for (now, expected) in vectors {
        for (index, algorithm) in ["SHA1", "SHA256", "SHA512"].into_iter().enumerate() {
            let code = generate(
                &uri(seeds[index], &format!("&algorithm={algorithm}&digits=8")),
                Some(now),
            )
            .unwrap();
            assert_eq!(code.code.as_str(), expected[index]);
            assert_eq!(code.generated_at_unix, now);
            assert_eq!(code.valid_from_unix, now - now % 30);
            assert_eq!(code.valid_until_unix, code.valid_from_unix + 30);
        }
    }
}

#[test]
fn bare_seed_whitespace_matches_rfc6238_vectors() {
    // The public RFC SHA1 key, with ASCII and Unicode formatting whitespace.
    let grouped = " \tGEZD GNBV\nGY3T\rQOJQ\u{000b}GEZD\u{000c}GNBV\u{00a0}GY3T\u{2003}QOJQ\u{2028}\u{2029}\u{3000}";
    let lowercase = grouped.to_ascii_lowercase();
    let canonical = data_encoding::BASE32_NOPAD.encode(b"12345678901234567890");
    for (now, expected) in [
        (59, "287082"),
        (1111111109, "081804"),
        (1111111111, "050471"),
        (1234567890, "005924"),
        (2000000000, "279037"),
        (20000000000, "353130"),
    ] {
        for value in [grouped, lowercase.as_str()] {
            let code = generate(value, Some(now)).unwrap();
            assert_eq!(code.code.as_str(), expected);
            assert_eq!(
                code.code.as_str(),
                generate(&canonical, Some(now)).unwrap().code.as_str()
            );
        }
    }
}

#[test]
fn defaults_padding_lowercase_short_seed_and_boundaries() {
    let seed = data_encoding::BASE32_NOPAD.encode(b"12345678901234567890");
    let code = generate(&seed.to_ascii_lowercase(), Some(59)).unwrap();
    assert_eq!(code.code.as_str(), "287082");
    assert_eq!(code.algorithm, Algorithm::Sha1);
    assert_eq!(code.digits, 6);
    assert_eq!((code.valid_from_unix, code.valid_until_unix), (30, 60));
    assert_eq!((code.issuer, code.account), (None, None));
    for now in [0, 29, 30, 59, 60] {
        let code = generate("MY======", Some(now)).unwrap(); // One-byte public synthetic key.
        assert_eq!(
            code.code.as_str(),
            generate("my", Some(now)).unwrap().code.as_str()
        );
        for formatted in [" \tM y = = = = = =\r\n", " m\u{00a0}y \n"] {
            assert_eq!(
                code.code.as_str(),
                generate(formatted, Some(now)).unwrap().code.as_str()
            );
        }
        assert_eq!(code.valid_from_unix, now / 30 * 30);
        assert_eq!(code.valid_until_unix, (now / 30 + 1) * 30);
    }
    let custom = uri(b"public", "&algorithm=SHA512&digits=8&period=45");
    let a = generate(&custom, Some(44)).unwrap();
    let b = generate(&custom, Some(45)).unwrap();
    assert_eq!((a.valid_from_unix, a.valid_until_unix), (0, 45));
    assert_eq!((b.valid_from_unix, b.valid_until_unix), (45, 90));
    assert_ne!(a.code.as_str(), b.code.as_str());
    assert_eq!(b.code.len(), 8);
    assert!(generate("MY", Some(u64::MAX)).is_err());
    assert!(generate(&uri(b"public", "&period=1"), Some(u64::MAX)).is_err());
}

#[test]
fn uri_seed_whitespace_is_not_bare_seed_formatting() {
    for seed in [
        " MY",
        "M Y",
        "MY\n",
        "%20MY",
        "M%09Y",
        "MY%0A",
        "M%C2%A0Y",
        "M%E2%80%83Y",
        "M+Y",
    ] {
        let value = format!("otpauth://totp/alice?secret={seed}");
        assert!(generate(&value, Some(0)).is_err());
    }
    assert!(generate(" otpauth://totp/alice?secret=MY", Some(0)).is_err());
}

#[test]
fn normalized_identity_and_unknown_annotations_do_not_change_crypto() {
    let base = uri(b"public", "");
    let annotated =
        format!("{base}&image=https%3A%2F%2Fexample.test%2Fa.png&provider=x&provider=y");
    let a = generate(&base, Some(59)).unwrap();
    let b = generate(&annotated, Some(59)).unwrap();
    assert_eq!(a.code.as_str(), b.code.as_str());
    assert_eq!(b.issuer.as_deref(), Some("Example"));
    assert_eq!(b.account.as_deref(), Some("alice"));
    for value in [
        "otpauth://totp/ACME%20Co%3A%20alice%40example.test?secret=MY&issuer=ACME%20Co",
        "otpauth://totp/alice%40example.test?secret=MY&issuer=ACME%20Co",
        "otpauth://totp/ACME%20Co:alice%40example.test?secret=MY",
    ] {
        let code = generate(value, Some(0)).unwrap();
        assert_eq!(code.issuer.as_deref(), Some("ACME Co"));
        assert_eq!(code.account.as_deref(), Some("alice@example.test"));
    }
    let code = generate("otpauth://totp/alice?secret=MY", Some(0)).unwrap();
    assert_eq!(code.issuer, None);
    assert_eq!(code.account.as_deref(), Some("alice"));
}

#[test]
fn malformed_ambiguous_unsupported_inputs_never_echo_material() {
    for value in [
        "",
        "M",
        "MZ",
        "MY=",
        "MY=====",
        "MY=======",
        "M=Y=====",
        "M1",
        " \t\r\n\u{00a0}\u{2003}",
        "M-Y",
        "M_Y",
        "M.Y",
        "M\0Y",
        "M\u{001b}Y",
        "M\u{007f}Y",
        "M\u{200b}Y",
        "M\u{200c}Y",
        "M\u{200d}Y",
        "M\u{2060}Y",
        "M\u{feff}Y",
    ] {
        assert!(generate(value, Some(0)).is_err(), "invalid Base32 accepted");
    }
    let marker = "SYNTHETIC-PRIVATE-MATERIAL";
    let mut invalid = vec![
        format!("otpauth://hotp/{marker}?secret=MY&counter=0"),
        format!("otpauth://totp/{marker}?secret={marker}"),
        format!("otpauth://totp/{marker}?issuer=Example"),
        format!("otpauth://totp/{marker}?secret=MY&secret=MY"),
        format!("otpauth://totp/{marker}?secret=MY&counter=0"),
        "otpauth://totp/?secret=MY".into(),
        "otpauth://totp/a:b:c?secret=MY".into(),
        "otpauth://totp/a/b?secret=MY".into(),
        "otpauth://user@totp/a?secret=MY".into(),
        "otpauth://totp:443/a?secret=MY".into(),
        "otpauth://totp/a?secret=MY#fragment".into(),
        "otpauth://totp/a%FF?secret=MY".into(),
        "otpauth://totp/a%0A?secret=MY".into(),
        "otpauth://totp/a%?secret=MY".into(),
        "otpauth://totp/A:a?secret=MY&issuer=B".into(),
        "otpauth://totp/a?secret=MY&issuer=%FF".into(),
        "otpauth://totp/a?secret=MY&issuer=X&issuer=X".into(),
    ];
    for query in [
        "algorithm=SHA3",
        "algorithm=",
        "algorithm=SHA1&algorithm=SHA1",
        "digits=7",
        "digits=6&digits=6",
        "period=0",
        "period=-1",
        "period=+30",
        "period=",
        "period=18446744073709551616",
        "period=30&period=30",
    ] {
        invalid.push(format!("otpauth://totp/{marker}?secret=MY&{query}"));
    }
    for value in invalid {
        let error = generate(&value, Some(0))
            .err()
            .expect("invalid URI accepted");
        let error = format!("{error:#}");
        assert!(!error.contains(marker));
        assert!(!error.contains("otpauth://"));
        assert!(!error.contains("secret="));
    }
}
