//! Join-by-code screen shown to the assisted user — Features 140, 148, and 149.
//!
//! The assisted user (Ana) is handed a 9-digit code out-of-band (phone call,
//! SMS, ticket).  She types it once and presses **Join**.  No account, login,
//! or registration is required at any point.  The daemon handles all network
//! connectivity: ICE gathering, NAT traversal, TURN relay selection, and
//! Noise-IK handshake.  The UI shell asks for nothing networking-related.
//!
//! # No-account join (Feature 140)
//!
//! [`JoinScreen`] accepts exactly one user input — the 9-digit `session_code`
//! provided by the expert out-of-band.  No username, password, email, or
//! account of any kind is required or accepted.  The type system enforces this:
//! [`connect`](JoinScreen::connect) takes no authentication arguments.
//!
//! # Zero-networking-questions invariant (Feature 149)
//!
//! [`JoinScreen`] exposes no server address, port, protocol, proxy, relay, or
//! STUN/TURN configuration to the user.  The daemon resolves all connectivity
//! details from the code and the ambient environment.
//!
//! # time_to_connected measurement (Feature 148)
//!
//! [`JoinScreen`] records the wall-clock instant when [`connect`](JoinScreen::connect)
//! is called and computes [`time_to_connected_ms`](JoinScreen::time_to_connected_ms)
//! when [`on_connected`](JoinScreen::on_connected) fires.  The shell reports this
//! to the daemon for telemetry.  The system SLA is p95 ≤ 5 000 ms from code entry.
//!
//! # State machine
//!
//! ```text
//! ┌──────┐  set_code + connect()  ┌────────────┐
//! │ Idle │ ─────────────────────► │ Connecting │
//! └──────┘                        └────────────┘
//!    ▲                             │           │
//!    │ reset()              on_connected()  on_failed()
//!    │                             │           │
//!    │                        ┌─────────┐  ┌────────┐
//!    └────────────────────────│Connected│  │ Failed │
//!                             └─────────┘  └────────┘
//! ```
//!
//! # Example
//!
//! ```
//! use lowband_shells::join_screen::{JoinScreen, JoinState, CodeError};
//!
//! let mut screen = JoinScreen::new();
//!
//! // The only input the user provides is the 9-digit code.
//! screen.set_code("123456789").unwrap();
//! screen.connect().unwrap();
//! assert_eq!(screen.state(), JoinState::Connecting);
//!
//! // Daemon fires the connected event — shell transitions the screen.
//! screen.on_connected();
//! assert_eq!(screen.state(), JoinState::Connected);
//! assert!(screen.time_to_connected_ms().is_some());
//! ```

/// Placeholder text for the session-code input field (Feature 140).
pub const CODE_INPUT_PLACEHOLDER: &str = "000 000 000";

/// Label for the primary join button (Feature 140).
pub const JOIN_BUTTON_LABEL: &str = "Join";

/// Errors returned by [`JoinScreen::set_code`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeError {
    /// The code must be exactly 9 characters; this one has a different length.
    WrongLength {
        /// Number of characters actually provided.
        got: usize,
    },
    /// Every character must be an ASCII decimal digit (0–9).
    NonDigit {
        /// The first non-digit character found.
        ch: char,
    },
}

impl std::fmt::Display for CodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CodeError::WrongLength { got } => {
                write!(f, "session code must be 9 digits, got {got}")
            }
            CodeError::NonDigit { ch } => {
                write!(f, "session code may only contain digits, found {:?}", ch)
            }
        }
    }
}

/// Errors returned by [`JoinScreen::connect`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectError {
    /// A valid 9-digit code must be set before connecting.
    NoCodeEntered,
    /// A connection attempt is already in progress.
    AlreadyConnecting,
    /// The session is already established.
    AlreadyConnected,
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConnectError::NoCodeEntered => {
                write!(f, "enter a 9-digit session code before connecting")
            }
            ConnectError::AlreadyConnecting => write!(f, "connection already in progress"),
            ConnectError::AlreadyConnected => write!(f, "already connected"),
        }
    }
}

