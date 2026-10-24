//! Linux keeps persisted receipt safety without client-source operations.
pub use crate::accounts::takeover_receipt::{
    Journal, Receipt, ReceiptState, SourceProof, Verification,
};
pub use crate::platform::takeover::RefreshSourceContext;
