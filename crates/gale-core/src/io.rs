//! Host → session commands shared by every interactive protocol.

/// Commands a UI sends to a running interactive session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionInput {
    /// Raw bytes typed by the user (already encoded for the terminal).
    Data(Vec<u8>),
    /// Terminal grid changed size.
    Resize { cols: u16, rows: u16 },
    /// Close the session gracefully.
    Close,
}