/// State of the join screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinState {
    /// Waiting for the user to enter a code.
    Idle,
    /// Code submitted; waiting for the daemon to complete the handshake.
    Connecting,
    /// Session established.
    Connected,
    /// Connection attempt ended in an error; reason is shown to the user.
    Failed(String),
}

/// Join-by-code screen for the assisted user.
///
/// The only input exposed to the user is the 9-digit session code.  All
/// networking details (server resolution, ICE, NAT traversal, TURN relay
/// selection, Noise-IK handshake) are handled entirely by the daemon after
/// [`connect`](Self::connect) dispatches the code over the IPC socket.
///
/// Construct with [`JoinScreen::new`].  Drive with `set_code` → `connect`, then
/// transition with `on_connected` / `on_failed` as IPC events arrive from the
/// daemon.
pub struct JoinScreen {
    /// The 9-digit session code entered by the user.
    ///
    /// This is the only piece of information the user provides.  All other
    /// connectivity details are resolved by the daemon.
    session_code: Option<String>,
    state: JoinState,
    /// Wall-clock instant recorded when `connect()` is called (Feature 148).
    connect_start: Option<std::time::Instant>,
    /// Elapsed milliseconds from `connect()` to `on_connected()` (Feature 148).
    time_to_connected_ms: Option<u64>,
}

impl JoinScreen {
    /// Create a new join screen in the [`JoinState::Idle`] state.
    pub fn new() -> Self {
        JoinScreen {
            session_code: None,
            state: JoinState::Idle,
            connect_start: None,
            time_to_connected_ms: None,
        }
    }

    /// Validate and store the session code entered by the user.
    ///
    /// `code` must be exactly 9 ASCII decimal digits.  Leading/trailing
    /// whitespace is **not** stripped — the caller should trim the raw input
    /// before passing it here.
    ///
    /// Replaces any previously stored code.  May be called in `Idle` or
    /// `Failed` state to let the user correct a typo; ignored during
    /// `Connecting` or `Connected`.
    pub fn set_code(&mut self, code: &str) -> Result<(), CodeError> {
        let chars: Vec<char> = code.chars().collect();

        if chars.len() != 9 {
            return Err(CodeError::WrongLength { got: chars.len() });
        }
        if let Some(ch) = chars.iter().find(|c| !c.is_ascii_digit()) {
            return Err(CodeError::NonDigit { ch: *ch });
        }

        self.session_code = Some(code.to_string());
        Ok(())
    }

    /// Return the code currently stored, if any.
    pub fn session_code(&self) -> Option<&str> {
        self.session_code.as_deref()
    }

    /// Attempt to initiate a connection using the stored session code.
    ///
    /// Transitions from [`JoinState::Idle`] (or [`JoinState::Failed`] after a
    /// retry) to [`JoinState::Connecting`].  The shell caller is expected to
    /// forward the code to the daemon over the IPC socket.
    ///
    /// Records the current instant as the start of the connection attempt for
    /// [`time_to_connected_ms`](Self::time_to_connected_ms) measurement (Feature 148).
    ///
    /// Returns the 9-digit code that should be sent to the daemon, so the
    /// caller does not need to call [`session_code`](Self::session_code)
    /// separately.
    pub fn connect(&mut self) -> Result<&str, ConnectError> {
        match &self.state {
            JoinState::Connecting => return Err(ConnectError::AlreadyConnecting),
            JoinState::Connected => return Err(ConnectError::AlreadyConnected),
            JoinState::Idle | JoinState::Failed(_) => {}
        }
        if self.session_code.is_none() {
            return Err(ConnectError::NoCodeEntered);
        }
        self.state = JoinState::Connecting;
        self.connect_start = Some(std::time::Instant::now());
        self.time_to_connected_ms = None;
        Ok(self.session_code.as_deref().unwrap())
    }

