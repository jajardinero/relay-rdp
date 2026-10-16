//! Turns protocol, authentication and network errors into something a person can act on.

use std::error::Error as StdError;
use std::fmt::Display;
use std::io;

use ironrdp_connector::sspi::credssp::NStatusCode;
use ironrdp_connector::{ConnectorError, ConnectorErrorKind};
use ironrdp_pdu::nego::FailureCode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Transient; worth retrying automatically.
    Network,
    Credentials,
    Security,
    Server,
    Internal,
}

#[derive(Debug, Clone)]
pub struct Failure {
    pub kind: FailureKind,
    pub title: String,
    pub hint: Option<String>,
    /// Technical detail for the "details" disclosure and the log.
    pub detail: String,
}

impl Failure {
    fn new(
        kind: FailureKind,
        title: impl Into<String>,
        hint: Option<&str>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            title: title.into(),
            hint: hint.map(str::to_owned),
            detail: detail.into(),
        }
    }

    pub fn internal(e: impl Display) -> Self {
        Self::new(
            FailureKind::Internal,
            "Something went wrong inside Relay",
            Some("Try connecting again. If it keeps happening, the log file has the details."),
            e.to_string(),
        )
    }

    pub fn untrusted_certificate() -> Self {
        Self::new(
            FailureKind::Security,
            "You didn't trust the computer's certificate",
            Some("Connect again if you want to review it."),
            "certificate rejected by user",
        )
    }

    /// `legacy`: the host already allows legacy TLS.
    pub fn tls(e: &io::Error, legacy: bool) -> Self {
        let hint = if legacy {
            "The computer must support TLS 1.0 or newer and a compatible encryption suite."
        } else {
            "The computer must support TLS 1.2 or newer and a compatible encryption suite. \
             For older Windows computers that only offer TLS 1.0, turn on Allow legacy TLS in \
             the host's Advanced settings."
        };
        Self::new(
            FailureKind::Security,
            "Couldn't establish a secure connection",
            Some(hint),
            error_chain(e),
        )
    }

    pub fn dns(host: &str, e: &io::Error) -> Self {
        Self::new(
            FailureKind::Network,
            format!("Couldn't find a computer named {host}"),
            Some("Check the spelling, or use the IP address instead."),
            e.to_string(),
        )
    }

    pub fn timeout(port: u16) -> Self {
        Self::new(
            FailureKind::Network,
            "The computer didn't respond",
            Some(&format!(
                "Check that it is turned on and that a firewall isn't blocking port {port}."
            )),
            "connection timed out",
        )
    }

    pub fn is_network(&self) -> bool {
        self.kind == FailureKind::Network
    }

    pub fn from_io(e: &io::Error, port: u16) -> Self {
        use io::ErrorKind as K;
        let (title, hint): (&str, String) = match e.kind() {
            K::ConnectionRefused => (
                "The computer refused the connection",
                format!(
                    "Make sure Remote Desktop is turned on there and that port {port} is open."
                ),
            ),
            K::TimedOut => return Self::timeout(port),
            K::HostUnreachable | K::NetworkUnreachable | K::NetworkDown => (
                "Can't reach that network",
                "Check your network connection or VPN.".to_owned(),
            ),
            K::ConnectionReset
            | K::ConnectionAborted
            | K::UnexpectedEof
            | K::BrokenPipe
            | K::NotConnected => (
                "The connection was interrupted",
                "The network dropped or the computer closed the connection.".to_owned(),
            ),
            _ => (
                "A network error occurred",
                "Check your connection and try again.".to_owned(),
            ),
        };
        Self::new(FailureKind::Network, title, Some(&hint), e.to_string())
    }

    pub fn from_connector(e: &ConnectorError, port: u16) -> Self {
        let detail = error_chain(e);

        if let Some(io) = find_io_error(e) {
            let mut failure = Self::from_io(io, port);
            failure.detail = detail;
            return failure;
        }

        match e.kind() {
            ConnectorErrorKind::Credssp(sspi) => credssp_failure(sspi, detail),
            ConnectorErrorKind::AccessDenied => Self::new(
                FailureKind::Credentials,
                "This account isn't allowed to connect",
                Some("Ask an administrator to add it to the Remote Desktop Users group."),
                detail,
            ),
            ConnectorErrorKind::Negotiation(failure) => negotiation_failure(failure.code(), detail),
            ConnectorErrorKind::Reason(reason) => Self::new(
                FailureKind::Server,
                "The computer ended the connection",
                Some(reason.as_str()),
                detail,
            ),
            _ => Self::new(
                FailureKind::Server,
                "The connection couldn't be set up",
                Some(
                    "The computer sent something unexpected. Try again, or switch Graphics to Compatible.",
                ),
                detail,
            ),
        }
    }

    pub fn from_session(e: &ironrdp_session::SessionError, port: u16) -> Self {
        let detail = error_chain(e);
        if let Some(io) = find_io_error(e) {
            let mut failure = Self::from_io(io, port);
            failure.detail = detail;
            return failure;
        }
        Self::new(
            FailureKind::Server,
            "The session stopped unexpectedly",
            Some("Reconnect to continue where you left off."),
            detail,
        )
    }
}

