//! SPICE Ticket authentication primitives.

use rsa::pkcs8::DecodePublicKey;
use rsa::rand_core::OsRng;
use rsa::traits::PublicKeyParts;
use rsa::{Oaep, RsaPublicKey};
use sha1::Sha1;
use zeroize::Zeroizing;

use crate::ClientError;

/// Protocol maximum password bytes before the terminating NUL.
pub(crate) const MAX_PASSWORD_BYTES: usize = 60;
/// RSA-1024 always produces this encrypted Ticket size.
pub(crate) const ENCRYPTED_TICKET_SIZE: usize = 128;

/// Encrypts a NUL-terminated Ticket without retaining the cleartext buffer.
pub(crate) fn encrypt_ticket(
    public_key_der: &[u8],
    password: &str,
) -> Result<[u8; ENCRYPTED_TICKET_SIZE], ClientError> {
    if password.len() > MAX_PASSWORD_BYTES || password.as_bytes().contains(&0) {
        return Err(ClientError::Configuration(
            "Ticket password must be at most 60 bytes and contain no NUL",
        ));
    }
    let public_key = RsaPublicKey::from_public_key_der(public_key_der)
        .map_err(|_| ClientError::InvalidTicketKey)?;
    if public_key.size() != ENCRYPTED_TICKET_SIZE {
        return Err(ClientError::InvalidTicketKey);
    }

    let mut plaintext = Zeroizing::new(Vec::with_capacity(password.len() + 1));
    plaintext.extend_from_slice(password.as_bytes());
    plaintext.push(0);
    let encrypted = public_key
        .encrypt(&mut OsRng, Oaep::new::<Sha1>(), &plaintext)
        .map_err(|_| ClientError::TicketEncryption)?;
    encrypted
        .try_into()
        .map_err(|_| ClientError::TicketEncryption)
}