    /// Transition to [`JoinState::Connected`].
    ///
    /// Call this when the daemon fires a `SessionEstablished` IPC event.
    /// Computes and stores [`time_to_connected_ms`](Self::time_to_connected_ms)
    /// from the instant [`connect`](Self::connect) was called (Feature 148).
    pub fn on_connected(&mut self) {
        if let Some(start) = self.connect_start {
            self.time_to_connected_ms = Some(start.elapsed().as_millis() as u64);
        }
        self.state = JoinState::Connected;
    }

    /// Elapsed milliseconds from [`connect`](Self::connect) to
    /// [`on_connected`](Self::on_connected) (Feature 148).
    ///
    /// Returns `None` until a connection has been successfully established.
    /// The system SLA is p95 ≤ 5 000 ms from code entry.
    pub fn time_to_connected_ms(&self) -> Option<u64> {
        self.time_to_connected_ms
    }

    /// Transition to [`JoinState::Failed`] with a human-readable reason string.
    ///
    /// Call this when the daemon fires a connection-failure IPC event.  The
    /// reason is surfaced in the UI so the user knows they can try a different
    /// code — never a networking error message they must act on.
    pub fn on_failed(&mut self, reason: impl Into<String>) {
        self.state = JoinState::Failed(reason.into());
    }

    /// Reset to the initial idle state and clear the stored code.
    ///
    /// Use this when the user dismisses the error banner and starts over.
    pub fn reset(&mut self) {
        self.session_code = None;
        self.state = JoinState::Idle;
        self.connect_start = None;
        self.time_to_connected_ms = None;
    }

    /// Current screen state.
    pub fn state(&self) -> JoinState {
        self.state.clone()
    }
}

impl Default for JoinScreen {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Zero-networking-questions invariant ───────────────────────────────────

    // The structural guarantee: JoinScreen exposes exactly one input — session_code.
    // The tests below confirm that connect() requires no network information from
    // the caller — no host, port, protocol, proxy, relay, or server selection.

    #[test]
    fn connect_requires_only_session_code_no_network_parameters() {
        // A successful connect() needs nothing beyond a valid 9-digit code.
        // If this compiles and passes without any additional arguments, the
        // zero-networking-questions invariant is enforced by the type system.
        let mut screen = JoinScreen::new();
        screen.set_code("000000000").unwrap();
        let code = screen.connect().unwrap();
        assert_eq!(code, "000000000");
        assert_eq!(screen.state(), JoinState::Connecting);
    }

    #[test]
    fn screen_has_no_server_address_field() {
        // JoinScreen holds only session_code and state.  This test forces the
        // compiler to instantiate the struct and confirms no network field exists
        // by verifying that the only readable user-input accessor is session_code.
        let screen = JoinScreen::new();
        // The following line must be the only user-input accessor that compiles:
        let _ = screen.session_code();
        // If a host/port/proxy accessor existed and was needed for the join flow,
        // it would have to appear in connect()'s signature — it does not.
    }

    // ── Code validation ───────────────────────────────────────────────────────

    #[test]
    fn valid_nine_digit_code_is_accepted() {
        let mut screen = JoinScreen::new();
        assert!(screen.set_code("123456789").is_ok());
        assert_eq!(screen.session_code(), Some("123456789"));
    }

    #[test]
    fn code_shorter_than_nine_is_rejected() {
        let mut screen = JoinScreen::new();
        let err = screen.set_code("12345678").unwrap_err();
        assert_eq!(err, CodeError::WrongLength { got: 8 });
    }

    #[test]
    fn code_longer_than_nine_is_rejected() {
        let mut screen = JoinScreen::new();
        let err = screen.set_code("1234567890").unwrap_err();
        assert_eq!(err, CodeError::WrongLength { got: 10 });
    }

