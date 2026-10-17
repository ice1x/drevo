# Test fixtures

`test_rsa_2048.pk8` is an RSA private key made for the OIDC tests only (PKCS#8
DER). `test_rsa_2048.public.json` holds its public modulus and exponent as JWK
fields. Nothing outside `tests/oidc.rs` uses them, and they protect nothing.
