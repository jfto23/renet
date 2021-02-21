use crate::connection::ClientId;
use crate::error::Result;

pub mod unsecure;

pub trait SecurityService {
    fn ss_wrap(&mut self, data: &[u8]) -> Result<Box<[u8]>>;
    fn ss_unwrap(&mut self, data: &[u8]) -> Result<Box<[u8]>>;
}

pub trait AuthenticationProtocol {
    type Service: SecurityService;

    fn create_payload(&mut self) -> Result<Option<Box<[u8]>>>;
    fn read_payload(&mut self, payload: &[u8]) -> Result<()>;
    fn is_authenticated(&self) -> bool;
    fn build_security_interface(&self) -> Self::Service;
    fn id(&self) -> ClientId;
}

// TODO: review name
pub trait ServerAuthenticationProtocol: AuthenticationProtocol + Sized {
    fn from_payload(payload: &[u8]) -> Result<Self>;
    // TODO: add deny connection fn
}