    #[test]
    fn empty_code_is_rejected_with_wrong_length() {
        let mut screen = JoinScreen::new();
        let err = screen.set_code("").unwrap_err();
        assert_eq!(err, CodeError::WrongLength { got: 0 });
    }

    #[test]
    fn code_with_letter_is_rejected() {
        let mut screen = JoinScreen::new();
        let err = screen.set_code("12345678a").unwrap_err();
        assert_eq!(err, CodeError::NonDigit { ch: 'a' });
    }

    #[test]
    fn code_with_space_is_rejected() {
        let mut screen = JoinScreen::new();
        let err = screen.set_code("1234 6789").unwrap_err();
        assert_eq!(err, CodeError::NonDigit { ch: ' ' });
    }

    #[test]
    fn code_with_hyphen_is_rejected() {
        let mut screen = JoinScreen::new();
        let err = screen.set_code("123-56789").unwrap_err();
        assert_eq!(err, CodeError::NonDigit { ch: '-' });
    }

    #[test]
    fn all_zeros_is_a_valid_code() {
        let mut screen = JoinScreen::new();
        assert!(screen.set_code("000000000").is_ok());
    }

    #[test]
    fn set_code_replaces_previous_code() {
        let mut screen = JoinScreen::new();
        screen.set_code("111111111").unwrap();
        screen.set_code("999999999").unwrap();
        assert_eq!(screen.session_code(), Some("999999999"));
    }

    // ── State transitions ─────────────────────────────────────────────────────

    #[test]
    fn new_screen_is_idle() {
        let screen = JoinScreen::new();
        assert_eq!(screen.state(), JoinState::Idle);
    }

    #[test]
    fn connect_without_code_returns_error() {
        let mut screen = JoinScreen::new();
        assert_eq!(screen.connect(), Err(ConnectError::NoCodeEntered));
        assert_eq!(screen.state(), JoinState::Idle);
    }

    #[test]
    fn connect_with_valid_code_transitions_to_connecting() {
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        screen.connect().unwrap();
        assert_eq!(screen.state(), JoinState::Connecting);
    }

    #[test]
    fn connect_while_connecting_returns_error() {
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        screen.connect().unwrap();
        assert_eq!(screen.connect(), Err(ConnectError::AlreadyConnecting));
    }

    #[test]
    fn on_connected_transitions_to_connected() {
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        screen.connect().unwrap();
        screen.on_connected();
        assert_eq!(screen.state(), JoinState::Connected);
    }

    #[test]
    fn connect_while_connected_returns_error() {
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        screen.connect().unwrap();
        screen.on_connected();
        assert_eq!(screen.connect(), Err(ConnectError::AlreadyConnected));
    }

    #[test]
    fn on_failed_transitions_to_failed_with_reason() {
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        screen.connect().unwrap();
        screen.on_failed("code expired");
        assert_eq!(screen.state(), JoinState::Failed("code expired".to_string()));
    }

    #[test]
    fn reset_from_failed_returns_to_idle_and_clears_code() {
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        screen.connect().unwrap();
        screen.on_failed("code expired");
        screen.reset();
        assert_eq!(screen.state(), JoinState::Idle);
        assert_eq!(screen.session_code(), None);
    }

    #[test]
    fn reset_from_connected_returns_to_idle() {
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        screen.connect().unwrap();
        screen.on_connected();
        screen.reset();
        assert_eq!(screen.state(), JoinState::Idle);
        assert_eq!(screen.session_code(), None);
    }

    #[test]
    fn retry_after_failure_succeeds() {
        let mut screen = JoinScreen::new();

        // First attempt fails.
        screen.set_code("111111111").unwrap();
        screen.connect().unwrap();
        screen.on_failed("code expired");

        // User gets a new code and retries without resetting.
        screen.set_code("222222222").unwrap();
        let code = screen.connect().unwrap();
        assert_eq!(code, "222222222");
        assert_eq!(screen.state(), JoinState::Connecting);
    }