fn credssp_failure(e: &ironrdp_connector::sspi::Error, detail: String) -> Failure {
    use ironrdp_connector::sspi::ErrorKind;

    const WRONG_CREDENTIALS: &str = "The user name or password is incorrect";
    const ACCOUNT_HINT: &str = "Domain accounts look like CORP\\jdoe or jdoe@corp.example.com.";

    if let Some(status) = e.nstatus {
        let known = match status {
            NStatusCode::LOGON_FAILURE | NStatusCode::WRONG_PASSWORD => {
                Some((WRONG_CREDENTIALS, ACCOUNT_HINT))
            }
            NStatusCode::ACCOUNT_LOCKED_OUT => Some((
                "This account is locked",
                "Too many failed sign-ins. Wait, or ask an administrator to unlock it.",
            )),
            NStatusCode::PASSWORD_EXPIRED | NStatusCode::PASSWORD_MUST_CHANGE => Some((
                "The password has expired",
                "Change it by signing in to a domain computer, then connect again.",
            )),
            NStatusCode::ACCOUNT_DISABLED => Some((
                "This account is disabled",
                "Ask an administrator to enable it.",
            )),
            NStatusCode::ACCOUNT_RESTRICTION => Some((
                "This account can't sign in right now",
                "It may be outside its allowed hours, or need a non-empty password.",
            )),
            NStatusCode::LOGON_TYPE_NOT_GRANTED => Some((
                "This account isn't allowed to use Remote Desktop",
                "Ask an administrator to add it to the Remote Desktop Users group.",
            )),
            _ => None,
        };
        if let Some((title, hint)) = known {
            return Failure::new(FailureKind::Credentials, title, Some(hint), detail);
        }
    }

    match e.error_type {
        ErrorKind::LogonDenied => Failure::new(
            FailureKind::Credentials,
            WRONG_CREDENTIALS,
            Some(ACCOUNT_HINT),
            detail,
        ),
        ErrorKind::TimeSkew => Failure::new(
            FailureKind::Security,
            "This computer's clock is too far off",
            Some("Sync the time on both computers and try again."),
            detail,
        ),
        _ => Failure::new(
            FailureKind::Credentials,
            "Sign-in failed",
            Some("Check the user name, domain and password."),
            detail,
        ),
    }
}

fn negotiation_failure(code: FailureCode, detail: String) -> Failure {
    let (title, hint) = match code {
        FailureCode::SSL_NOT_ALLOWED_BY_SERVER => (
            "The computer only offers legacy RDP security",
            "Enable TLS or Network Level Authentication for Remote Desktop on that computer.",
        ),
        FailureCode::SSL_CERT_NOT_ON_SERVER => (
            "The computer has no Remote Desktop certificate",
            "Ask an administrator to check its Remote Desktop configuration.",
        ),
        FailureCode::SSL_WITH_USER_AUTH_REQUIRED_BY_SERVER => (
            "The computer requires a client certificate",
            "Client certificate sign-in isn't supported yet.",
        ),
        _ => (
            "The computer refused the security settings",
            "Its Remote Desktop security settings aren't compatible.",
        ),
    };
    Failure::new(FailureKind::Security, title, Some(hint), detail)
}

fn find_io_error<'a>(e: &'a (dyn StdError + 'static)) -> Option<&'a io::Error> {
    let mut current: Option<&(dyn StdError + 'static)> = Some(e);
    while let Some(err) = current {
        if let Some(io) = err.downcast_ref::<io::Error>() {
            return Some(io);
        }
        current = err.source();
    }
    None
}

fn error_chain(e: &(dyn StdError + 'static)) -> String {
    let mut out = e.to_string();
    let mut current = e.source();
    while let Some(err) = current {
        let text = err.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        current = err.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_errors_are_network_failures() {
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        let failure = Failure::from_io(&refused, 3389);
        assert!(failure.is_network());
        assert!(failure.hint.unwrap().contains("3389"));

        let reset = io::Error::from(io::ErrorKind::ConnectionReset);
        assert_eq!(
            Failure::from_io(&reset, 3389).title,
            "The connection was interrupted"
        );
    }

    #[test]
    fn wrapped_io_errors_are_found() {
        let e = ironrdp_connector::custom_err!(
            "TCP connect",
            io::Error::from(io::ErrorKind::ConnectionRefused)
        );
        let failure = Failure::from_connector(&e, 3389);
        assert!(failure.is_network());
        assert_eq!(failure.title, "The computer refused the connection");
    }

    #[test]
    fn wrong_password_is_a_credentials_failure() {
        let sspi = ironrdp_connector::sspi::Error::new_with_nstatus(
            ironrdp_connector::sspi::ErrorKind::LogonDenied,
            "logon failure",
            NStatusCode::LOGON_FAILURE,
        );
        let e = ConnectorError::new("CredSSP", ConnectorErrorKind::Credssp(sspi));
        let failure = Failure::from_connector(&e, 3389);
        assert_eq!(failure.kind, FailureKind::Credentials);
        assert_eq!(failure.title, "The user name or password is incorrect");
    }
}
