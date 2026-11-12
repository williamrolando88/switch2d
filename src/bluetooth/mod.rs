pub mod att;
pub mod l2cap;

pub use att::{AttClient, Characteristic, ResolvedHandles, Service};
pub use l2cap::{L2capSocket, SockaddrL2, AF_BLUETOOTH, ATT_CID, BDADDR_LE_PUBLIC, BDADDR_LE_RANDOM};