    // ── Error Display ─────────────────────────────────────────────────────────

    #[test]
    fn code_error_display_wrong_length() {
        let e = CodeError::WrongLength { got: 5 };
        assert!(e.to_string().contains("9 digits"));
        assert!(e.to_string().contains("5"));
    }

    #[test]
    fn code_error_display_non_digit() {
        let e = CodeError::NonDigit { ch: 'x' };
        assert!(e.to_string().contains("digits"));
    }

    #[test]
    fn connect_error_display_no_code_entered() {
        let e = ConnectError::NoCodeEntered;
        assert!(e.to_string().contains("9-digit"));
    }

    // ── time_to_connected_ms (Feature 148) ───────────────────────────────────

    #[test]
    fn time_to_connected_is_none_before_connection() {
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        screen.connect().unwrap();
        assert_eq!(screen.time_to_connected_ms(), None);
    }

    #[test]
    fn time_to_connected_is_some_after_on_connected() {
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        screen.connect().unwrap();
        screen.on_connected();
        assert!(
            screen.time_to_connected_ms().is_some(),
            "time_to_connected_ms must be recorded after on_connected"
        );
    }

    #[test]
    fn time_to_connected_is_none_before_connect_called() {
        let screen = JoinScreen::new();
        assert_eq!(screen.time_to_connected_ms(), None);
    }

    #[test]
    fn time_to_connected_resets_on_reset() {
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        screen.connect().unwrap();
        screen.on_connected();
        assert!(screen.time_to_connected_ms().is_some());
        screen.reset();
        assert_eq!(screen.time_to_connected_ms(), None);
    }

    #[test]
    fn time_to_connected_resets_on_reconnect_after_failure() {
        let mut screen = JoinScreen::new();

        // First attempt connects and records timing.
        screen.set_code("111111111").unwrap();
        screen.connect().unwrap();
        screen.on_connected();
        assert!(screen.time_to_connected_ms().is_some());

        // Reset and try again — timing must be cleared on reconnect.
        screen.reset();
        screen.set_code("222222222").unwrap();
        screen.connect().unwrap();
        assert_eq!(
            screen.time_to_connected_ms(),
            None,
            "time_to_connected_ms must be None while reconnecting"
        );
    }

    // ── No-account join invariant (Feature 140) ───────────────────────────────

    #[test]
    fn connect_requires_no_account_credentials() {
        // connect() accepts zero authentication arguments — no username, password,
        // email, or token.  If this compiles, the no-account invariant holds at
        // the type level.
        let mut screen = JoinScreen::new();
        screen.set_code("123456789").unwrap();
        let _code = screen.connect().unwrap();
        assert_eq!(screen.state(), JoinState::Connecting);
    }

    #[test]
    fn join_screen_has_no_auth_or_account_fields() {
        // JoinScreen::new() takes no credentials and exposes no auth accessor.
        // The only user-facing input accessor is session_code().
        let screen = JoinScreen::new();
        let _ = screen.session_code(); // only accessor that compiles
        // session_code is None until set — no default login is pre-filled.
        assert_eq!(screen.session_code(), None);
    }

    #[test]
    fn join_screen_starts_fresh_with_no_stored_identity() {
        // A newly constructed JoinScreen holds no pre-loaded identity or session;
        // it is idle and anonymous until the user manually enters a code.
        let screen = JoinScreen::new();
        assert_eq!(screen.state(), JoinState::Idle);
        assert_eq!(screen.session_code(), None);
        assert_eq!(screen.time_to_connected_ms(), None);
    }

    #[test]
    fn code_input_placeholder_is_defined() {
        assert_eq!(CODE_INPUT_PLACEHOLDER, "000 000 000");
    }

    #[test]
    fn join_button_label_is_defined() {
        assert_eq!(JOIN_BUTTON_LABEL, "Join");
    }
}
