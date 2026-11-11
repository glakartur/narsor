//! The backend lease a controller holds while it serves a backend.

mod grant;

pub(crate) use grant::{bounded_ttl, lease_token, lease_view};
